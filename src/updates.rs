//! Checks GitHub for a newer SlintYT release. On Linux and macOS it can also
//! run that release's official installer script, which asks for admin rights
//! through the OS prompt (pkexec / macOS administrator dialog) when needed.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use slint::ComponentHandle;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::path::Path;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
// Windows only checks for releases; running the installer is Linux and macOS only.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{ffi::OsStr, process::Stdio};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{Notify, mpsc},
};

use crate::{
    AppState, AppWindow,
    github::{fetch, user_agent},
};

const REPOSITORY: &str = "X0RA/SlintYT";
/// Stamped by the release workflow. Development builds have no version and
/// never check for or install updates.
pub const VERSION: Option<&str> = option_env!("SLINTYT_VERSION");
#[cfg(target_os = "linux")]
const SCRIPT: &str = "install-linux.sh";
#[cfg(target_os = "macos")]
const SCRIPT: &str = "install-macos.sh";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const INSTALL_SLOW_NOTICE: Duration = Duration::from_secs(600);
/// Exit status the installer scripts use when the elevation prompt is declined.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PERMISSION_DENIED: i32 = 126;
/// The installer prints a line starting with this just before files may change.
/// Both scripts ignore stop signals from then on.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COMMIT_MARKER: &str = "Installing to ";

static UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
// Normal application shutdown must not abandon a helper midway through replacing
// the app. Keep its task alive until the installer and its children have exited.
static INSTALL_TASK: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
/// Asks the running installer to stop while it is still only downloading.
#[cfg(any(target_os = "linux", target_os = "macos"))]
static CANCEL: Notify = Notify::const_new();
/// Set while the installer may be replacing files and so must not be stopped.
static COMMITTING: AtomicBool = AtomicBool::new(false);
/// The window was closed while committing; hide it once the install ends.
static CLOSE_PENDING: AtomicBool = AtomicBool::new(false);

/// Stops an install that is still downloading, or waits for one that may be
/// replacing files.
pub async fn shutdown() {
    let task = INSTALL_TASK.lock().unwrap().take();
    let Some(task) = task else { return };
    if !task.is_finished() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        CANCEL.notify_one();
        if COMMITTING.load(Ordering::SeqCst) {
            eprintln!("Waiting for the SlintYT update to finish…");
        }
    }
    let _ = task.await;
}

type Version = (u32, u32, u32);

/// Accepts exactly `X.Y.Z` or `vX.Y.Z` with plain decimal parts.
fn parse_version(value: &str) -> Option<Version> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let mut parts = value.split('.').map(|part| {
        let canonical = !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        canonical.then(|| part.parse::<u32>().ok()).flatten()
    });
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

fn tag(version: Version) -> String {
    format!("v{}.{}.{}", version.0, version.1, version.2)
}

fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/{tag}")
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// Where the running copy was installed, if the installer script owns it.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", test)),
    allow(dead_code)
)]
struct Location {
    /// Linux install prefix passed to the script as `PREFIX`.
    prefix: Option<PathBuf>,
}

/// The Linux script installs `<prefix>/bin/slintyt` plus a desktop file.
/// `/usr` belongs to the distribution's package manager, so it is never
/// overwritten; cargo build and install folders are skipped too.
#[cfg(any(target_os = "linux", test))]
fn linux_location(exe: &Path, prefix_has_desktop_file: impl Fn(&Path) -> bool) -> Option<Location> {
    if exe.file_name()? != "slintyt" {
        return None;
    }
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let prefix = bin.parent()?;
    if !prefix.is_absolute()
        || prefix == Path::new("/")
        || prefix == Path::new("/usr")
        || prefix.ends_with(".cargo")
        || !prefix_has_desktop_file(prefix)
    {
        return None;
    }
    Some(Location {
        prefix: Some(prefix.to_owned()),
    })
}

/// The macOS script always installs `/Applications/SlintYT.app`.
#[cfg(any(target_os = "macos", test))]
fn macos_location(exe: &Path) -> Option<Location> {
    exe.starts_with("/Applications/SlintYT.app/Contents/MacOS")
        .then_some(Location { prefix: None })
}

