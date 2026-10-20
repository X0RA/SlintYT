use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

pub const BROWSERS: &[&str] = &[
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Selection {
    pub browser: String,
    pub profile: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyring: Option<String>,
}

impl Selection {
    pub fn validate(&self) -> Result<()> {
        if !BROWSERS.contains(&self.browser.as_str()) {
            bail!("Choose a supported browser.");
        }
        if self.keyring.as_deref().is_some_and(|value| {
            ![
                "GNOMEKEYRING",
                "KWALLET",
                "KWALLET5",
                "KWALLET6",
                "BASICTEXT",
            ]
            .contains(&value)
        }) {
            bail!("Choose a supported cookie keyring.");
        }
        let path = self.profile.to_string_lossy();
        if !self.profile.is_absolute() || path.chars().any(char::is_control) || path.contains("::")
        {
            bail!("Enter an absolute profile path without control characters or ::.");
        }
        if !self.cookie_paths().iter().any(|p| p.is_file()) {
            bail!(
                "No cookie database was found in this exact profile. Choose another profile or check the path."
            );
        }
        Ok(())
    }

    fn cookie_paths(&self) -> Vec<PathBuf> {
        match self.browser.as_str() {
            "firefox" => vec![self.profile.join("cookies.sqlite")],
            "safari" => vec![self.profile.clone()],
            _ => vec![
                self.profile.join("Network/Cookies"),
                self.profile.join("Cookies"),
            ],
        }
    }

    pub fn label(&self) -> String {
        let profile = self
            .profile
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let mut root = self.profile.parent().unwrap_or(&self.profile);
        if matches!(
            root.file_name().and_then(|v| v.to_str()),
            Some("Profiles" | "User Data")
        ) {
            root = root.parent().unwrap_or(root);
        }
        let app = root.file_name().unwrap_or_default().to_string_lossy();
        let app = app.trim_start_matches('.');
        let name = match app {
            "net.imput.helium" | "helium" => "Helium".into(),
            "zen" => "Zen".into(),
            "librewolf" => "LibreWolf".into(),
            _ if matches!(self.browser.as_str(), "chromium" | "firefox")
                && !app.eq_ignore_ascii_case(&self.browser) =>
            {
                app.to_owned()
            }
            _ => {
                let mut name = self.browser.clone();
                if let Some(first) = name.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                name
            }
        };
        format!("{name} · {profile}")
    }
}

pub struct Discovery {
    pub profiles: Vec<Selection>,
    pub warning: String,
}

pub fn discover() -> Result<Discovery> {
    let dirs = BaseDirs::new().context("Could not locate your home directory.")?;
    let home = dirs.home_dir();
    let config = dirs.config_dir();
    let mut known: Vec<(&str, PathBuf)> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        for (browser, path) in [
            ("brave", "BraveSoftware/Brave-Browser"),
            ("chrome", "google-chrome"),
            ("chromium", "chromium"),
            ("edge", "microsoft-edge"),
            ("opera", "opera"),
            ("vivaldi", "vivaldi"),
            ("whale", "naver-whale"),
            ("chromium", "net.imput.helium"),
            ("chromium", "helium"),
            ("firefox", "zen"),
            ("firefox", "librewolf"),
        ] {
            known.push((browser, config.join(path)));
        }
        for path in [
            config.join("mozilla/firefox"),
            home.join(".mozilla/firefox"),
            home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
            home.join(".var/app/org.mozilla.firefox/config/mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ] {
            known.push(("firefox", path));
        }
        known.push((
            "chrome",
            home.join(".var/app/com.google.Chrome/config/google-chrome"),
        ));
        known.push(("chromium", home.join("snap/chromium/common/chromium")));
    }
    #[cfg(target_os = "macos")]
    {
        let base = home.join("Library/Application Support");
        for (browser, path) in [
            ("brave", "BraveSoftware/Brave-Browser"),
            ("chrome", "Google/Chrome"),
            ("chromium", "Chromium"),
            ("edge", "Microsoft Edge"),
            ("opera", "com.operasoftware.Opera"),
            ("vivaldi", "Vivaldi"),
            ("firefox", "Firefox/Profiles"),
        ] {
            known.push((browser, base.join(path)));
        }
        known.push(("safari", home.join("Library/Cookies/Cookies.binarycookies")));
        known.push((
            "safari",
            home.join(
                "Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies",
            ),
        ));
    }
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home.join("AppData/Local"));
        for (browser, path) in [
            ("chrome", "Google/Chrome/User Data"),
            ("chromium", "Chromium/User Data"),
            ("brave", "BraveSoftware/Brave-Browser/User Data"),
            ("edge", "Microsoft/Edge/User Data"),
            ("vivaldi", "Vivaldi/User Data"),
        ] {
            known.push((browser, local.join(path)));
        }
        known.push(("firefox", config.join("Mozilla/Firefox/Profiles")));
        known.push(("opera", config.join("Opera Software/Opera Stable")));
    }
    let mut profiles = Vec::new();
    let mut seen = HashSet::new();
    for (browser, root) in known {
        let mut candidates = vec![root.clone()];
        for parent in [root.clone(), root.join("Profiles")] {
            if let Ok(entries) = fs::read_dir(parent) {
                candidates.extend(
                    entries
                        .flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.path()),
                );
            }
        }
        for path in candidates {
            add_profile(browser, &path, &mut profiles, &mut seen);
        }
    }
    let mut roots = vec![config.to_path_buf()];
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = fs::read_dir(home) {
            roots.extend(
                entries
                    .flatten()
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        e.file_type().is_ok_and(|t| t.is_dir())
                            && name.starts_with('.')
                            && !skip(&name)
                            && !matches!(name.as_str(), ".config" | ".local" | ".var")
                    })
                    .map(|e| e.path()),
            );
        }
        roots.extend([
            home.join(".var/app"),
            home.join("snap"),
            home.join(".local/share"),
        ]);
    }
    #[cfg(target_os = "macos")]
    roots.extend([
        home.join("Library/Application Support"),
        home.join("Library/Containers"),
    ]);
    #[cfg(target_os = "windows")]
    roots.push(home.join("AppData/Local"));
    let mut remaining = 50_000;
    let mut visited = HashSet::new();
    for root in roots {
        scan(
            &root,
            0,
            &mut remaining,
            &mut visited,
            &mut profiles,
            &mut seen,
        );
    }
    profiles.sort_by_key(|p| (p.browser.clone(), p.profile.clone()));
    let warning = if remaining == 0 {
        "The profile scan reached its limit. You can enter a missing profile manually.".into()
    } else {
        String::new()
    };
    Ok(Discovery { profiles, warning })
}

