use super::*;
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use sha2::{Digest, Sha256};

fn encrypted(host: &str, value: &str, password: &[u8], version: u32, prefix: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    if version >= 24 {
        plain.extend(Sha256::digest(host.as_bytes()));
    }
    plain.extend(value.as_bytes());
    let mut key = [0; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
    let mut result = prefix.to_vec();
    result.extend(
        cbc::Encryptor::<aes::Aes128>::new((&key).into(), (&[b' '; 16]).into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plain),
    );
    result
}

fn chromium_db(path: &Path, version: u32) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); CREATE TABLE cookies(host_key TEXT,name TEXT,value TEXT,path TEXT,expires_utc INTEGER,is_secure INTEGER,encrypted_value BLOB);").unwrap();
    db.execute(
        "INSERT INTO meta VALUES('version',?1)",
        [version.to_string()],
    )
    .unwrap();
    db
}

fn insert(db: &Connection, host: &str, name: &str, value: &str, expiry: i64, encrypted: &[u8]) {
    db.execute(
        "INSERT INTO cookies VALUES(?1,?2,?3,'/',?4,1,?5)",
        rusqlite::params![host, name, value, expiry, encrypted],
    )
    .unwrap();
}

#[test]
fn chromium_reads_the_exact_profile_filters_domains_and_includes_wal_updates() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("Network/Cookies");
    let db = chromium_db(&path, 24);
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    insert(
        &db,
        ".youtube.com",
        "SAPISID",
        "",
        0,
        &encrypted(
            ".youtube.com",
            "synthetic-session",
            b"test-password",
            24,
            b"v11",
        ),
    );
    insert(&db, ".youtube.com", "EXPIRED", "expired", 1, &[]);
    insert(&db, ".example.com", "OTHER", "unrelated", 0, &[]);
    insert(&db, "youtube.com.evil", "OTHER", "unrelated", 0, &[]);
    let nested = chromium_db(&root.path().join("extension/Cookies"), 24);
    insert(&nested, ".youtube.com", "WRONG", "wrong-profile", 0, &[]);
    let selected = chromium_path(root.path()).unwrap();
    assert_eq!(selected, path);
    let (rows, version) = chromium(&selected, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(version, 24);
    let mut cookie = rows.into_iter().next().unwrap();
    cookie.cookie.value = crypto::cbc(
        &cookie.encrypted[3..],
        b"test-password",
        1,
        &cookie.cookie.host,
        version,
    )
    .unwrap();
    let text = netscape(vec![cookie.cookie], 10).unwrap();
    assert!(text.contains("SAPISID\tsynthetic-session"));
    assert!(!text.contains("unrelated"));
    assert!(!text.contains("expired"));
    assert!(!text.contains("wrong-profile"));
    // Neither reader inserted or modified a browser record.
    assert_eq!(
        db.query_row("SELECT count(*) FROM cookies", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
}

#[test]
fn plaintext_chromium_rows_do_not_require_a_keyring_or_blob_typed_defaults() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("Cookies");
    let db = chromium_db(&path, 23);
    db.execute_batch("INSERT INTO cookies VALUES('.youtube.com','SAPISID','synthetic','/',0,1,''),('.youtube.com','EMPTY','','/',0,1,''),('.youtube.com','NULL','','/',0,1,NULL);").unwrap();
    let (rows, _) = chromium(&path, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.encrypted.is_empty()));
    assert!(
        netscape(rows.into_iter().map(|r| r.cookie).collect(), 10)
            .unwrap()
            .contains("SAPISID\tsynthetic")
    );
}

#[test]
fn chromium_decryption_checks_padding_domain_hash_and_legacy_formats() {
    let data = encrypted(".youtube.com", "synthetic-secret", b"password", 24, b"v11");
    assert_eq!(
        &*crypto::cbc(&data[3..], b"password", 1, ".youtube.com", 24).unwrap(),
        "synthetic-secret"
    );
    for (password, host) in [
        (b"wrong".as_slice(), ".youtube.com"),
        (b"password".as_slice(), ".example.com"),
    ] {
        let error = crypto::cbc(&data[3..], password, 1, host, 24)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("synthetic-secret"));
    }
    assert!(crypto::cbc(&data[..8], b"password", 1, ".youtube.com", 24).is_err());
    let old = encrypted(".youtube.com", "legacy", b"peanuts", 23, b"v10");
    assert_eq!(
        &*crypto::cbc(&old[3..], b"peanuts", 1, ".youtube.com", 23).unwrap(),
        "legacy"
    );
}

#[test]
fn firefox_import_filters_domains_and_expiry_without_touching_nested_profiles() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cookies.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE moz_cookies(host TEXT,name TEXT,value TEXT,path TEXT,expiry INTEGER,isSecure INTEGER); INSERT INTO moz_cookies VALUES('.youtube.com','SAPISID','synthetic','/',0,1),('.youtube.com','OLD','expired','/',1,1),('youtube.com.evil','OTHER','unrelated','/',0,1);").unwrap();
    let text = netscape(firefox(&path, 10).unwrap(), 10).unwrap();
    assert!(text.contains("SAPISID\tsynthetic"));
    assert!(!text.contains("expired"));
    assert!(!text.contains("unrelated"));
    assert!(firefox(&root.path().join("missing.sqlite"), 10).is_err());
    assert!(!root.path().join("missing.sqlite").exists());
}