fn location() -> Option<Location> {
    let exe = std::env::current_exe().ok()?;
    #[cfg(target_os = "linux")]
    return linux_location(&exe, |prefix| {
        prefix.join("share/applications/slintyt.desktop").is_file()
    });
    #[cfg(target_os = "macos")]
    return macos_location(&exe);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = exe;
        None
    }
}

#[derive(Default)]
struct Shared {
    /// The newer release found by the last check.
    latest: Option<String>,
    /// A release installed during this session, used on the next launch.
    installed: Option<String>,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()?)
}

async fn latest_release() -> Result<Version> {
    let metadata = fetch(
        &client()?,
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
        2 * 1024 * 1024,
    )
    .await
    .context("Could not check for updates. Check your connection and try again.")?;
    let release: Release = serde_json::from_slice(&metadata)
        .context("Could not read the SlintYT release information.")?;
    let version = parse_version(&release.tag_name)
        .context("The latest SlintYT release has an unexpected version.")?;
    // Only the exact canonical tag is used in download URLs.
    if tag(version) != release.tag_name {
        bail!("The latest SlintYT release has an unexpected version.");
    }
    Ok(version)
}

struct Status {
    busy: bool,
    error: bool,
    text: String,
    latest: String,
    installable: bool,
}

fn publish(weak: &slint::Weak<AppWindow>, status: Status) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let state = app.global::<AppState>();
        state.set_update_busy(status.busy);
        state.set_update_error(status.error);
        state.set_update_status(status.text.into());
        state.set_update_url(if status.latest.is_empty() {
            "".into()
        } else {
            release_url(&status.latest).into()
        });
        state.set_update_latest(status.latest.into());
        state.set_update_installable(status.installable);
    });
}

pub fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let state = app.global::<AppState>();
    let location = location();
    state.set_app_version(VERSION.unwrap_or_default().into());
    // Only a copy the official installer owns can replace itself.
    state.set_update_supported(location.is_some());
    let Some(current) = VERSION.and_then(parse_version) else {
        state.set_update_status("This development build doesn't check for updates.".into());
        return;
    };
    let shared = Arc::new(Mutex::new(Shared::default()));

    // Closing the window must not leave a hidden process waiting on a password
    // prompt or file replacement, so keep it open and say what is happening.
    let weak = app.as_weak();
    app.window().on_close_requested(move || {
        if !COMMITTING.load(Ordering::SeqCst) {
            return slint::CloseRequestResponse::HideWindow;
        }
        CLOSE_PENDING.store(true, Ordering::SeqCst);
        if let Some(app) = weak.upgrade() {
            app.global::<AppState>()
                .set_toast("Finishing the update. SlintYT will close when it's done.".into());
        }
        slint::CloseRequestResponse::KeepWindowShown
    });

    let (weak, handle, check_shared) = (app.as_weak(), runtime.clone(), shared.clone());
    let check_location = location.clone();
    state.on_check_app_update(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.global::<AppState>().get_update_busy() {
            return;
        }
        app.global::<AppState>().set_update_busy(true);
        handle.spawn(check(
            weak.clone(),
            current,
            check_location.clone(),
            check_shared.clone(),
            false,
        ));
    });

    let (weak, handle, install_shared) = (app.as_weak(), runtime.clone(), shared.clone());
    let install_location = location.clone();
    state.on_install_app_update(move || {
        let Some(app) = weak.upgrade() else { return };
        let state = app.global::<AppState>();
        let latest = install_shared.lock().unwrap().latest.clone();
        let (Some(latest), Some(location)) = (latest, install_location.clone()) else {
            return;
        };
        if state.get_update_busy() {
            return;
        }
        state.set_update_busy(true);
        let task = handle.spawn(install(
            weak.clone(),
            latest,
            location,
            install_shared.clone(),
        ));
        *INSTALL_TASK.lock().unwrap() = Some(task);
    });

    // Stay out of the way of the first feed load.
    let weak = app.as_weak();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        check(weak, current, location, shared, true).await;
    });
}

