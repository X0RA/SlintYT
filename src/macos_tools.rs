//! App-private playback tools. This module is compiled only on macOS.
use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use slint::ComponentHandle;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};

use crate::{AppState, AppWindow};

static UPDATE: Mutex<()> = Mutex::const_new(());
const MAX_DOWNLOAD: usize = 200 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Tool {
    YtDlp,
    Deno,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Deno => "deno",
        }
    }

    fn repository(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp/yt-dlp",
            Self::Deno => "denoland/deno",
        }
    }

    fn asset(self) -> Result<&'static str> {
        match self {
            Self::YtDlp => Ok("yt-dlp_macos"),
            Self::Deno => match std::env::consts::ARCH {
                "aarch64" => Ok("deno-aarch64-apple-darwin.zip"),
                "x86_64" => Ok("deno-x86_64-apple-darwin.zip"),
                _ => bail!("This Mac's architecture is not supported by Deno."),
            },
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn url(&self, repository: &str, name: &str) -> Result<&str> {
        let asset = self
            .assets
            .iter()
            .find(|a| a.name == name)
            .context("The release does not include the required macOS download.")?;
        // Follow redirects only after selecting an official release asset.
        let prefix = format!("https://github.com/{repository}/releases/download/");
        if !asset.browser_download_url.starts_with(&prefix) {
            bail!("The release returned an unexpected download location.");
        }
        Ok(&asset.browser_download_url)
    }
}

fn root() -> Result<PathBuf> {
    Ok(BaseDirs::new()
        .context("Could not locate Application Support.")?
        .home_dir()
        .join("Library/Application Support/SlintYT/tools"))
}

/// Existing tools remain usable during checks; first use waits for installation.
pub async fn installed_paths() -> Result<(PathBuf, PathBuf)> {
    let root = root()?;
    let yt_dlp = root.join("yt-dlp");
    let deno = root.join("deno");
    if yt_dlp.is_file() && deno.is_file() {
        return Ok((yt_dlp, deno));
    }
    let _guard = UPDATE.lock().await;
    if !yt_dlp.is_file() || !deno.is_file() {
        bail!("Playback tools are not ready. Check for updates in Settings → yt-dlp, then retry.");
    }
    Ok((yt_dlp, deno))
}

fn publish(
    weak: &slint::Weak<AppWindow>,
    busy: bool,
    status: String,
    error: bool,
    versions: Option<String>,
) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let state = app.global::<AppState>();
        state.set_tools_busy(busy);
        state.set_tools_status(status.into());
        state.set_tools_error(error);
        if let Some(versions) = versions {
            state.set_tools_versions(versions.into());
        }
    });
}

pub fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let state = app.global::<AppState>();
    state.set_managed_tools(true);
    state.set_tools_busy(true);
    state.set_tools_status("Checking for updates…".into());
    let weak = app.as_weak();
    let handle = runtime.clone();
    state.on_check_tool_updates(move || {
        let Some(app) = weak.upgrade() else { return };
        let state = app.global::<AppState>();
        if state.get_tools_busy() {
            return;
        }
        state.set_tools_busy(true);
        state.set_tools_error(false);
        state.set_tools_status("Checking for updates…".into());
        handle.spawn(check(weak.clone()));
    });
    runtime.spawn(check(app.as_weak()));
}

async fn check(weak: slint::Weak<AppWindow>) {
    let _guard = UPDATE.lock().await;
    let result = update(&weak).await;
    let versions = match root() {
        Ok(root) => Some(installed_versions(&root).await),
        Err(_) => None,
    };
    match result {
        Ok(changed) => publish(
            &weak,
            false,
            if changed {
                "Playback tools updated."
            } else {
                "Everything is up to date."
            }
            .into(),
            false,
            versions,
        ),
        Err(error) => publish(
            &weak,
            false,
            format!("{error} Installed tools are kept; you can try again."),
            true,
            versions,
        ),
    }
}

async fn version(path: &Path, tool: Tool) -> Option<String> {
    let mut command = Command::new(path);
    command.arg("--version").stdout(Stdio::piped());
    let output = run_command(command, Duration::from_secs(15)).await.ok()?;
    parse_version(&output, tool)
}

