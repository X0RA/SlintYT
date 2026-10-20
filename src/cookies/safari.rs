use super::{Cookie, DATABASE_ERROR, youtube_host};
use anyhow::{Context, Result, bail};
use std::{io::Read, path::Path};
use zeroize::Zeroizing;

pub(super) fn read(path: &Path, now: i64) -> Result<Vec<Cookie>> {
    let file = std::fs::File::open(path).map_err(|_| anyhow::anyhow!(DATABASE_ERROR))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!(DATABASE_ERROR))?;
    if bytes.len() > 64 * 1024 * 1024 {
        bail!(DATABASE_ERROR);
    }
    parse(&bytes, now)
}

fn part(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8]> {
    bytes
        .get(offset..offset.checked_add(length).context(DATABASE_ERROR)?)
        .context(DATABASE_ERROR)
}
fn number(bytes: &[u8], offset: usize, big_endian: bool) -> Result<usize> {
    let raw: [u8; 4] = part(bytes, offset, 4)?.try_into()?;
    Ok(if big_endian {
        u32::from_be_bytes(raw)
    } else {
        u32::from_le_bytes(raw)
    } as usize)
}
fn string(bytes: &[u8], offset: usize) -> Result<&str> {
    let tail = bytes.get(offset..).context(DATABASE_ERROR)?;
    let length = tail.iter().position(|b| *b == 0).context(DATABASE_ERROR)?;
    std::str::from_utf8(&tail[..length]).map_err(|_| anyhow::anyhow!(DATABASE_ERROR))
}

pub(super) fn parse(bytes: &[u8], now: i64) -> Result<Vec<Cookie>> {
    if part(bytes, 0, 4)? != b"cook" {
        bail!(DATABASE_ERROR);
    }
    let pages = number(bytes, 4, true)?;
    let mut offset = pages
        .checked_mul(4)
        .and_then(|n| n.checked_add(8))
        .context(DATABASE_ERROR)?;
    let mut cookies = Vec::new();
    for page_index in 0..pages {
        let size = number(bytes, 8 + page_index * 4, true)?;
        let page = part(bytes, offset, size)?;
        offset += size;
        if part(page, 0, 4)? != [0, 0, 1, 0] {
            bail!(DATABASE_ERROR);
        }
        let count = number(page, 4, false)?;
        for index in 0..count {
            let start = number(page, 8 + index * 4, false)?;
            let length = number(page, start, false)?;
            let record = part(page, start, length)?;
            let host = string(record, number(record, 16, false)?)?;
            if !youtube_host(host) {
                continue;
            }
            let expiry = f64::from_le_bytes(part(record, 40, 8)?.try_into()?) + 978_307_200.;
            if !expiry.is_finite() {
                bail!(DATABASE_ERROR);
            }
            if expiry <= now as f64 {
                continue;
            }
            cookies.push(Cookie {
                host: host.into(),
                name: string(record, number(record, 20, false)?)?.into(),
                path: string(record, number(record, 24, false)?)?.into(),
                value: Zeroizing::new(string(record, number(record, 28, false)?)?.into()),
                expires: expiry as i64,
                secure: number(record, 8, false)? & 1 != 0,
            });
        }
    }
    Ok(cookies)
}