async fn check(
    weak: slint::Weak<AppWindow>,
    current: Version,
    location: Option<Location>,
    shared: Arc<Mutex<Shared>>,
    startup: bool,
) {
    let _guard = UPDATE.lock().await;
    publish(
        &weak,
        Status {
            busy: true,
            error: false,
            text: "Checking for updates…".into(),
            latest: String::new(),
            installable: false,
        },
    );
    let latest = match latest_release().await {
        Ok(latest) => latest,
        Err(error) => {
            publish(
                &weak,
                Status {
                    busy: false,
                    error: true,
                    text: error.to_string(),
                    latest: String::new(),
                    installable: false,
                },
            );
            return;
        }
    };
    let latest_tag = tag(latest);
    let installed = shared.lock().unwrap().installed.clone();
    let status = if latest <= current {
        shared.lock().unwrap().latest = None;
        Status {
            busy: false,
            error: false,
            text: format!("SlintYT is up to date ({}).", tag(current)),
            latest: String::new(),
            installable: false,
        }
    } else if installed.as_deref() == Some(latest_tag.as_str()) {
        Status {
            busy: false,
            error: false,
            text: installed_message(&latest_tag),
            latest: latest_tag,
            installable: false,
        }
    } else {
        shared.lock().unwrap().latest = Some(latest_tag.clone());
        if startup {
            let notice = format!("SlintYT {latest_tag} is available. See Settings → Updates.");
            let _ = weak.upgrade_in_event_loop(move |app| {
                app.global::<AppState>().set_toast(notice.into());
            });
        }
        let installable = cfg!(any(target_os = "linux", target_os = "macos")) && location.is_some();
        let text = if installable {
            format!("SlintYT {latest_tag} is available.")
        } else if cfg!(any(target_os = "linux", target_os = "macos")) {
            format!(
                "SlintYT {latest_tag} is available. This copy wasn't installed by the SlintYT installer, so update it the same way you installed it."
            )
        } else {
            format!("SlintYT {latest_tag} is available. Download it from the release page.")
        };
        Status {
            busy: false,
            error: false,
            text,
            latest: latest_tag,
            installable,
        }
    };
    publish(&weak, status);
}

