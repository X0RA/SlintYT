//! Small, size-limited HTTPS downloads from GitHub release endpoints.
use anyhow::{Result, bail};
use std::time::Duration;

/// Names the running release, or `dev` for development builds.
pub fn user_agent() -> String {
    format!("SlintYT/{}", crate::updates::VERSION.unwrap_or("dev"))
}

/// Reads at most `limit` bytes. Small metadata requests get a short timeout.
pub async fn fetch(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
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
