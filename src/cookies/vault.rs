//! Pasted cookies, encrypted at rest. Each profile is one AES-256-GCM file in
//! the config folder; its key lives in the OS secret store, so the file alone
//! cannot be read.
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const VERSION: u8 = 1;
const NONCE: usize = 12;
const READ_ERROR: &str = "The saved cookies could not be read. Paste new cookies for this profile.";
const WRITE_ERROR: &str = "The pasted cookies could not be saved.";
#[cfg(target_os = "linux")]
const KEYRING_ERROR: &str = "Could not use the system keyring for pasted cookies. Make sure GNOME Keyring or KWallet is running and unlocked, then retry.";

/// Storage for the per-profile keys. Tests use an in-memory store.
pub(crate) trait KeyStore {
    async fn put(&self, id: &str, key: &[u8]) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>>;
    async fn remove(&self, id: &str) -> Result<()>;
}

pub fn folder() -> Result<PathBuf> {
    Ok(crate::config::Store::new()?.directory().join("cookies"))
}

/// The profile id of a vault file, or None for any path outside the folder.
pub fn id(path: &Path, folder: &Path) -> Option<String> {
    if path.parent()? != folder || path.extension()? != "bin" {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    (id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| id.to_owned())
}

/// A fresh, unused vault path. Reserving it before saving lets a caller that
/// gives up on a slow keyring still delete whatever was written for it.
pub fn reserve() -> Result<PathBuf> {
    Ok(reserve_in(&folder()?))
}

fn reserve_in(folder: &Path) -> PathBuf {
    let mut raw = [0u8; 16];
    OsRng.fill_bytes(&mut raw);
    let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    folder.join(format!("{id}.bin"))
}

pub async fn save(path: &Path, text: &str) -> Result<()> {
    let folder = folder()?;
    save_in(&folder, &OsKeys::new(&folder), path, text).await
}

pub async fn load(path: &Path) -> Result<Zeroizing<String>> {
    let folder = folder()?;
    load_in(&folder, &OsKeys::new(&folder), path).await
}

pub async fn delete(path: &Path) -> Result<()> {
    let folder = folder()?;
    delete_in(&folder, &OsKeys::new(&folder), path).await
}

async fn save_in(folder: &Path, keys: &impl KeyStore, path: &Path, text: &str) -> Result<()> {
    let id = id(path, folder).context(WRITE_ERROR)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(folder).context(WRITE_ERROR)?;
    let key = Zeroizing::new(Aes256Gcm::generate_key(OsRng).to_vec());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let sealed = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
        .encrypt(
            &nonce,
            Payload {
                msg: text.as_bytes(),
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!(WRITE_ERROR))?;
    keys.put(&id, &key).await?;
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&[VERSION])?;
        file.write_all(&nonce)?;
        file.write_all(&sealed)?;
        file.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(path);
        let _ = keys.remove(&id).await;
        bail!(WRITE_ERROR);
    }
    Ok(())
}

async fn load_in(folder: &Path, keys: &impl KeyStore, path: &Path) -> Result<Zeroizing<String>> {
    let id = id(path, folder).context(READ_ERROR)?;
    let data = fs::read(path).map_err(|_| anyhow::anyhow!(READ_ERROR))?;
    if data.len() <= 1 + NONCE || data[0] != VERSION {
        bail!(READ_ERROR);
    }
    let key = keys.get(&id).await?;
    if key.len() != 32 {
        bail!(READ_ERROR);
    }
    let plain = Zeroizing::new(
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
            .decrypt(
                Nonce::from_slice(&data[1..1 + NONCE]),
                Payload {
                    msg: &data[1 + NONCE..],
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!(READ_ERROR))?,
    );
    let text = std::str::from_utf8(&plain).map_err(|_| anyhow::anyhow!(READ_ERROR))?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Persist the pending key cleanup and erase the local cookies immediately.
/// This is synchronous so shutdown cannot discard it with a detached task.
pub fn prepare_delete(path: &Path) -> Result<()> {
    prepare_delete_in(&folder()?, path)
}

fn prepare_delete_in(folder: &Path, path: &Path) -> Result<()> {
    id(path, folder).context("This is not a saved cookie profile.")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(folder)
        .context("Could not record cookie cleanup.")?;
    let marker = path.with_extension("delete");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(marker) {
        Ok(file) => file
            .sync_all()
            .context("Could not record cookie cleanup.")?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => bail!("Could not record cookie cleanup."),
    }
    remove_file(path)
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => bail!("The saved cookie file could not be deleted."),
    }
}

/// A missing file or key is not an error. The marker survives a failed or
/// cancelled keyring operation, so a later launch can finish the cleanup.
async fn delete_in(folder: &Path, keys: &impl KeyStore, path: &Path) -> Result<()> {
    let id = id(path, folder).context("This is not a saved cookie profile.")?;
    prepare_delete_in(folder, path)?;
    keys.remove(&id).await?;
    remove_file(&path.with_extension("delete"))
}

fn pending_paths(folder: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "delete"))
        .map(|path| path.with_extension("bin"))
        .filter(|path| id(path, folder).is_some())
        .collect())
}