async fn run_command(mut command: Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take();
    let result = tokio::time::timeout(timeout, async {
        let read = async {
            let mut output = Vec::new();
            if let Some(stdout) = stdout {
                stdout.take(4097).read_to_end(&mut output).await?;
                if output.len() > 4096 {
                    bail!("The playback tool returned too much version information.");
                }
            }
            Ok::<_, anyhow::Error>(output)
        };
        let (output, status) =
            tokio::try_join!(read, async { Ok::<_, anyhow::Error>(child.wait().await?) })?;
        if !status.success() {
            bail!("The playback tool command failed.");
        }
        Ok(output)
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        error => {
            // Reap timed-out/failed children immediately. Dropping this future
            // during shutdown also kills the child via kill_on_drop.
            let _ = child.kill().await;
            match error {
                Ok(Err(error)) => Err(error),
                _ => bail!("The playback tool command timed out."),
            }
        }
    }
}

fn parse_version(output: &[u8], tool: Tool) -> Option<String> {
    let line = std::str::from_utf8(output).ok()?.lines().next()?;
    let value = match tool {
        Tool::YtDlp => line.trim(),
        Tool::Deno => line.strip_prefix("deno ")?.split_whitespace().next()?,
    };
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|c| c.is_ascii_digit() || c == b'.')
    {
        return None;
    }
    Some(value.into())
}

async fn installed_versions(root: &Path) -> String {
    let yt = version(&root.join("yt-dlp"), Tool::YtDlp)
        .await
        .unwrap_or_else(|| "Not installed".into());
    let deno = version(&root.join("deno"), Tool::Deno)
        .await
        .unwrap_or_else(|| "Not installed".into());
    format!("yt-dlp {yt} · Deno {deno}")
}

