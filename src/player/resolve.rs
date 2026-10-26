use crate::playback_settings::StreamSelection;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, ffi::OsStr, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use zeroize::Zeroizing;

#[derive(Clone, Deserialize)]
pub struct Media {
    pub(super) streams: Vec<String>,
    pub user_agent: String,
    pub referer: String,
    /// yt-dlp's codec string for the video stream, empty when unknown.
    #[serde(default)]
    pub vcodec: String,
}

impl Media {
    pub fn source(&self) -> Result<String> {
        if self.streams.is_empty() || self.streams.len() > 2 {
            bail!("YouTube did not return playable streams.");
        }
        for stream in &self.streams {
            let url = reqwest::Url::parse(stream)
                .map_err(|_| anyhow::anyhow!("YouTube returned an invalid stream."))?;
            if !matches!(url.scheme(), "https" | "http") {
                bail!("YouTube returned an unsupported stream.");
            }
        }
        if [&self.user_agent, &self.referer]
            .iter()
            .any(|s| s.contains(['\r', '\n', '\0']))
        {
            bail!("YouTube returned invalid playback headers.");
        }
        if self.streams.len() == 1 {
            return Ok(self.streams[0].clone());
        }
        // mpv's EDL joins separate DASH video/audio tracks on one timeline.
        // Byte-length escaping keeps punctuation in signed URLs literal.
        Ok(format!(
            "edl://!new_stream;!no_clip;%{}%{};!new_stream;!no_clip;%{}%{}",
            self.streams[0].len(),
            self.streams[0],
            self.streams[1].len(),
            self.streams[1]
        ))
    }
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// A normal lookup never probes media. Recovery is bounded to public URLs,
/// then a checked lookup if mpv rejects those too.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    #[default]
    Normal,
    Public,
    Verified,
}

impl Mode {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Normal => Some(Self::Public),
            Self::Public => Some(Self::Verified),
            Self::Verified => None,
        }
    }
}

pub(super) struct Lookup {
    pub media: Media,
    pub catalogue: Result<super::quality::Catalogue, String>,
}

/// macOS uses app-private playback tools; other platforms use PATH.
pub(super) async fn resolve(
    id: &str,
    cookies: &str,
    selection: &StreamSelection,
    mode: Mode,
) -> Result<Lookup> {
    #[cfg(target_os = "macos")]
    let paths = crate::macos_tools::installed_paths().await?;
    #[cfg(target_os = "macos")]
    let executable = paths.yt_dlp.as_os_str();
    #[cfg(not(target_os = "macos"))]
    let executable = OsStr::new("yt-dlp");
    resolve_with(
        executable,
        id,
        cookies,
        Duration::from_secs(90),
        selection,
        mode,
    )
    .await
}

/// A separate, unprobed menu lookup is only needed when the shared response
/// could not supply a catalogue. It never replaces the stream being played.
pub(super) async fn catalogue(
    id: &str,
    cookies: &str,
    mode: Mode,
) -> Result<super::quality::Catalogue> {
    #[cfg(target_os = "macos")]
    let paths = crate::macos_tools::installed_paths().await?;
    #[cfg(target_os = "macos")]
    let executable = paths.yt_dlp.as_os_str();
    #[cfg(not(target_os = "macos"))]
    let executable = OsStr::new("yt-dlp");
    catalogue_with(executable, id, cookies, mode, Duration::from_secs(90)).await
}

async fn catalogue_with(
    executable: &OsStr,
    id: &str,
    cookies: &str,
    mode: Mode,
    timeout: Duration,
) -> Result<super::quality::Catalogue> {
    let signed_in = mode != Mode::Public && !session_config(cookies)?.is_empty();
    tokio::time::timeout(timeout, async {
        let attempt = async |cookies: &str, timeout| {
            let output = extract(
                executable,
                id,
                cookies,
                false,
                timeout,
                &StreamSelection::default(),
            )
            .await?;
            super::quality::parse(&output)
        };
        match attempt(
            if signed_in { cookies } else { "" },
            if signed_in { timeout / 2 } else { timeout },
        )
        .await
        {
            Ok(catalogue) => Ok(catalogue),
            Err(_) if signed_in => attempt("", timeout).await,
            Err(error) => Err(error),
        }
    })
    .await
    .context("Finding video formats timed out. Refresh streams to retry.")?
}