/// Runs independently of startup. Only explicit deletion markers are retried;
/// saved profiles and unrelated files are never swept.
pub async fn retry_pending() {
    let Ok(folder) = folder() else {
        return;
    };
    let Ok(paths) = pending_paths(&folder) else {
        return;
    };
    let keys = OsKeys::new(&folder);
    for path in paths {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            delete_in(&folder, &keys, &path),
        )
        .await;
    }
}

struct OsKeys {
    #[cfg(windows)]
    folder: PathBuf,
}

impl OsKeys {
    fn new(folder: &Path) -> Self {
        let _ = folder;
        Self {
            #[cfg(windows)]
            folder: folder.to_path_buf(),
        }
    }
}

#[cfg(target_os = "linux")]
impl KeyStore for OsKeys {
    async fn put(&self, id: &str, key: &[u8]) -> Result<()> {
        use secret_service::{EncryptionType, SecretService};
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        let collection = service
            .get_default_collection()
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        if collection.is_locked().await.unwrap_or(true) {
            collection
                .unlock()
                .await
                .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        }
        collection
            .create_item(
                "SlintYT pasted cookies",
                attributes(id),
                key,
                true,
                "application/octet-stream",
            )
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
        use secret_service::{EncryptionType, SecretService};
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        let found = service
            .search_items(attributes(id))
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        let item = match (found.unlocked.first(), found.locked.first()) {
            (Some(item), _) => item,
            (None, Some(item)) => {
                item.unlock()
                    .await
                    .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
                item
            }
            (None, None) => bail!(READ_ERROR),
        };
        item.get_secret()
            .await
            .map(Zeroizing::new)
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))
    }

    async fn remove(&self, id: &str) -> Result<()> {
        use secret_service::{EncryptionType, SecretService};
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        let found = service
            .search_items(attributes(id))
            .await
            .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        for item in found.unlocked.iter().chain(&found.locked) {
            item.delete()
                .await
                .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn attributes(id: &str) -> std::collections::HashMap<&str, &str> {
    std::collections::HashMap::from([("application", "slintyt"), ("slintyt-profile", id)])
}

#[cfg(target_os = "macos")]
impl KeyStore for OsKeys {
    async fn put(&self, id: &str, key: &[u8]) -> Result<()> {
        let (account, key) = (format!("cookies-{id}"), Zeroizing::new(key.to_vec()));
        tokio::task::spawn_blocking(move || {
            security_framework::passwords::set_generic_password("SlintYT", &account, &key)
        })
        .await
        .context(WRITE_ERROR)?
        .map_err(|_| anyhow::anyhow!("Could not save the key in your macOS Keychain."))
    }

    async fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let account = format!("cookies-{id}");
        tokio::task::spawn_blocking(move || {
            security_framework::passwords::get_generic_password("SlintYT", &account)
        })
        .await
        .context(READ_ERROR)?
        .map(Zeroizing::new)
        .map_err(|_| anyhow::anyhow!("Could not read the key from your macOS Keychain."))
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let account = format!("cookies-{id}");
        let result = tokio::task::spawn_blocking(move || {
            security_framework::passwords::delete_generic_password("SlintYT", &account)
        })
        .await
        .context(WRITE_ERROR)?;
        match result {
            Ok(()) => Ok(()),
            // errSecItemNotFound: already gone.
            Err(e) if e.code() == -25300 => Ok(()),
            Err(_) => bail!("Could not remove the key from your macOS Keychain."),
        }
    }
}

/// Windows keeps the key next to the file, sealed with DPAPI to the OS user.
#[cfg(windows)]
impl KeyStore for OsKeys {
    async fn put(&self, id: &str, key: &[u8]) -> Result<()> {
        let sealed = super::windows::protect(key)?;
        fs::write(self.folder.join(format!("{id}.key")), sealed).context(WRITE_ERROR)
    }

    async fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let sealed = fs::read(self.folder.join(format!("{id}.key")))
            .map_err(|_| anyhow::anyhow!(READ_ERROR))?;
        super::windows::unprotect(&sealed).map_err(|_| anyhow::anyhow!(READ_ERROR))
    }

    async fn remove(&self, id: &str) -> Result<()> {
        match fs::remove_file(self.folder.join(format!("{id}.key"))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                bail!("The saved cookie key could not be deleted.")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl KeyStore for OsKeys {
    async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
        bail!("Pasted cookies are not supported on this operating system.")
    }
    async fn get(&self, _: &str) -> Result<Zeroizing<Vec<u8>>> {
        bail!("Pasted cookies are not supported on this operating system.")
    }
    async fn remove(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    struct MemoryKeys(Mutex<HashMap<String, Vec<u8>>>);

    impl KeyStore for MemoryKeys {
        async fn put(&self, id: &str, key: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(id.into(), key.to_vec());
            Ok(())
        }
        async fn get(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
            let keys = self.0.lock().unwrap();
            keys.get(id)
                .cloned()
                .map(Zeroizing::new)
                .context(READ_ERROR)
        }
        async fn remove(&self, id: &str) -> Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    async fn saved(folder: &Path, keys: &impl KeyStore) -> PathBuf {
        let path = reserve_in(folder);
        save_in(folder, keys, &path, TEXT).await.unwrap();
        path
    }

    const TEXT: &str =
        "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsynthetic-secret\n";

    #[tokio::test]
    async fn saved_cookies_round_trip_and_are_not_readable_on_disk() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("cookies");
        let keys = MemoryKeys::default();
        let path = saved(&folder, &keys).await;
        assert!(id(&path, &folder).is_some());
        let disk = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&disk).contains("synthetic-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(load_in(&folder, &keys, &path).await.unwrap().as_str(), TEXT);
        // Two saves never share an id or key.
        let other = saved(&folder, &keys).await;
        assert_ne!(path, other);
        assert_eq!(keys.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn tampered_moved_or_unkeyed_files_fail_without_leaking() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("cookies");
        let keys = MemoryKeys::default();
        let path = saved(&folder, &keys).await;
        let mut data = fs::read(&path).unwrap();
        let last = data.len() - 1;
        data[last] ^= 1;
        fs::write(&path, &data).unwrap();
        let error = load_in(&folder, &keys, &path)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("synthetic"));
        // A valid file renamed to another id fails the AAD check.
        let path = saved(&folder, &keys).await;
        let renamed = folder.join(format!("{}.bin", "0".repeat(32)));
        fs::rename(&path, &renamed).unwrap();
        let moved_key = keys
            .0
            .lock()
            .unwrap()
            .get(&id(&path, &folder).unwrap())
            .cloned()
            .unwrap();
        keys.0.lock().unwrap().insert("0".repeat(32), moved_key);
        assert!(load_in(&folder, &keys, &renamed).await.is_err());
        // Files outside the folder or with other names are never opened.
        for path in [
            root.path().join("x.bin"),
            folder.join("NOT-HEX.bin"),
            folder.join("a.txt"),
        ] {
            assert!(id(&path, &folder).is_none());
            assert!(load_in(&folder, &keys, &path).await.is_err());
        }
        let path = saved(&folder, &keys).await;
        keys.0.lock().unwrap().clear();
        assert!(load_in(&folder, &keys, &path).await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_both_parts_and_tolerates_missing_ones() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("cookies");
        let keys = MemoryKeys::default();
        let path = saved(&folder, &keys).await;
        delete_in(&folder, &keys, &path).await.unwrap();
        assert!(!path.exists());
        assert!(keys.0.lock().unwrap().is_empty());
        delete_in(&folder, &keys, &path).await.unwrap();
        assert!(
            delete_in(&folder, &keys, &root.path().join("x.bin"))
                .await
                .is_err()
        );
    }

    struct StalledKeys;
    impl KeyStore for StalledKeys {
        async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        async fn get(&self, _: &str) -> Result<Zeroizing<Vec<u8>>> {
            unreachable!()
        }
        async fn remove(&self, _: &str) -> Result<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn stalled_cleanup_erases_cookies_and_can_resume_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("cookies");
        let keys = MemoryKeys::default();
        let path = saved(&folder, &keys).await;
        let other = saved(&folder, &keys).await;
        // Erasure and the retry marker exist even if the task never starts.
        prepare_delete_in(&folder, &path).unwrap();
        assert!(!path.exists());
        assert_eq!(
            pending_paths(&folder).unwrap().as_slice(),
            std::slice::from_ref(&path)
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                delete_in(&folder, &StalledKeys, &path)
            )
            .await
            .is_err()
        );
        assert!(!path.exists());
        assert!(path.with_extension("delete").exists());
        // Simulate the next launch with a responsive keyring.
        for pending in pending_paths(&folder).unwrap() {
            delete_in(&folder, &keys, &pending).await.unwrap();
        }
        assert!(pending_paths(&folder).unwrap().is_empty());
        assert!(
            !keys
                .0
                .lock()
                .unwrap()
                .contains_key(&id(&path, &folder).unwrap())
        );
        assert_eq!(
            load_in(&folder, &keys, &other).await.unwrap().as_str(),
            TEXT
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_file_write_leaves_no_key_behind() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        // The key is stored first, then the read-only folder rejects the file.
        let folder = root.path().join("cookies");
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
        let keys = MemoryKeys::default();
        let result = save_in(&folder, &keys, &reserve_in(&folder), TEXT).await;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(keys.0.lock().unwrap().is_empty());
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "writes and deletes one item in the real OS keychain"]
    async fn vault_round_trips_through_os_keychain() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("cookies");
        let keys = OsKeys::new(&folder);
        let path = saved(&folder, &keys).await;
        assert_eq!(load_in(&folder, &keys, &path).await.unwrap().as_str(), TEXT);
        delete_in(&folder, &keys, &path).await.unwrap();
        assert!(load_in(&folder, &keys, &path).await.is_err());
    }
}