#[test]
fn firefox_import_reads_exclusively_locked_database_and_uncheckpointed_wal() {
    for wal in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cookies.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;").unwrap();
        if wal {
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
                .unwrap();
        }
        db.execute_batch("CREATE TABLE moz_cookies(host TEXT,name TEXT,value TEXT,path TEXT,expiry INTEGER,isSecure INTEGER); INSERT INTO moz_cookies VALUES('.youtube.com','SAPISID','synthetic-locked-session','/',0,1),('.example.com','OTHER','unrelated','/',0,1);").unwrap();
        assert!(
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap()
                .prepare("SELECT host FROM moz_cookies")
                .is_err()
        );
        let text = netscape(firefox(&path, 10).unwrap(), 10).unwrap();
        assert!(text.contains("SAPISID\tsynthetic-locked-session"));
        assert!(!text.contains("unrelated"));
        assert_eq!(
            db.query_row("SELECT count(*) FROM moz_cookies", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
}

#[test]
fn cookie_export_rejects_delimiter_injection_and_empty_sessions() {
    assert!(netscape(Vec::new(), 1).is_err());
    let cookie = Cookie {
        host: ".youtube.com".into(),
        name: "SAPISID".into(),
        value: Zeroizing::new("secret\nINJECTED".into()),
        path: "/".into(),
        expires: 0,
        secure: true,
    };
    let error = netscape(vec![cookie], 1).unwrap_err().to_string();
    assert!(!error.contains("secret"));
}

#[cfg(target_os = "linux")]
#[test]
fn helium_flags_and_explicit_keyrings_keep_their_precedence() {
    let config = tempfile::tempdir().unwrap();
    let mut selection = Selection {
        browser: "chromium".into(),
        profile: config.path().join("net.imput.helium/Profile 2"),
        keyring: None,
    };
    std::fs::write(
        config.path().join("chromium-flags.conf"),
        "# Comment\n--password-store='gnome-libsecret'\n",
    )
    .unwrap();
    assert_eq!(
        linux::configured_keyring(&selection, config.path()).as_deref(),
        Some("GNOMEKEYRING")
    );
    std::fs::write(
        config.path().join("helium-flags.conf"),
        "--password-store basic\n",
    )
    .unwrap();
    assert_eq!(
        linux::configured_keyring(&selection, config.path()).as_deref(),
        Some("BASICTEXT")
    );
    selection.keyring = Some("KWALLET6".into());
    assert_eq!(
        linux::configured_keyring(&selection, config.path()).as_deref(),
        Some("KWALLET6")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn keyring_identity_ignores_electron_and_rejects_ambiguous_matches() {
    let entry = |application: Option<&str>| {
        (
            "Chromium Safe Storage".into(),
            application
                .map(|s| [("application".into(), s.into())].into_iter().collect())
                .unwrap_or_default(),
        )
    };
    assert_eq!(
        linux::select_identity(
            &[entry(Some("t3code")), entry(Some("chromium"))],
            "Chromium"
        )
        .unwrap(),
        1
    );
    assert!(linux::select_identity(&[entry(Some("t3code"))], "Chromium").is_err());
    assert!(
        linux::select_identity(
            &[entry(Some("chromium")), entry(Some("chromium"))],
            "Chromium"
        )
        .is_err()
    );
    assert_eq!(
        linux::select_identity(&[entry(Some("t3code")), entry(None)], "Chromium").unwrap(),
        1
    );
    assert!(linux::select_identity(&[entry(None), entry(None)], "Chromium").is_err());
}

#[test]
fn safari_binary_parser_handles_expiry_and_rejects_truncated_records() {
    let strings = b".youtube.com\0SAPISID\0/\0synthetic\0";
    let mut record = vec![0; 56];
    for (offset, value) in [
        (0, 56 + strings.len()),
        (8, 1),
        (16, 56),
        (20, 69),
        (24, 77),
        (28, 79),
    ] {
        record[offset..offset + 4].copy_from_slice(&(value as u32).to_le_bytes());
    }
    record[40..48].copy_from_slice(&1000_f64.to_le_bytes());
    record.extend(strings);
    let mut page = vec![0, 0, 1, 0];
    page.extend(1u32.to_le_bytes());
    page.extend(16u32.to_le_bytes());
    page.extend([0; 4]);
    page.extend(record);
    let mut bytes = b"cook".to_vec();
    bytes.extend(1u32.to_be_bytes());
    bytes.extend((page.len() as u32).to_be_bytes());
    bytes.extend(page);
    let cookies = safari::parse(&bytes, 978_307_201).unwrap();
    assert_eq!(cookies.len(), 1);
    assert!(
        netscape(cookies, 978_307_201)
            .unwrap()
            .contains("SAPISID\tsynthetic")
    );
    assert!(safari::parse(&bytes, 978_308_201).unwrap().is_empty());
    assert!(safari::parse(&bytes[..bytes.len() - 4], 1).is_err());
    assert!(safari::parse(b"invalid", 1).is_err());
}
