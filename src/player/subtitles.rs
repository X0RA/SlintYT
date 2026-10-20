//! Caption metadata shares the account-scoped stream catalogue. Only the
//! selected track is loaded; signed URLs never enter Slint models or logs.
use libmpv2::Mpv;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Track {
    pub key: String,
    pub label: String,
    pub language: String,
    pub url: String,
}

#[derive(Default, Deserialize)]
pub struct Info {
    #[serde(default)]
    subtitles: Option<BTreeMap<String, Vec<Format>>>,
    #[serde(default)]
    automatic_captions: Option<BTreeMap<String, Vec<Format>>>,
}

#[derive(Deserialize)]
struct Format {
    url: Option<String>,
    #[serde(default)]
    ext: String,
    name: Option<String>,
}

fn valid_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.host_str().is_some_and(|host| {
                host == "youtube.com"
                    || host.ends_with(".youtube.com")
                    || host == "googlevideo.com"
                    || host.ends_with(".googlevideo.com")
            })
    })
}

pub fn tracks(info: &Info) -> Vec<Track> {
    let mut tracks = Vec::new();
    for (kind, languages) in [
        ("uploaded", &info.subtitles),
        ("automatic", &info.automatic_captions),
    ] {
        for (language, formats) in languages.iter().flatten() {
            if language == "live_chat" {
                continue;
            }
            // Prefer WebVTT; mpv also decodes SRT and TTML. JSON/live-chat
            // entries are metadata rather than a playable subtitle stream.
            let preferred = ["vtt", "srt", "ttml"].iter().find_map(|ext| {
                formats
                    .iter()
                    .find(|f| f.ext == *ext && f.url.as_deref().is_some_and(valid_url))
            });
            let Some(format) = preferred else {
                continue;
            };
            let name = format
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or(language);
            tracks.push(Track {
                key: format!("{kind}:{language}"),
                label: format!("{name} [{language}] · {kind}"),
                language: language.clone(),
                url: format.url.clone().unwrap(),
            });
        }
    }
    tracks
}

/// Download before handing the track to mpv so its command worker remains responsive.
pub async fn prepare(track: Track, cookies: &str) -> anyhow::Result<Track> {
    #[cfg(test)]
    if std::path::Path::new(&track.url).is_absolute() {
        return Ok(track);
    }
    anyhow::ensure!(valid_url(&track.url), "Unsupported caption URL");
    let url = reqwest::Url::parse(&track.url)?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut request = http
        .get(url.clone())
        .header("Referer", "https://www.youtube.com/")
        .header("User-Agent", "Mozilla/5.0");
    // Never forward YouTube credentials to the video CDN.
    if url
        .host_str()
        .is_some_and(|h| h == "youtube.com" || h.ends_with(".youtube.com"))
    {
        let value = super::resolve::cookie_header(cookies)?;
        if !value.is_empty() {
            let mut header = reqwest::header::HeaderValue::from_str(&value)?;
            header.set_sensitive(true);
            request = request.header(reqwest::header::COOKIE, header);
        }
    }
    let mut response = request.send().await?.error_for_status()?;
    const LIMIT: usize = 4 * 1024 * 1024;
    anyhow::ensure!(
        response.content_length().is_none_or(|n| n <= LIMIT as u64),
        "Captions too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(bytes.len() + chunk.len() <= LIMIT, "Captions too large");
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(bytes)?;
    anyhow::ensure!(
        !text.contains('\0') && (text.contains("-->") || text.contains("<tt")),
        "Empty or invalid captions"
    );
    Ok(Track {
        url: format!("memory://{text}"),
        ..track
    })
}

/// Called on the mpv command worker after FileLoaded, never on the UI thread.
pub fn select(engine: &Mpv, track: Option<&Track>) -> libmpv2::Result<()> {
    if let Some(track) = track {
        engine.command(
            "sub-add",
            &[&track.url, "cached", &track.label, &track.language],
        )?;
        engine.set_property("sub-visibility", true)
    } else {
        engine.set_property("sid", "no")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_keep_languages_and_kinds_and_choose_playable_formats() {
        let info: Info = serde_json::from_value(serde_json::json!({
            "subtitles": {
                "en": [
                    {"ext":"json3", "url":"https://www.youtube.com/api/timedtext?fmt=json3"},
                    {"ext":"ttml", "url":"https://www.youtube.com/api/timedtext?fmt=ttml"},
                    {"ext":"vtt", "name":"English", "url":"https://www.youtube.com/api/timedtext?fmt=vtt"}
                ],
                "fr": [{"ext":"srt", "url":"https://www.youtube.com/api/timedtext?lang=fr"}],
                "live_chat": [{"ext":"vtt", "url":"https://www.youtube.com/api/timedtext"}],
                "unsafe": [{"ext":"vtt", "url":"file:///secret"}],
                "wrong-host": [{"ext":"vtt", "url":"https://youtube.com.example/api/timedtext"}]
            },
            "automatic_captions": {
                "en": [{"ext":"vtt", "name":"English", "url":"https://www.youtube.com/api/timedtext?kind=asr"}],
                "de": [{"ext":"vtt", "name":"German", "url":"https://www.youtube.com/api/timedtext?tlang=de"}]
            }
        })).unwrap();
        let tracks = tracks(&info);
        assert_eq!(tracks.len(), 4);
        assert_eq!(tracks[0].key, "uploaded:en");
        assert!(tracks[0].url.ends_with("fmt=vtt"));
        assert!(tracks[0].label.contains("English"));
        assert!(tracks.iter().any(|t| t.key == "automatic:en"));
        assert!(tracks.iter().any(|t| t.key == "automatic:de"));
        assert!(tracks.iter().all(|t| !t.label.contains("https://")));
        assert!(super::tracks(&Info::default()).is_empty());
        let null: Info =
            serde_json::from_str(r#"{"subtitles":null,"automatic_captions":null}"#).unwrap();
        assert!(super::tracks(&null).is_empty());
    }
}
