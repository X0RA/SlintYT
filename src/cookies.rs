//! Native browser cookie import. Only the exact selected store and YouTube
//! domains are read; decrypted values never leave memory or enter diagnostics.
mod crypto;
#[cfg(target_os = "linux")]
mod linux;
mod safari;
#[cfg(test)]
mod tests;
#[cfg(windows)]
mod windows;

use crate::browser::Selection;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{path::Path, time::Duration};
use zeroize::Zeroizing;

const DATABASE_ERROR: &str =
    "Could not read this profile's cookie database. Close the browser if it is locked, then retry.";
const DECRYPT_ERROR: &str = "The browser cookies could not be decrypted. Check the selected profile and cookie keyring, then retry.";

struct Cookie {
    host: String,
    name: String,
    value: Zeroizing<String>,
    path: String,
    expires: i64,
    secure: bool,
}

struct EncryptedCookie {
    cookie: Cookie,
    encrypted: Zeroizing<Vec<u8>>,
}

pub async fn read(selection: &Selection) -> Result<String> {
    selection.validate()?;
    tokio::time::timeout(Duration::from_secs(90), read_inner(selection))
        .await
        .context("Reading browser cookies timed out. Unlock your keyring and retry.")?
}

async fn read_inner(selection: &Selection) -> Result<String> {
    let selected = selection.clone();
    let now = crate::history::now() as i64;
    if selection.browser == "firefox" || selection.browser == "safari" {
        let cookies = tokio::task::spawn_blocking(move || {
            if selected.browser == "firefox" {
                firefox(&selected.profile.join("cookies.sqlite"), now)
            } else {
                safari::read(&selected.profile, now)
            }
        })
        .await
        .context(DATABASE_ERROR)??;
        return netscape(cookies, now);
    }
    let (rows, version) =
        tokio::task::spawn_blocking(move || chromium(&chromium_path(&selected.profile)?, now))
            .await
            .context(DATABASE_ERROR)??;
    let key = chromium_key(selection, &rows).await?;
    let cookies = rows
        .into_iter()
        .map(|mut row| {
            if !row.encrypted.is_empty() {
                row.cookie.value = decrypt(
                    &row.encrypted,
                    key.as_deref().map(|k| k.as_slice()),
                    &row.cookie.host,
                    version,
                )?;
            }
            Ok(row.cookie)
        })
        .collect::<Result<Vec<_>>>()?;
    netscape(cookies, now)
}

fn youtube_host(host: &str) -> bool {
    matches!(
        host.strip_prefix('.').unwrap_or(host),
        "youtube.com" | "www.youtube.com"
    )
}

fn chromium_path(profile: &Path) -> Result<std::path::PathBuf> {
    [profile.join("Network/Cookies"), profile.join("Cookies")]
        .into_iter()
        .find(|p| p.is_file())
        .context(DATABASE_ERROR)
}

fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| anyhow::anyhow!(DATABASE_ERROR))?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| anyhow::anyhow!(DATABASE_ERROR))?;
    Ok(connection)
}