fn installed_message(tag: &str) -> String {
    format!("Installed {tag}. It will be used next time you open SlintYT.")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn install(
    weak: slint::Weak<AppWindow>,
    latest: String,
    location: Location,
    shared: Arc<Mutex<Shared>>,
) {
    let _guard = UPDATE.lock().await;
    COMMITTING.store(false, Ordering::SeqCst);
    let status = |text: String, busy: bool, error: bool, installable: bool| Status {
        busy,
        error,
        text,
        latest: latest.clone(),
        installable,
    };
    publish(
        &weak,
        status(
            format!("Downloading the {latest} installer…"),
            true,
            false,
            false,
        ),
    );
    let progress_weak = weak.clone();
    let progress_latest = latest.clone();
    let result = download_and_run(&latest, &location, move |line| {
        publish(
            &progress_weak,
            Status {
                busy: true,
                error: false,
                text: line,
                latest: progress_latest.clone(),
                installable: false,
            },
        );
    })
    .await;
    match result {
        Ok(()) => {
            let mut shared = shared.lock().unwrap();
            shared.installed = Some(latest.clone());
            shared.latest = None;
            publish(
                &weak,
                status(installed_message(&latest), false, false, false),
            );
        }
        // The helper has exited (and attempted rollback), so retry is now safe.
        Err(error) => publish(&weak, status(error.to_string(), false, true, true)),
    }
    COMMITTING.store(false, Ordering::SeqCst);
    if CLOSE_PENDING.swap(false, Ordering::SeqCst) {
        let _ = weak.upgrade_in_event_loop(|app| {
            let _ = app.hide();
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn install(
    _weak: slint::Weak<AppWindow>,
    _latest: String,
    _location: Location,
    _shared: Arc<Mutex<Shared>>,
) {
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn download_and_run(
    latest: &str,
    location: &Location,
    progress: impl FnMut(String),
) -> Result<()> {
    use std::io::Write;
    // Pinned to the release tag so the script matches the assets it installs.
    let url = format!("https://raw.githubusercontent.com/{REPOSITORY}/{latest}/scripts/{SCRIPT}");
    let client = client()?;
    let script = tokio::select! {
        script = fetch(&client, &url, 256 * 1024) => script
            .context("Could not download the installer. Check your connection and try again.")?,
        () = CANCEL.notified() => bail!("The update was cancelled."),
    };
    if !script.starts_with(b"#!") {
        bail!("The downloaded installer is not a script.");
    }
    let mut file = tempfile::Builder::new()
        .prefix("slintyt-update-")
        .suffix(".sh")
        .tempfile()
        .context("Could not save the installer.")?;
    file.write_all(&script)
        .and_then(|()| file.flush())
        .context("Could not save the installer.")?;
    let mut env = vec![
        ("SLINTYT_VERSION", latest.to_owned()),
        ("SLINTYT_ELEVATE", "gui".to_owned()),
    ];
    if let Some(prefix) = &location.prefix {
        env.push(("PREFIX", prefix.to_string_lossy().into_owned()));
    }
    run_installer(
        OsStr::new("bash"),
        file.path(),
        &env,
        INSTALL_SLOW_NOTICE,
        &CANCEL,
        &COMMITTING,
        progress,
    )
    .await
}

/// Runs `program script` and reports readable output lines as progress.
/// A slow helper is still running, not a failed install. Never kill just its
/// wrapper: an elevated descendant may be committing or rolling back files.
/// `cancel` stops the installer only before it prints [`COMMIT_MARKER`];
/// `committing` is set once it has.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run_installer(
    program: &OsStr,
    script: &Path,
    env: &[(&str, String)],
    slow_notice: Duration,
    cancel: &Notify,
    committing: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Result<()> {
    let mut child = Command::new(program)
        .arg(script)
        // Always install from the official repository and the checked tag.
        .env_remove("SLINTYT_REPO")
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so stopping it also stops curl and hdiutil.
        .process_group(0)
        .spawn()
        .context("The installer could not be started.")?;
    let group = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok());
    let (send, mut receive) = mpsc::unbounded_channel();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(forward_lines(stdout, send.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_lines(stderr, send));
    }
    let mut last = String::new();
    let notice = tokio::time::sleep(slow_notice);
    tokio::pin!(notice);
    let (mut notified, mut cancelled) = (false, false);
    loop {
        tokio::select! {
            line = receive.recv() => {
                let Some(line) = line else { break };
                if line.starts_with(COMMIT_MARKER) {
                    committing.store(true, Ordering::SeqCst);
                }
                last.clone_from(&line);
                progress(line);
            }
            () = &mut notice, if !notified => {
                notified = true;
                progress("The installer is still running. Check for a password dialog; waiting for it to finish safely…".into());
            }
            () = cancel.notified(), if !cancelled => {
                cancelled = true;
                // If the marker is printed but not yet read, the installer
                // already ignores this signal, so it cannot interrupt a commit.
                if !committing.load(Ordering::SeqCst)
                    && let Some(group) = group
                {
                    // SAFETY: kill only sends a signal; a negative pid names
                    // the installer's process group.
                    unsafe { libc::kill(-group, libc::SIGTERM) };
                }
            }
        }
    }
    let status = child
        .wait()
        .await
        .context("The installer stopped unexpectedly.")?;
    match status.code() {
        Some(0) => Ok(()),
        _ if cancelled && !committing.load(Ordering::SeqCst) => {
            bail!("The update was cancelled, so SlintYT was not changed.")
        }
        Some(PERMISSION_DENIED) => {
            bail!("Permission was not granted, so SlintYT was not changed.")
        }
        _ if last.is_empty() => {
            bail!("The update failed. Check the installation before trying again.")
        }
        _ => bail!("The update failed: {last}"),
    }
}

/// Keeps human-readable lines only. curl's progress meter is skipped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn readable(line: &str) -> Option<String> {
    let line = line.rsplit('\r').next().unwrap_or_default().trim();
    let first = line.chars().next()?;
    if first.is_ascii_digit() || first == '%' || first == '#' {
        return None;
    }
    Some(line.chars().take(200).collect())
}

/// Drains the pipe until the installer closes it. Closing it early would kill
/// the installer's next write with SIGPIPE, so undecodable bytes are replaced.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn forward_lines(reader: impl AsyncRead + Unpin, send: mpsc::UnboundedSender<String>) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                if let Some(text) = readable(text.trim_end_matches(['\n', '\r'])) {
                    let _ = send.send(text);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_strict_and_compare_numerically() {
        assert_eq!(parse_version("v0.0.7"), Some((0, 0, 7)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert!(parse_version("v0.0.10") > parse_version("v0.0.9"));
        assert!(parse_version("v0.1.0") > parse_version("v0.0.99"));
        for bad in [
            "",
            "v",
            "v1.2",
            "1.2.3.4",
            "1.2.3-rc1",
            "v1.2.x",
            "v01.2.3",
            "v1..3",
            " 1.2.3",
            "v1.2.3\n",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert_eq!(tag((0, 0, 8)), "v0.0.8");
        assert_eq!(
            release_url("v0.0.8"),
            "https://github.com/X0RA/SlintYT/releases/tag/v0.0.8"
        );
    }

    #[test]
    fn only_installer_owned_locations_can_update() {
        let yes = |_: &Path| true;
        let installed = linux_location(Path::new("/usr/local/bin/slintyt"), yes).unwrap();
        assert_eq!(installed.prefix.as_deref(), Some(Path::new("/usr/local")));
        assert!(linux_location(Path::new("/home/me/.local/bin/slintyt"), yes).is_some());
        for path in [
            "/usr/bin/slintyt",
            "/bin/slintyt",
            "/home/me/.cargo/bin/slintyt",
            "/home/me/SlintYT/target/release/slintyt",
            "/usr/local/bin/other",
            "bin/slintyt",
        ] {
            assert!(linux_location(Path::new(path), yes).is_none(), "{path}");
        }
        // Without the installer's desktop file (e.g. a manual copy) no update.
        assert!(linux_location(Path::new("/usr/local/bin/slintyt"), |_| false).is_none());
        assert!(
            macos_location(Path::new(
                "/Applications/SlintYT.app/Contents/MacOS/slintyt"
            ))
            .is_some()
        );
        assert!(
            macos_location(Path::new(
                "/Users/me/Downloads/SlintYT.app/Contents/MacOS/slintyt"
            ))
            .is_none()
        );
        assert!(
            macos_location(Path::new("/Applications/Other.app/Contents/MacOS/slintyt")).is_none()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn progress_shows_messages_but_not_curl_meters() {
        assert_eq!(
            readable("Downloading slintyt.dmg...").as_deref(),
            Some("Downloading slintyt.dmg...")
        );
        assert_eq!(readable("  % Total    % Received"), None);
        assert_eq!(readable("100  1234k  100 1234k"), None);
        assert_eq!(
            readable("\r  5  100k\rInstalling to /Applications"),
            Some("Installing to /Applications".into())
        );
        assert_eq!(readable("   "), None);
        assert_eq!(readable(&"x".repeat(500)).unwrap().len(), 200);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn script(root: &Path, body: &str) -> std::path::PathBuf {
        let path = root.join("installer.sh");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn installer_receives_the_release_and_reports_progress() {
        let root = tempfile::tempdir().unwrap();
        let path = script(
            root.path(),
            r#"
[ "$SLINTYT_VERSION" = v0.0.8 ] || exit 3
[ "$SLINTYT_ELEVATE" = gui ] || exit 4
[ -z "$SLINTYT_REPO" ] || exit 5
[ "$PREFIX" = /opt/slintyt ] || exit 6
echo 'Downloading...'
echo 'Installed.' >&2
"#,
        );
        let env = [
            ("SLINTYT_VERSION", "v0.0.8".to_owned()),
            ("SLINTYT_ELEVATE", "gui".to_owned()),
            ("PREFIX", "/opt/slintyt".to_owned()),
        ];
        let mut lines = Vec::new();
        run_installer(
            OsStr::new("sh"),
            &path,
            &env,
            Duration::from_secs(5),
            &Notify::new(),
            &AtomicBool::new(false),
            |line| lines.push(line),
        )
        .await
        .unwrap();
        lines.sort();
        assert_eq!(lines, ["Downloading...", "Installed."]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn failures_are_explained_and_slow_helpers_are_awaited() {
        let root = tempfile::tempdir().unwrap();
        let denied = script(
            root.path(),
            "echo 'Permission was not granted.' >&2\nexit 126\n",
        );
        let error = run_installer(
            OsStr::new("sh"),
            &denied,
            &[],
            Duration::from_secs(5),
            &Notify::new(),
            &AtomicBool::new(false),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Permission was not granted"));

        let failed = script(
            root.path(),
            "echo 'Missing checksum for slintyt' >&2\nexit 1\n",
        );
        let error = run_installer(
            OsStr::new("sh"),
            &failed,
            &[],
            Duration::from_secs(5),
            &Notify::new(),
            &AtomicBool::new(false),
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "The update failed: Missing checksum for slintyt"
        );

        let marker = root.path().join("finished");
        let slow = script(
            root.path(),
            // A separate child represents an elevated helper. We must wait for
            // it instead of killing only the wrapper and allowing another try.
            &format!(
                "sh -c 'sleep 1; touch \"$1\"' sh '{}' &\nwait\n",
                marker.display()
            ),
        );
        let mut lines = Vec::new();
        run_installer(
            OsStr::new("sh"),
            &slow,
            &[],
            Duration::from_millis(200),
            &Notify::new(),
            &AtomicBool::new(false),
            |line| lines.push(line),
        )
        .await
        .unwrap();
        assert!(lines.iter().any(|line| line.contains("still running")));
        assert!(
            marker.exists(),
            "reported completion before the helper exited"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cancelling_while_downloading_stops_the_whole_installer() {
        let root = tempfile::tempdir().unwrap();
        // The sleep stands in for curl, a child that must be stopped too.
        let path = script(root.path(), "echo 'Downloading...'\nsleep 30\n");
        let cancel = Notify::new();
        let committing = AtomicBool::new(false);
        cancel.notify_one();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_installer(
                OsStr::new("sh"),
                &path,
                &[],
                Duration::from_secs(60),
                &cancel,
                &committing,
                |_| {},
            ),
        )
        .await
        .expect("the installer was not stopped");
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(!committing.load(Ordering::SeqCst));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cancelling_after_the_commit_marker_waits_for_the_installer() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("finished");
        // No signal trap here, so a stop signal would leave `finished` missing.
        let path = script(
            root.path(),
            &format!(
                "echo 'Installing to /opt/slintyt...'\nsleep 1\ntouch '{}'\n",
                marker.display()
            ),
        );
        let cancel = Notify::new();
        let committing = AtomicBool::new(false);
        run_installer(
            OsStr::new("sh"),
            &path,
            &[],
            Duration::from_secs(60),
            &cancel,
            &committing,
            |line| {
                if line.starts_with(COMMIT_MARKER) {
                    cancel.notify_one();
                }
            },
        )
        .await
        .unwrap();
        assert!(committing.load(Ordering::SeqCst));
        assert!(
            marker.exists(),
            "the installer was stopped while committing"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn undecodable_output_does_not_stop_reading() {
        let root = tempfile::tempdir().unwrap();
        let path = script(root.path(), "printf 'bad \\377 byte\\n'\necho 'after'\n");
        let mut lines = Vec::new();
        run_installer(
            OsStr::new("sh"),
            &path,
            &[],
            Duration::from_secs(60),
            &Notify::new(),
            &AtomicBool::new(false),
            |line| lines.push(line),
        )
        .await
        .unwrap();
        assert_eq!(lines, ["bad \u{fffd} byte", "after"]);
    }
}