async fn resolve_with(
    executable: &OsStr,
    id: &str,
    cookies: &str,
    timeout: Duration,
    selection: &StreamSelection,
    mode: Mode,
) -> Result<Lookup> {
    if !valid_id(id) {
        bail!("This video has an invalid YouTube ID.");
    }
    let signed_in = mode != Mode::Public && !session_config(cookies)?.is_empty();
    let check = mode == Mode::Verified;
    // Signed-in first. Public is a fallback, never an unconditional second
    // extraction. Both attempts share a deadline; cancellation kills the child.
    tokio::time::timeout(timeout, async {
        match run(
            executable,
            id,
            if signed_in { cookies } else { "" },
            if signed_in { timeout / 2 } else { timeout },
            selection,
            check,
        )
        .await
        {
            Ok(lookup) => Ok(lookup),
            Err(_) if signed_in => run(executable, id, "", timeout, selection, check).await,
            Err(error) => Err(error),
        }
    })
    .await
    .context("Finding video streams timed out. Please retry.")?
}

fn session_config(cookies: &str) -> Result<Zeroizing<String>> {
    let header = cookie_header(cookies)?;
    if header.is_empty() {
        return Ok(header);
    }
    let header = Zeroizing::new(format!("Cookie: {}", header.as_str()));
    // Secrets stay on yt-dlp's stdin, never in arguments or temporary files.
    let quoted =
        shlex::try_quote(&header).map_err(|_| anyhow::anyhow!("Invalid browser session."))?;
    Ok(Zeroizing::new(format!("--add-headers {quoted}\n")))
}