fn firefox(path: &Path, now: i64) -> Result<Vec<Cookie>> {
    with_database(path, |connection| {
        let mut query = connection.prepare("SELECT host,name,value,path,expiry,isSecure FROM moz_cookies WHERE host IN ('youtube.com','.youtube.com','www.youtube.com','.www.youtube.com') AND (expiry = 0 OR expiry > ?1)")?;
        query
            .query_map([now], |row| {
                Ok(Cookie {
                    host: row.get(0)?,
                    name: row.get(1)?,
                    value: Zeroizing::new(row.get(2)?),
                    path: row.get(3)?,
                    expires: row.get(4)?,
                    secure: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
    })
}

fn chromium(path: &Path, now: i64) -> Result<(Vec<EncryptedCookie>, u32)> {
    with_database(path, |connection| {
        let transaction = connection.transaction()?;
        let version: Option<String> = transaction
            .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let version = version.and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut query = transaction.prepare("SELECT host_key,name,value,path,expires_utc,is_secure,encrypted_value FROM cookies WHERE host_key IN ('youtube.com','.youtube.com','www.youtube.com','.www.youtube.com') AND (expires_utc = 0 OR expires_utc > ?1)")?;
        let rows = query
            .query_map([(now + 11_644_473_600) * 1_000_000], |row| {
                let expires: i64 = row.get(4)?;
                let value = Zeroizing::new(row.get::<_, String>(2)?);
                // Chromium's BLOB column historically defaults to SQL text ''.
                // Plaintext rows need no keyring, including legacy mixed rows.
                let encrypted = if !value.is_empty() {
                    Vec::new()
                } else {
                    use rusqlite::types::ValueRef;
                    match row.get_ref(6)? {
                        ValueRef::Blob(bytes) => bytes.to_vec(),
                        ValueRef::Text([]) | ValueRef::Null => Vec::new(),
                        other => {
                            return Err(rusqlite::Error::InvalidColumnType(
                                6,
                                "encrypted_value".into(),
                                other.data_type(),
                            ));
                        }
                    }
                };
                Ok(EncryptedCookie {
                    cookie: Cookie {
                        host: row.get(0)?,
                        name: row.get(1)?,
                        value,
                        path: row.get(3)?,
                        expires: if expires == 0 {
                            0
                        } else {
                            expires / 1_000_000 - 11_644_473_600
                        },
                        secure: row.get(5)?,
                    },
                    encrypted: Zeroizing::new(encrypted),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, version))
    })
}

fn with_database<T>(
    path: &Path,
    read: impl Fn(&mut Connection) -> rusqlite::Result<T>,
) -> Result<T> {
    let mut connection = open(path)?;
    match read(&mut connection) {
        Ok(value) => return Ok(value),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) => {}
        Err(_) => bail!(DATABASE_ERROR),
    }
    drop(connection);

    read_snapshot(path, read, |from, to| std::fs::copy(from, to))
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn file_stamp(path: &Path) -> std::io::Result<Option<FileStamp>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(FileStamp {
        length: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    }))
}

fn read_snapshot<T>(
    path: &Path,
    read: impl Fn(&mut Connection) -> rusqlite::Result<T>,
    copy: impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<T> {
    // Firefox forks can hold an exclusive SQLite lock. Check both source files
    // before and after copying to detect writes/checkpoints, including a WAL
    // being replaced or removed. Retry with a fresh directory and SHM state.
    let mut wal_name = path.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal = std::path::PathBuf::from(wal_name);
    for _ in 0..3 {
        let copied = (|| -> Result<T> {
            let directory = tempfile::tempdir()?;
            let snapshot = directory.path().join("Cookies");
            let before = (file_stamp(path)?, file_stamp(&wal)?);
            copy(path, &snapshot)?;
            if before.1.is_some() {
                copy(&wal, &directory.path().join("Cookies-wal"))?;
            }
            let after = (file_stamp(path)?, file_stamp(&wal)?);
            if before != after {
                bail!(DATABASE_ERROR);
            }
            let mut connection = open(&snapshot)?;
            Ok(read(&mut connection)?)
        })();
        if let Ok(value) = copied {
            return Ok(value);
        }
    }
    bail!(DATABASE_ERROR)
}

async fn chromium_key(
    selection: &Selection,
    rows: &[EncryptedCookie],
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    #[cfg(target_os = "linux")]
    if rows.iter().any(|r| r.encrypted.starts_with(b"v11")) {
        return linux::password(selection).await.map(Some);
    }
    #[cfg(target_os = "macos")]
    if rows.iter().any(|r| r.encrypted.starts_with(b"v10")) {
        let name = keyring_name(&selection.browser);
        let password = tokio::task::spawn_blocking(move || {
            security_framework::passwords::get_generic_password(
                &format!("{name} Safe Storage"),
                name,
            )
        })
        .await
        .context(DECRYPT_ERROR)?
        .map_err(|_| anyhow::anyhow!("Could not access this browser's macOS Keychain entry."))?;
        return Ok(Some(Zeroizing::new(password)));
    }
    #[cfg(windows)]
    if rows.iter().any(|r| r.encrypted.starts_with(b"v10")) {
        let profile = selection.profile.clone();
        return tokio::task::spawn_blocking(move || windows::key(&profile))
            .await
            .context(DECRYPT_ERROR)?
            .map(Some);
    }
    let _ = (selection, rows);
    Ok(None)
}

fn decrypt(data: &[u8], key: Option<&[u8]>, host: &str, version: u32) -> Result<Zeroizing<String>> {
    #[cfg(target_os = "linux")]
    {
        let password = if data.starts_with(b"v10") {
            b"peanuts".as_slice()
        } else if data.starts_with(b"v11") {
            key.context(DECRYPT_ERROR)?
        } else {
            bail!("This browser uses an unsupported cookie encryption version.");
        };
        crypto::cbc(&data[3..], password, 1, host, version)
            .or_else(|_| crypto::cbc(&data[3..], b"", 1, host, version))
    }
    #[cfg(target_os = "macos")]
    {
        if data.starts_with(b"v10") {
            return crypto::cbc(&data[3..], key.context(DECRYPT_ERROR)?, 1003, host, version);
        }
        if data.starts_with(b"v20") {
            bail!(DECRYPT_ERROR);
        }
        String::from_utf8(data.to_vec())
            .map(Zeroizing::new)
            .map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))
    }
    #[cfg(windows)]
    {
        windows::decrypt(data, key, host, version)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (data, key, host, version);
        bail!("Encrypted browser cookies are not supported on this operating system.");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn keyring_name(browser: &str) -> &'static str {
    match browser {
        "brave" => "Brave",
        "chrome" => "Chrome",
        "whale" => "Whale",
        "edge" if cfg!(target_os = "macos") => "Microsoft Edge",
        "opera" if cfg!(target_os = "macos") => "Opera",
        "vivaldi" if cfg!(target_os = "macos") => "Vivaldi",
        "vivaldi" => "Chrome",
        _ => "Chromium",
    }
}

fn netscape(cookies: Vec<Cookie>, now: i64) -> Result<String> {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    let mut count = 0;
    for cookie in cookies {
        if !youtube_host(&cookie.host) || (cookie.expires != 0 && cookie.expires <= now) {
            continue;
        }
        let fields = [&cookie.host, &cookie.name, &cookie.value, &cookie.path];
        if fields
            .iter()
            .any(|s| s.chars().any(|c| matches!(c, '\t' | '\n' | '\r' | '\0')))
        {
            bail!("The browser returned invalid cookie data.");
        }
        use std::fmt::Write;
        writeln!(
            text,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            cookie.host,
            if cookie.host.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            },
            if cookie.path.is_empty() {
                "/"
            } else {
                &cookie.path
            },
            if cookie.secure { "TRUE" } else { "FALSE" },
            cookie.expires,
            cookie.name,
            cookie.value.as_str()
        )?;
        count += 1;
    }
    if count == 0 {
        bail!("No YouTube cookies found. Sign in to YouTube in this profile, then retry.");
    }
    Ok(text)
}