fn add_profile(
    browser: &str,
    path: &Path,
    profiles: &mut Vec<Selection>,
    seen: &mut HashSet<PathBuf>,
) {
    let selection = Selection {
        browser: browser.into(),
        profile: path.into(),
        keyring: None,
    };
    if selection.validate().is_ok()
        && let Ok(canonical) = path.canonicalize()
        && seen.insert(canonical.clone())
    {
        profiles.push(Selection {
            profile: canonical,
            ..selection
        });
    }
}

fn family(path: &Path) -> Option<&'static str> {
    if path.join("cookies.sqlite").is_file()
        && (path.join("prefs.js").is_file()
            || path.parent().is_some_and(|p| {
                p.join("profiles.ini").is_file()
                    || p.parent().is_some_and(|p| p.join("profiles.ini").is_file())
            }))
    {
        Some("firefox")
    } else if path.join("Preferences").is_file()
        && ["History", "Bookmarks", "Favicons"]
            .iter()
            .any(|name| path.join(name).is_file())
        && (path.join("Cookies").is_file() || path.join("Network/Cookies").is_file())
        && (path.join("Local State").is_file()
            || path
                .parent()
                .is_some_and(|p| p.join("Local State").is_file()))
    {
        Some("chromium")
    } else {
        None
    }
}

fn skip(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "cache"
            | "caches"
            | "code cache"
            | "gpucache"
            | "shadercache"
            | "grshadercache"
            | "dawncache"
            | "dawngraphitecache"
            | "dawnwebgpucache"
            | "cache2"
            | "startupcache"
            | "storage"
            | "indexeddb"
            | "local storage"
            | "session storage"
            | "service worker"
            | "extensions"
            | "extension state"
            | "logs"
            | "crash reports"
            | "crashpad"
            | "node_modules"
            | ".git"
            | ".cache"
            | ".npm"
            | ".cargo"
            | ".rustup"
            | ".ssh"
            | ".gnupg"
            | ".aws"
            | ".codex"
            | ".agents"
            | "trash"
    )
}

fn scan(
    path: &Path,
    depth: usize,
    remaining: &mut usize,
    visited: &mut HashSet<PathBuf>,
    profiles: &mut Vec<Selection>,
    seen: &mut HashSet<PathBuf>,
) {
    if *remaining == 0
        || depth > 7
        || !visited.insert(path.into())
        || !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
    {
        return;
    }
    *remaining -= 1;
    if let Some(browser) = family(path) {
        add_profile(browser, path, profiles, seen);
        return;
    }
    if depth == 7 {
        return;
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if *remaining == 0 {
                break;
            }
            *remaining -= 1;
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && !skip(&entry.file_name().to_string_lossy())
            {
                scan(&entry.path(), depth + 1, remaining, visited, profiles, seen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(root: &Path, name: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            "not a database; discovery must only inspect filenames",
        )
        .unwrap();
    }

    #[test]
    fn signature_scan_finds_derivatives_without_cache_false_positives() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in [
            "unknown/Local State",
            "unknown/Default/Preferences",
            "unknown/Default/History",
            "unknown/Default/Network/Cookies",
            "unknown/Default/Cookies",
            "zen/prefs.js",
            "zen/cookies.sqlite",
            "unrelated/cookies.sqlite",
            "unrelated/Cookies",
            "electron/Preferences",
            "electron/Local State",
            "electron/Cookies",
            "cache/backup/prefs.js",
            "cache/backup/cookies.sqlite",
        ] {
            file(root, name);
        }
        let mut profiles = Vec::new();
        scan(
            root,
            0,
            &mut 50_000,
            &mut HashSet::new(),
            &mut profiles,
            &mut HashSet::new(),
        );
        assert_eq!(profiles.len(), 2);
        assert!(
            profiles
                .iter()
                .any(|p| p.browser == "chromium" && p.profile.ends_with("unknown/Default"))
        );
        assert!(
            profiles
                .iter()
                .any(|p| p.browser == "firefox" && p.profile.ends_with("zen"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn signature_scan_does_not_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        file(outside.path(), "prefs.js");
        file(outside.path(), "cookies.sqlite");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        let mut profiles = Vec::new();
        scan(
            root.path(),
            0,
            &mut 50_000,
            &mut HashSet::new(),
            &mut profiles,
            &mut HashSet::new(),
        );
        assert!(profiles.is_empty());
    }

    #[test]
    fn selection_rejects_missing_exact_profile_and_malformed_paths() {
        let root = tempfile::tempdir().unwrap();
        file(root.path(), "child/Cookies");
        let selection = Selection {
            browser: "chrome".into(),
            profile: root.path().into(),
            keyring: None,
        };
        assert!(selection.validate().is_err());
        let invalid = Selection {
            browser: "chrome".into(),
            profile: "relative/path".into(),
            keyring: None,
        };
        assert!(invalid.validate().is_err());
    }
}
