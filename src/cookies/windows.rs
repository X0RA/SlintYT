use super::{DECRYPT_ERROR, crypto};
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::path::Path;
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData},
};
use zeroize::{Zeroize, Zeroizing};

fn unprotect(data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let size = u32::try_from(data.len()).context(DECRYPT_ERROR)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: size,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // DPAPI authenticates the calling OS user; no UI or elevation is requested.
    let success = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if success == 0 {
        bail!(DECRYPT_ERROR);
    }
    if output.pbData.is_null() {
        if output.cbData == 0 {
            return Ok(Zeroizing::new(Vec::new()));
        }
        bail!(DECRYPT_ERROR);
    }
    let result = unsafe {
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let copy = Zeroizing::new(bytes.to_vec());
        bytes.zeroize();
        LocalFree(output.pbData.cast());
        copy
    };
    Ok(result)
}

pub(super) fn key(profile: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let path = [
        profile.join("Local State"),
        profile.parent().unwrap_or(profile).join("Local State"),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .context("The browser's Local State file is missing.")?;
    let data = Zeroizing::new(std::fs::read(path).map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))?);
    let state: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))?;
    let encrypted = state
        .pointer("/os_crypt/encrypted_key")
        .and_then(serde_json::Value::as_str)
        .context(DECRYPT_ERROR)?;
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encrypted)
            .map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))?,
    );
    unprotect(decoded.strip_prefix(b"DPAPI").context(DECRYPT_ERROR)?)
}

pub(super) fn decrypt(
    data: &[u8],
    key: Option<&[u8]>,
    host: &str,
    version: u32,
) -> Result<Zeroizing<String>> {
    if data.starts_with(b"v20") {
        bail!(
            "This browser uses Windows App-Bound Encryption. Choose a supported profile such as Firefox; SlintYT cannot decrypt this cookie format."
        );
    }
    if data.starts_with(b"v10") {
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
        let cipher = Aes256Gcm::new_from_slice(key.context(DECRYPT_ERROR)?)
            .map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))?;
        let nonce = data.get(3..15).context(DECRYPT_ERROR)?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                data.get(15..).context(DECRYPT_ERROR)?,
            )
            .map_err(|_| anyhow::anyhow!(DECRYPT_ERROR))?;
        crypto::plaintext(Zeroizing::new(plaintext), host, version)
    } else {
        crypto::plaintext(unprotect(data)?, host, 0)
    }
}