pub(super) fn cookie_header(cookies: &str) -> Result<Zeroizing<String>> {
    let mut values = BTreeMap::new();
    let now = crate::history::now() as i64;
    for line in cookies.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 7 {
            bail!("The browser session has invalid cookie data.");
        }
        let host = fields[0].strip_prefix('.').unwrap_or(fields[0]);
        if !matches!(host, "youtube.com" | "www.youtube.com") || fields[2] != "/" {
            continue;
        }
        let expires = fields[4]
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("The browser session has invalid cookie data."))?;
        if expires != 0 && expires <= now {
            continue;
        }
        let (name, value) = (fields[5], fields[6]);
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
            || !value
                .bytes()
                .all(|c| (0x21..=0x7e).contains(&c) && !b"\";,\\".contains(&c))
        {
            bail!("The browser session has invalid cookie data.");
        }
        // Prefer a host-specific cookie over a parent-domain cookie of the
        // same name. Only root-path YouTube cookies are supplied to yt-dlp.
        let priority = u8::from(host == "www.youtube.com");
        if values
            .get(name)
            .is_none_or(|(previous, _)| priority >= *previous)
        {
            values.insert(name, (priority, value));
        }
    }
    if values.is_empty() {
        return Ok(Zeroizing::new(String::new()));
    }
    Ok(Zeroizing::new(
        values
            .iter()
            .map(|(name, (_, value))| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

async fn run(
    executable: &OsStr,
    id: &str,
    cookies: &str,
    timeout: Duration,
    selection: &StreamSelection,
    check: bool,
) -> Result<Lookup> {
    let output = extract(executable, id, cookies, check, timeout, selection).await?;
    Ok(Lookup {
        media: parse_media(&output)?,
        // A malformed catalogue must not prevent an otherwise playable video.
        catalogue: super::quality::parse(&output).map_err(|error| error.to_string()),
    })
}

async fn extract(
    executable: &OsStr,
    id: &str,
    cookies: &str,
    check: bool,
    timeout: Duration,
    selection: &StreamSelection,
) -> Result<Zeroizing<Vec<u8>>> {
    if !valid_id(id) {
        bail!("This video has an invalid YouTube ID.");
    }
    let config = session_config(cookies)?;
    let mut command = Command::new(executable);
    #[cfg(target_os = "macos")]
    {
        // Only the app-managed executable needs the managed runtime. Test
        // executables still exercise the exact same stdin/argument handling.
        if std::path::Path::new(executable).file_name() == Some(OsStr::new("yt-dlp_macos")) {
            let tools = crate::macos_tools::installed_paths().await?;
            command
                .arg("--js-runtimes")
                .arg(format!("deno:{}", tools.deno.display()));
        }
    }
    if !selection.sort.is_empty() {
        command.arg("--format-sort").arg(&selection.sort);
    }
    let mut child = command
        .args([
            "--ignore-config",
            "--no-plugin-dirs",
            "--config-locations",
            "-",
            "--no-cache-dir",
            "--no-playlist",
            "--skip-download",
            "--no-warnings",
            if check {
                "--check-formats"
            } else {
                "--no-check-formats"
            },
            "--socket-timeout",
            "15",
            "--retries",
            "2",
            "--extractor-retries",
            "1",
            "--format",
            &selection.format,
            "--dump-single-json",
            "--",
        ])
        .arg(format!("https://www.youtube.com/watch?v={id}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            #[cfg(target_os = "macos")]
            return anyhow::anyhow!(
                "yt-dlp could not be started. Check for updates in Settings → yt-dlp."
            );
            #[cfg(not(target_os = "macos"))]
            anyhow::anyhow!(
                "yt-dlp could not be started. Install yt-dlp and make sure it is on PATH."
            )
        })?;
    let output = tokio::time::timeout(timeout, async move {
        let mut stdin = child.stdin.take().context("Could not prepare video lookup.")?;
        let stdout = child.stdout.take().context("Could not read video lookup.")?;
        let write = async move {
            stdin.write_all(config.as_bytes()).await?;
            drop(stdin);
            Ok::<_, anyhow::Error>(())
        };
        let read = async move {
            const LIMIT: u64 = 16 * 1024 * 1024;
            let mut bytes = Zeroizing::new(Vec::new());
            stdout.take(LIMIT + 1).read_to_end(&mut bytes).await?;
            if bytes.len() as u64 > LIMIT { bail!("Video lookup returned too much data."); }
            Ok::<_, anyhow::Error>(bytes)
        };
        let ((), bytes, status) = tokio::try_join!(write, read, async { child.wait().await.map_err(anyhow::Error::from) })?;
        if !status.success() { bail!("Video lookup failed."); }
        Ok::<_, anyhow::Error>(bytes)
    }).await.context("Finding video streams timed out. Please retry.")?
        .map_err(|_| anyhow::anyhow!("Could not find playable streams. The video may be unavailable or require a newer yt-dlp. Please retry."))?;
    Ok(output)
}

#[derive(Deserialize, Default)]
struct Headers {
    #[serde(rename = "User-Agent", default)]
    user_agent: String,
    #[serde(rename = "Referer", default)]
    referer: String,
}
#[derive(Deserialize)]
struct Format {
    url: String,
    #[serde(default)]
    http_headers: Headers,
    #[serde(default)]
    vcodec: Option<String>,
}
#[derive(Deserialize)]
struct Info {
    url: Option<String>,
    requested_formats: Option<Vec<Format>>,
    #[serde(default)]
    http_headers: Headers,
    #[serde(default)]
    vcodec: Option<String>,
}
fn parse_media(output: &[u8]) -> Result<Media> {
    let info: Info = serde_json::from_slice(output)
        .map_err(|_| anyhow::anyhow!("yt-dlp returned an invalid response."))?;
    let formats = info.requested_formats.unwrap_or_else(|| {
        info.url
            .map(|url| {
                vec![Format {
                    url,
                    http_headers: Headers::default(),
                    vcodec: info.vcodec.clone(),
                }]
            })
            .unwrap_or_default()
    });
    let vcodec = formats
        .iter()
        .filter_map(|f| f.vcodec.as_deref())
        .find(|c| !c.is_empty() && *c != "none")
        .unwrap_or_default()
        .to_owned();
    let first = formats
        .first()
        .context("YouTube did not return playable streams.")?;
    let user_agent = if info.http_headers.user_agent.is_empty() {
        first.http_headers.user_agent.clone()
    } else {
        info.http_headers.user_agent
    };
    let referer = if info.http_headers.referer.is_empty() {
        first.http_headers.referer.clone()
    } else {
        info.http_headers.referer
    };
    let media = Media {
        streams: formats.into_iter().map(|f| f.url).collect(),
        user_agent,
        referer: if referer.is_empty() {
            "https://www.youtube.com/".into()
        } else {
            referer
        },
        vcodec,
    };
    media.source()?;
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_is_supplied_as_one_quoted_stdin_option_with_youtube_scope() {
        let cookies = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic'quoted\n.example.com\tTRUE\t/\tTRUE\t0\tOTHER\tunrelated\n.youtube.com\tTRUE\t/\tTRUE\t1\tOLD\texpired\n.youtube.com\tTRUE\t/private\tTRUE\t0\tPRIVATE\tpath-specific\n";
        let config = session_config(cookies).unwrap();
        let args = shlex::split(&config).unwrap();
        assert_eq!(args, ["--add-headers", "Cookie: SAPISID=synthetic'quoted"]);
        assert!(!config.contains("unrelated"));
        assert!(!config.contains("expired"));
        assert!(!config.contains("path-specific"));
        assert!(session_config(".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tx;injected=y\n").is_err());
    }

    #[test]
    fn parses_cli_formats_without_forwarding_cookies_or_unsafe_headers() {
        let data = br#"{"requested_formats":[{"url":"https://cdn.example/video"},{"url":"https://cdn.example/audio"}],"http_headers":{"User-Agent":"Example","Cookie":"synthetic-secret"}}"#;
        let media = parse_media(data).unwrap();
        assert_eq!(media.streams.len(), 2);
        assert_eq!(media.user_agent, "Example");
        assert!(!media.source().unwrap().contains("synthetic-secret"));
        assert_eq!(media.vcodec, "");
        let picked = br#"{"requested_formats":[{"url":"https://cdn.example/video","vcodec":"vp09.00.40.08"},{"url":"https://cdn.example/audio","vcodec":"none"}]}"#;
        assert_eq!(parse_media(picked).unwrap().vcodec, "vp09.00.40.08");
        let muxed = br#"{"url":"https://cdn.example/muxed","vcodec":"avc1.42001E"}"#;
        assert_eq!(parse_media(muxed).unwrap().vcodec, "avc1.42001E");
        assert!(parse_media(br#"{"url":"file:///etc/passwd"}"#).is_err());
        assert!(
            parse_media(
                br#"{"url":"https://example.com","http_headers":{"User-Agent":"bad\r\nheader"}}"#
            )
            .is_err()
        );
        assert_eq!(
            parse_media(br#"{"url":"https://cdn.example/muxed"}"#)
                .unwrap()
                .streams
                .len(),
            1
        );
    }

    #[cfg(unix)]
    fn fake_executable(root: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("fake-yt-dlp");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    #[tokio::test]
    async fn executable_receives_no_secret_arguments_and_failures_are_sanitized() {
        let root = tempfile::tempdir().unwrap();
        let exe = fake_executable(
            root.path(),
            r#"
case "$*" in *synthetic-secret*|*python*|*--cookies*) exit 11;; esac
case "$*" in *--ignore-config*--no-plugin-dirs*--config-locations*) ;; *) exit 12;; esac
case "$*" in *--no-check-formats*) ;; *) exit 14;; esac
config=$(cat)
case "$config" in *'Cookie: SAPISID=synthetic-secret'*) ;; *) exit 13;; esac
printf '%s\n' '{"url":"https://cdn.example/video"}'
"#,
        );
        let cookies = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n";
        assert!(
            run(
                exe.as_os_str(),
                "abcdefghijk",
                cookies,
                Duration::from_secs(3),
                &StreamSelection::default(),
                false
            )
            .await
            .is_ok()
        );
        fake_executable(
            root.path(),
            "echo synthetic-secret >&2\necho synthetic-secret\nexit 1\n",
        );
        let result = run(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            false,
        )
        .await;
        assert!(
            !result
                .err()
                .unwrap()
                .to_string()
                .contains("synthetic-secret")
        );
        fake_executable(root.path(), "exec sleep 30\n");
        let result = run(
            exe.as_os_str(),
            "abcdefghijk",
            "",
            Duration::from_millis(100),
            &StreamSelection::default(),
            false,
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("timed out"));
        assert!(
            run(
                root.path().join("missing").as_os_str(),
                "abcdefghijk",
                "",
                Duration::from_secs(3),
                &StreamSelection::default(),
                false
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains(if cfg!(target_os = "macos") {
                "Settings"
            } else {
                "PATH"
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signed_in_lookup_is_reused_and_public_is_only_a_fallback() {
        let root = tempfile::tempdir().unwrap();
        let cookies = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n";
        let exe = fake_executable(
            root.path(),
            r#"
config=$(cat)
case "$config" in *'Cookie: SAPISID=synthetic-secret'*) ;; *) exit 21;; esac
[ ! -e "$0.success" ] || exit 24
touch "$0.success"
case "$*" in *--no-check-formats*) ;; *) exit 25;; esac
printf '%s\n' '{"url":"https://cdn.example/account","formats":[{"format_id":"v","url":"https://cdn.example/account","protocol":"https","vcodec":"av01","acodec":"aac","height":720}],"subtitles":{"en":[{"url":"https://www.youtube.com/api/timedtext?lang=en","ext":"vtt"}]}}'
"#,
        );
        let media = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            Mode::Normal,
        )
        .await
        .unwrap();
        assert_eq!(media.media.source().unwrap(), "https://cdn.example/account");
        let catalogue = media.catalogue.unwrap();
        assert_eq!(catalogue.options.len(), 1);
        assert_eq!(catalogue.subtitles.len(), 1);
        assert_eq!(
            catalogue.options[0]
                .media
                .as_ref()
                .unwrap()
                .source()
                .unwrap(),
            "https://cdn.example/account"
        );

        fake_executable(
            root.path(),
            r#"
config=$(cat)
if [ ! -e "$0.attempted" ]; then
    case "$config" in *'Cookie: SAPISID=synthetic-secret'*) ;; *) exit 22;; esac
    touch "$0.attempted"
    exit 1
fi
[ -z "$config" ] || exit 23
printf '%s\n' '{"url":"https://cdn.example/public"}'
"#,
        );
        let media = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            Mode::Normal,
        )
        .await
        .unwrap();
        assert_eq!(media.media.source().unwrap(), "https://cdn.example/public");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fallback_is_skipped_without_cookies_and_shares_the_lookup_deadline() {
        let root = tempfile::tempdir().unwrap();
        let exe = fake_executable(
            root.path(),
            r#"
cat >/dev/null
echo attempt >> "$0.attempts"
exit 1
"#,
        );
        assert!(
            resolve_with(
                exe.as_os_str(),
                "abcdefghijk",
                "",
                Duration::from_secs(3),
                &StreamSelection::default(),
                Mode::Normal
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(exe.with_extension("attempts")).unwrap(),
            "attempt\n"
        );
        fake_executable(root.path(), "cat >/dev/null\nexec sleep 30\n");
        let error = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n",
            Duration::from_millis(100),
            &StreamSelection::default(),
            Mode::Normal,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_uses_public_without_probes_then_checked_extraction_and_stops() {
        assert_eq!(Mode::Normal.next(), Some(Mode::Public));
        assert_eq!(Mode::Public.next(), Some(Mode::Verified));
        assert_eq!(Mode::Verified.next(), None);
        let root = tempfile::tempdir().unwrap();
        let cookies = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n";
        let exe = fake_executable(
            root.path(),
            r#"
config=$(cat)
[ -z "$config" ] || exit 21
case "$*" in *--no-check-formats*--skip-download*) exit 22;; esac
case "$*" in *--skip-download*--no-check-formats*) ;; *) exit 23;; esac
printf '%s\n' '{"url":"https://cdn.example/public"}'
"#,
        );
        let lookup = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            Mode::Public,
        )
        .await
        .unwrap();
        assert_eq!(lookup.media.source().unwrap(), "https://cdn.example/public");
        fake_executable(
            root.path(),
            r#"
config=$(cat)
case "$config" in *'Cookie: SAPISID=synthetic-secret'*) ;; *) exit 24;; esac
case "$*" in *--check-formats*) ;; *) exit 25;; esac
printf '%s\n' '{"url":"https://cdn.example/checked"}'
"#,
        );
        let lookup = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            Mode::Verified,
        )
        .await
        .unwrap();
        assert_eq!(
            lookup.media.source().unwrap(),
            "https://cdn.example/checked"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn menu_retry_is_independent_of_stream_selection_and_can_fall_back_to_public() {
        let root = tempfile::tempdir().unwrap();
        let exe = fake_executable(
            root.path(),
            r#"
config=$(cat)
case "$*" in *--no-check-formats*) ;; *) exit 1;; esac
if [ -n "$config" ]; then
    printf '%s\n' '{"url":"https://cdn.example/playable","formats":"invalid"}'
else
    printf '%s\n' '{"formats":[{"format_id":"18","url":"https://cdn.example/muxed","protocol":"https","vcodec":"avc1","acodec":"aac"}],"subtitles":{"en":[{"url":"https://www.youtube.com/api/timedtext?lang=en","ext":"vtt"}]}}'
fi
"#,
        );
        let cookies = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n";
        let lookup = resolve_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Duration::from_secs(3),
            &StreamSelection::default(),
            Mode::Normal,
        )
        .await
        .unwrap();
        assert!(lookup.catalogue.is_err());
        assert_eq!(
            lookup.media.source().unwrap(),
            "https://cdn.example/playable"
        );
        let menu = catalogue_with(
            exe.as_os_str(),
            "abcdefghijk",
            cookies,
            Mode::Normal,
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(menu.options.len(), 1);
        assert_eq!(menu.subtitles.len(), 1);
        // This response has no top-level selected stream: menu parsing does not
        // depend on the playback selection being available in the new lookup.
        assert!(menu.options[0].key.starts_with("18|"));
    }

    #[tokio::test]
    #[ignore = "requires explicit SLINTYT_TEST_PROFILE authorization, yt-dlp and network access"]
    async fn live_native_cookie_import_and_cli_stream_lookup() {
        let selection = crate::browser::Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("Set an authorized test profile")
                .into(),
            keyring: None,
        };
        let cookies = Zeroizing::new(
            crate::cookies::read(&selection)
                .await
                .expect("Native cookie import failed"),
        );
        let id = std::env::var("SLINTYT_TEST_VIDEO").unwrap_or_else(|_| "dQw4w9WgXcQ".into());
        let media = resolve(
            &id,
            &cookies,
            &crate::playback_settings::Preferences::default().selection(),
            Mode::Normal,
        )
        .await
        .expect("CLI lookup failed");
        assert!(media.media.source().is_ok());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        for (index, stream) in media.media.streams.iter().enumerate() {
            let response = client
                .get(stream)
                .header("User-Agent", &media.media.user_agent)
                .header("Referer", &media.media.referer)
                .header("Range", "bytes=0-1023")
                .send()
                .await
                .unwrap_or_else(|_| panic!("Stream {} request failed", index + 1));
            let status = response.status();
            eprintln!("Stream {} HTTP status: {status}", index + 1);
            assert!(
                status.is_success(),
                "Stream {} is not playable: {status}",
                index + 1
            );
        }
        eprintln!(
            "Native import and yt-dlp lookup passed; {} stream(s), all session values withheld",
            media.media.streams.len()
        );
    }

    #[test]
    fn validates_video_ids_and_escapes_separate_streams() {
        assert!(valid_id("aB_12-xyZ09"));
        for id in [
            "",
            "https://youtube.com/watch?v=abcdefghijk",
            "../abcdefgh",
            "abcdefghijé",
        ] {
            assert!(!valid_id(id));
        }
        let media = Media {
            streams: vec![
                "https://cdn.example/video?a=1;b=2".into(),
                "https://cdn.example/audio?q=é".into(),
            ],
            user_agent: String::new(),
            referer: String::new(),
            vcodec: String::new(),
        };
        assert_eq!(
            media.source().unwrap(),
            format!(
                "edl://!new_stream;!no_clip;%{}%{};!new_stream;!no_clip;%{}%{}",
                media.streams[0].len(),
                media.streams[0],
                media.streams[1].len(),
                media.streams[1]
            )
        );
        let invalid = Media {
            streams: vec!["file:///etc/passwd".into()],
            ..media
        };
        assert!(invalid.source().is_err());
    }
}
