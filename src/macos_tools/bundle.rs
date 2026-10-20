//! Immutable yt-dlp installations, pinned while a subprocess uses their libraries.
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    os::unix::fs::symlink,
    path::{Component, Path, PathBuf},
};

const CURRENT: &str = "yt-dlp-current";
const PREFIX: &str = "yt-dlp-bundle-";
const LEASE: &str = ".lease";

pub(super) struct Installation {
    pub executable: PathBuf,
    _lease: File,
}

pub(super) fn open(root: &Path) -> Result<Installation> {
    // Retry if an update switched the pointer and collected the old directory
    // between canonicalizing the pointer and acquiring its shared lease.
    for _ in 0..3 {
        if let Ok(directory) = root.join(CURRENT).canonicalize() {
            let Ok(lease) = File::open(directory.join(LEASE)) else {
                continue;
            };
            if lease.try_lock_shared().is_err() {
                continue;
            }
            let executable = directory.join("yt-dlp_macos");
            if executable.is_file() {
                return Ok(Installation {
                    executable,
                    _lease: lease,
                });
            }
        } else {
            break;
        }
    }
    bail!("yt-dlp is not installed.");
}

pub(super) fn staging(root: &Path) -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new().prefix(PREFIX).tempdir_in(root)?)
}

pub(super) fn activate(root: &Path, staging: tempfile::TempDir) -> Result<()> {
    File::create(staging.path().join(LEASE))?;
    let link = staging.path().join("current-link");
    symlink(
        staging
            .path()
            .file_name()
            .context("Invalid tool directory.")?,
        &link,
    )?;
    // Readers see either complete installation. Keep old directories until
    // every running extraction has released its shared lease.
    fs::rename(link, root.join(CURRENT)).context("Could not activate yt-dlp.")?;
    let _ = staging.keep();
    cleanup(root);
    Ok(())
}

fn cleanup(root: &Path) {
    let Ok(current) = root.join(CURRENT).canonicalize() else {
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(PREFIX)
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            || entry
                .path()
                .canonicalize()
                .is_ok_and(|path| path == current)
        {
            continue;
        }
        let Ok(lease) = File::open(entry.path().join(LEASE)) else {
            continue;
        };
        if lease.try_lock().is_ok() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub(super) fn validate_entries(listing: &[u8]) -> Result<()> {
    let listing = std::str::from_utf8(listing)?;
    let mut executable = false;
    for name in listing.lines() {
        let path = Path::new(name);
        if name.is_empty()
            || name.contains('\\')
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !(name == "yt-dlp_macos" || name.starts_with("_internal/"))
        {
            bail!("The yt-dlp archive contains an unexpected path.");
        }
        executable |= name == "yt-dlp_macos";
    }
    if !executable {
        bail!("The yt-dlp archive is missing its executable.");
    }
    Ok(())
}

pub(super) fn validate_types(listing: &[u8]) -> Result<()> {
    // zipinfo's long listing starts file entries with Unix permission bits.
    // Do not allow symlinks/special files to redirect extraction outside staging.
    let listing = std::str::from_utf8(listing)?;
    for line in listing.lines() {
        if line
            .as_bytes()
            .get(1)
            .is_some_and(|c| *c == b'r' || *c == b'-')
            && line
                .split_whitespace()
                .next()
                .is_some_and(|mode| mode.len() == 10)
            && !line.starts_with(['-', 'd'])
        {
            bail!("The yt-dlp archive contains an unsupported file type.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_cannot_escape_staging_or_replace_other_tools() {
        assert!(validate_entries(b"_internal/\n_internal/libpython.dylib\nyt-dlp_macos\n").is_ok());
        for entry in [
            "../outside",
            "/absolute",
            "_internal/../../outside",
            "deno",
            "_internal\\..\\outside",
        ] {
            assert!(validate_entries(format!("yt-dlp_macos\n{entry}\n").as_bytes()).is_err());
        }
        assert!(validate_entries(b"_internal/library\n").is_err());
        assert!(
            validate_types(b"-rwxr-xr-x  3.0 unx 42 bx 42 stor date time yt-dlp_macos\n").is_ok()
        );
        assert!(
            validate_types(b"lrwxr-xr-x  3.0 unx 42 bx 42 stor date time _internal/link\n")
                .is_err()
        );
    }

    fn activate_fixture(root: &Path) {
        let directory = staging(root).unwrap();
        fs::write(directory.path().join("yt-dlp_macos"), b"fixture").unwrap();
        activate(root, directory).unwrap();
    }

    #[test]
    fn updates_keep_in_use_libraries_and_collect_unused_bundles() {
        let root = tempfile::tempdir().unwrap();
        activate_fixture(root.path());
        let running = open(root.path()).unwrap();
        let original = running.executable.clone();
        activate_fixture(root.path());
        assert!(original.exists());
        let second = open(root.path()).unwrap().executable;
        assert_ne!(original, second);
        drop(running);
        activate_fixture(root.path());
        assert!(!original.exists());
        assert!(!second.exists());
        assert!(open(root.path()).unwrap().executable.is_file());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
