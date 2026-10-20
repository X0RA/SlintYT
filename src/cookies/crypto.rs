use anyhow::{Result, bail};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(super) fn plaintext(
    mut bytes: Zeroizing<Vec<u8>>,
    host: &str,
    version: u32,
) -> Result<Zeroizing<String>> {
    if version >= 24 {
        let hash = Sha256::digest(host.as_bytes());
        if bytes.get(..32) != Some(hash.as_slice()) {
            bail!(super::DECRYPT_ERROR);
        }
        bytes.drain(..32);
    }
    let value = std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!(super::DECRYPT_ERROR))?;
    Ok(Zeroizing::new(value.to_owned()))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(super) fn cbc(
    data: &[u8],
    password: &[u8],
    rounds: u32,
    host: &str,
    version: u32,
) -> Result<Zeroizing<String>> {
    let mut key = Zeroizing::new([0u8; 16]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", rounds, key.as_mut());
    let bytes = cbc::Decryptor::<aes::Aes128>::new(key.as_ref().into(), (&[b' '; 16]).into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| anyhow::anyhow!(super::DECRYPT_ERROR))?;
    plaintext(Zeroizing::new(bytes), host, version)
}