async fn fetch(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let timeout = if limit <= 2 * 1024 * 1024 { 20 } else { 180 };
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(timeout))
        .send()
        .await?
        .error_for_status()?;
    if response.content_length().is_some_and(|n| n > limit as u64) {
        bail!("The download is larger than expected.");
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(data.len()) {
            bail!("The download is larger than expected.");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn checksum(contents: &[u8], asset: &str) -> Result<String> {
    std::str::from_utf8(contents)?
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let file = fields.next()?.trim_start_matches('*');
            (file == asset && hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
                .then(|| hash.to_ascii_lowercase())
        })
        .context("The download's checksum is missing or invalid.")
}

fn verify(data: &[u8], expected: &str) -> Result<()> {
    if format!("{:x}", Sha256::digest(data)) != expected {
        bail!("The download failed its checksum check.");
    }
    Ok(())
}

async fn update(weak: &slint::Weak<AppWindow>) -> Result<bool> {
    let root = root()?;
    fs::create_dir_all(&root).context("Could not create the playback tools folder.")?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    // Also serialize multiple app instances. OS releases this lock on a crash.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join("update.lock"))?;
    lock.try_lock()
        .context("Another SlintYT window is updating playback tools.")?;
    publish(
        weak,
        true,
        "Checking for updates…".into(),
        false,
        Some(installed_versions(&root).await),
    );
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("SlintYT/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut changed = false;
    // Upgrade the runtime first, before a new yt-dlp might require it.
    for tool in [Tool::Deno, Tool::YtDlp] {
        let installed = version(&root.join(tool.name()), tool).await;
        let metadata = fetch(
            &client,
            &format!(
                "https://api.github.com/repos/{}/releases/latest",
                tool.repository()
            ),
            2 * 1024 * 1024,
        )
        .await
        .context("Could not check for playback tool updates. Check your connection and retry.")?;
        let release: Release = serde_json::from_slice(&metadata)
            .context("Could not read the playback tool release information.")?;
        let latest = release.tag_name.trim_start_matches('v');
        if installed.as_deref() == Some(latest) {
            continue;
        }
        let asset = tool.asset()?;
        publish(
            weak,
            true,
            format!("Downloading {} {latest}…", tool.name()),
            false,
            None,
        );
        let sums_name = match tool {
            Tool::YtDlp => "SHA2-256SUMS".into(),
            Tool::Deno => format!("{asset}.sha256sum"),
        };
        let sums = fetch(
            &client,
            release.url(tool.repository(), &sums_name)?,
            64 * 1024,
        )
        .await
        .context("Could not download playback tool checksums.")?;
        let expected = checksum(&sums, asset)?;
        let bytes = fetch(
            &client,
            release.url(tool.repository(), asset)?,
            MAX_DOWNLOAD,
        )
        .await
        .context("Could not download playback tools. Check your connection and retry.")?;
        install(&root, tool, bytes, &expected, latest).await?;
        changed = true;
    }
    Ok(changed)
}

async fn install(
    root: &Path,
    tool: Tool,
    bytes: Vec<u8>,
    expected: &str,
    latest: &str,
) -> Result<()> {
    let staging_root = root.to_path_buf();
    let expected = expected.to_owned();
    // Blocking work only creates temporary files. If cancelled, it can finish
    // safely and drop them; it cannot install anything after releasing the lock.
    let staging = tokio::task::spawn_blocking(move || {
        verify(&bytes, &expected)?;
        let staging = tempfile::tempdir_in(staging_root)?;
        let name = match tool {
            Tool::YtDlp => tool.name(),
            Tool::Deno => "deno.zip",
        };
        let mut file = File::create(staging.path().join(name))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok::<_, anyhow::Error>(staging)
    })
    .await
    .context("The playback tool installer stopped unexpectedly.")??;
    let staged = staging.path().join(tool.name());
    if matches!(tool, Tool::Deno) {
        // Extract just the expected entry; never trust archive paths.
        let mut command = Command::new("/usr/bin/unzip");
        command
            .arg("-p")
            .arg(staging.path().join("deno.zip"))
            .arg("deno")
            .stdout(File::create(&staged)?);
        run_command(command, Duration::from_secs(30))
            .await
            .context("Could not unpack Deno.")?;
    }
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o700))?;
    if version(&staged, tool).await.as_deref() != Some(latest) {
        bail!("The downloaded playback tool could not be verified on this Mac.");
    }
    // There are no cancellation points between verification and atomic replace.
    fs::rename(staged, root.join(tool.name())).context("Could not install the playback tool.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_exact_asset_and_reject_modified_downloads() {
        let hash = format!("{:x}", Sha256::digest(b"fixture"));
        let sums = format!("{}  deno.zip.other\n{hash} *deno.zip\n", "0".repeat(64));
        let expected = checksum(sums.as_bytes(), "deno.zip").unwrap();
        assert!(verify(b"fixture", &expected).is_ok());
        assert!(verify(b"modified", &expected).is_err());
        assert!(checksum(sums.as_bytes(), "missing.zip").is_err());
        assert!(checksum(b"invalid deno.zip", "deno.zip").is_err());
    }

    #[test]
    fn only_official_assets_are_accepted() {
        let mut release = Release {
            tag_name: "v2.9.7".into(),
            assets: vec![Asset {
                name: "deno.zip".into(),
                browser_download_url:
                    "https://github.com/denoland/deno/releases/download/v2.9.7/deno.zip".into(),
            }],
        };
        assert!(release.url("denoland/deno", "deno.zip").is_ok());
        assert!(release.url("denoland/deno", "missing.zip").is_err());
        release.assets[0].browser_download_url = "https://example.com/deno.zip".into();
        assert!(release.url("denoland/deno", "deno.zip").is_err());
    }

    #[tokio::test]
    async fn failed_install_preserves_working_binary_and_cleans_staging_files() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("yt-dlp"), b"existing").unwrap();
        assert!(
            install(
                root.path(),
                Tool::YtDlp,
                b"corrupt".to_vec(),
                &"0".repeat(64),
                "1"
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(root.path().join("yt-dlp")).unwrap(), b"existing");
        let hash = format!("{:x}", Sha256::digest(b"invalid-executable"));
        assert!(
            install(
                root.path(),
                Tool::YtDlp,
                b"invalid-executable".to_vec(),
                &hash,
                "1"
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(root.path().join("yt-dlp")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn install_validates_the_version_before_replacing_the_working_binary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("yt-dlp");
        fs::write(&path, b"existing").unwrap();
        let fixture = b"#!/bin/sh\nprintf '2026.08.19\\n'\n";
        let hash = format!("{:x}", Sha256::digest(fixture));
        assert!(
            install(
                root.path(),
                Tool::YtDlp,
                fixture.to_vec(),
                &hash,
                "2026.01.01"
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"existing");
        install(
            root.path(),
            Tool::YtDlp,
            fixture.to_vec(),
            &hash,
            "2026.08.19",
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), fixture);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn parses_tool_versions_without_extra_runtime_output() {
        assert_eq!(
            parse_version(b"deno 2.9.7 (stable)\nv8 other\n", Tool::Deno).as_deref(),
            Some("2.9.7")
        );
        assert_eq!(
            parse_version(b"2026.08.19\n", Tool::YtDlp).as_deref(),
            Some("2026.08.19")
        );
        assert!(parse_version(b"unexpected output", Tool::YtDlp).is_none());
    }

    #[tokio::test]
    async fn subprocess_timeout_kills_and_reaps_the_child() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "fixture"])
            .arg(&pid_file)
            .stdout(Stdio::null());
        let result = run_command(command, Duration::from_millis(500)).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        let pid = fs::read_to_string(pid_file).unwrap();
        let status = Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.trim())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!status.success(), "timed-out helper is still running");
    }

    #[tokio::test]
    async fn excessive_version_output_is_rejected() {
        let mut command = Command::new("/usr/bin/yes");
        command.stdout(Stdio::piped());
        let error = run_command(command, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("too much"));
    }

    #[tokio::test]
    async fn cancelling_a_subprocess_stops_the_child() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "fixture"])
            .arg(&pid_file)
            .stdout(Stdio::null());
        let task = tokio::spawn(run_command(command, Duration::from_secs(30)));
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(pid) = fs::read_to_string(&pid_file)
                    && !pid.trim().is_empty()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if !Command::new("/bin/kill")
                    .arg("-0")
                    .arg(pid.trim())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled helper is still running");
    }
}
