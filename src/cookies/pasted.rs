//! Cookies pasted by hand: a cookies.txt export, a JSON export (Cookie-Editor
//! and similar), or a DevTools `Cookie:` request header. Errors never quote
//! cookie values.
use super::{Cookie, netscape, youtube_host};
use anyhow::{Result, bail};
use zeroize::Zeroizing;

const SIGNED_OUT: &str = "These cookies aren't signed in to YouTube. Export them while signed in to youtube.com, then paste them again.";

/// Normalises pasted cookies into the Netscape text the rest of the app reads.
pub fn parse(text: &str) -> Result<String> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        bail!("Paste your exported YouTube cookies first.");
    }
    let cookies = if text.starts_with('[') || text.starts_with('{') {
        json(text)?
    } else if text.contains('\t') || text.starts_with('#') {
        cookies_txt(text)?
    } else {
        header(text)?
    };
    let now = crate::history::now() as i64;
    let cookies: Vec<_> = cookies
        .into_iter()
        .filter(|c| youtube_host(&c.host) && (c.expires == 0 || c.expires > now))
        .collect();
    if !cookies
        .iter()
        .any(|c| matches!(c.name.as_str(), "SAPISID" | "__Secure-3PAPISID"))
    {
        bail!(SIGNED_OUT);
    }
    netscape(cookies, now)
}

fn cookies_txt(text: &str) -> Result<Vec<Cookie>> {
    let mut cookies = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        // Exporters mark HttpOnly cookies with a prefix that looks like a comment.
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [host, _, path, secure, expires, name, value] = fields[..] else {
            bail!(
                "Line {} isn't a cookies.txt line. Paste the whole exported file.",
                number + 1
            );
        };
        cookies.push(cookie(
            host,
            name,
            value,
            path,
            expiry(expires)?,
            secure == "TRUE",
        )?);
    }
    Ok(cookies)
}

fn json(text: &str) -> Result<Vec<Cookie>> {
    const INVALID: &str =
        "This JSON isn't a cookie export. Paste the cookies.txt or JSON export for youtube.com.";
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!(INVALID))?;
    let list = match &value {
        serde_json::Value::Array(list) => list,
        serde_json::Value::Object(map) => match map.get("cookies") {
            Some(serde_json::Value::Array(list)) => list,
            _ => bail!(INVALID),
        },
        _ => bail!(INVALID),
    };
    fn field<'a>(item: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        item.get(key).and_then(|v| v.as_str())
    }
    list.iter()
        .map(|item| {
            let (Some(host), Some(name), Some(value)) = (
                field(item, "domain"),
                field(item, "name"),
                field(item, "value"),
            ) else {
                bail!(INVALID);
            };
            let session = item.get("session").and_then(|v| v.as_bool()) == Some(true);
            let expires = ["expirationDate", "expires", "expiry"]
                .iter()
                .find_map(|key| item.get(*key).and_then(|v| v.as_f64()))
                .filter(|_| !session)
                .map_or(0, |v| v as i64);
            cookie(
                host,
                name,
                value,
                field(item, "path").unwrap_or("/"),
                expires,
                item.get("secure").and_then(|v| v.as_bool()).unwrap_or(true),
            )
        })
        .collect()
}

/// A request header has no attributes, so every pair is a youtube.com cookie.
fn header(text: &str) -> Result<Vec<Cookie>> {
    let text = match text.split_once(':') {
        Some((name, rest)) if name.trim().eq_ignore_ascii_case("cookie") => rest,
        _ => text,
    };
    let text = text.trim();
    if text.contains(['\n', '\r']) {
        bail!("Paste a cookies.txt file, a JSON export or one Cookie header line.");
    }
    text.split(';')
        .filter(|pair| !pair.trim().is_empty())
        .map(|pair| {
            let Some((name, value)) = pair.trim().split_once('=') else {
                bail!("This doesn't look like cookies. Paste a cookies.txt file, a JSON export or the Cookie header.");
            };
            cookie(".youtube.com", name.trim(), value.trim(), "/", 0, true)
        })
        .collect()
}

fn expiry(field: &str) -> Result<i64> {
    field
        .parse::<i64>()
        .or_else(|_| field.parse::<f64>().map(|v| v as i64))
        .map_err(|_| anyhow::anyhow!("A cookie has an invalid expiry date."))
}

fn cookie(
    host: &str,
    name: &str,
    value: &str,
    path: &str,
    expires: i64,
    secure: bool,
) -> Result<Cookie> {
    if name.is_empty()
        || [host, name, value, path]
            .iter()
            .any(|s| s.chars().any(char::is_control))
    {
        bail!("The pasted cookies contain invalid characters.");
    }
    Ok(Cookie {
        host: host.into(),
        name: name.into(),
        value: Zeroizing::new(value.into()),
        path: path.into(),
        expires,
        secure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(parsed: &str) -> Vec<String> {
        parsed.lines().skip(1).map(str::to_owned).collect()
    }

    #[test]
    fn cookies_txt_keeps_youtube_and_httponly_rows() {
        let text = "# Netscape HTTP Cookie File\n\
            # comment\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsecret-a\n\
            #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t4102444800\tSID\tsecret-b\r\n\
            .google.com\tTRUE\t/\tTRUE\t0\tSID\tgoogle\n\
            .youtube.com\tTRUE\t/\tTRUE\t1\tOLD\texpired\n\
            \n";
        let parsed = parse(text).unwrap();
        assert_eq!(
            lines(&parsed),
            [
                ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsecret-a",
                ".youtube.com\tTRUE\t/\tTRUE\t4102444800\tSID\tsecret-b"
            ]
        );
    }

    #[test]
    fn json_exports_are_accepted_in_both_shapes() {
        let list = r#"[
            {"domain":".youtube.com","name":"__Secure-3PAPISID","value":"secret","path":"/","expirationDate":4102444800.5,"secure":true},
            {"domain":"www.youtube.com","name":"PREF","value":"f6=40","session":true,"expirationDate":4102444800},
            {"domain":".google.com","name":"NID","value":"x"}
        ]"#;
        let parsed = parse(list).unwrap();
        assert_eq!(
            lines(&parsed),
            [
                ".youtube.com\tTRUE\t/\tTRUE\t4102444800\t__Secure-3PAPISID\tsecret",
                "www.youtube.com\tFALSE\t/\tTRUE\t0\tPREF\tf6=40"
            ]
        );
        let wrapped = format!(r#"{{"cookies":{list}}}"#);
        assert_eq!(parse(&wrapped).unwrap(), parsed);
    }

    #[test]
    fn devtools_headers_become_youtube_session_cookies() {
        for text in [
            "Cookie: SAPISID=abc; PREF=f6=40; ",
            "SAPISID=abc;PREF=f6=40",
            "cookie:SAPISID=abc; PREF=f6=40",
        ] {
            assert_eq!(
                lines(&parse(text).unwrap()),
                [
                    ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tabc",
                    ".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tf6=40"
                ]
            );
        }
    }

    #[test]
    fn signed_out_or_invalid_pastes_fail_without_quoting_values() {
        for text in [
            "",
            "   ",
            "PREF=synthetic-secret",
            ".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tsynthetic-secret",
            ".youtube.com\tTRUE\t/\tsynthetic-secret",
            ".google.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret",
            "[{\"domain\":\".youtube.com\",\"name\":\"SAPISID\"}]",
            "{\"synthetic-secret\":1}",
            "not json synthetic-secret",
            "SAPISID=a\nPREF=synthetic-secret",
            ".youtube.com\tTRUE\t/\tTRUE\tsoon\tSAPISID\tsynthetic-secret",
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(!error.contains("synthetic-secret"), "{error}");
        }
        assert_eq!(
            parse(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tx")
                .unwrap_err()
                .to_string(),
            SIGNED_OUT
        );
    }
}
