//! Bounded recovery for read requests. Never retry account mutations here.
use anyhow::{Result, bail};
use serde_json::Value;

use crate::{browser::Selection, history::Account};

#[derive(Debug)]
pub(crate) struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The imported YouTube session has expired. Reconnect your browser profile; if that fails, sign in again in your browser.")
    }
}

impl std::error::Error for SessionExpired {}

pub(super) fn check_response(response: &Value) -> Result<()> {
    if response
        .pointer("/responseContext/mainAppWebResponseContext/loggedOut")
        .and_then(Value::as_bool)
        == Some(true)
        || response.pointer("/error/code").and_then(Value::as_u64) == Some(401)
    {
        return Err(SessionExpired.into());
    }
    Ok(())
}

/// Preserve only the authentication classification, not server response text.
pub(super) fn request_error(
    error: rustypipe::error::Error,
    message: &'static str,
) -> anyhow::Error {
    if matches!(
        error,
        rustypipe::error::Error::Auth(rustypipe::error::AuthError::NoLogin)
            | rustypipe::error::Error::HttpStatus(401, _)
    ) {
        SessionExpired.into()
    } else {
        anyhow::anyhow!(message)
    }
}

pub(super) fn check_source(selection: &Selection) -> Result<()> {
    if selection.pasted() {
        bail!(
            "The pasted YouTube session has expired. Replace this profile's saved cookies with a fresh export."
        );
    }
    Ok(())
}

pub(super) fn check_account(previous: Option<&Account>, refreshed: Option<&Account>) -> Result<()> {
    if previous.is_none() || previous != refreshed {
        bail!(
            "The browser's YouTube account or channel has changed or could not be verified. Reconnect the profile to choose the account explicitly."
        );
    }
    Ok(())
}

/// The worker owns one cancellable load at a time and publishes a replacement
/// only if its navigation generation is still current. Return the replacement
/// even if the retry fails, so later requests don't reuse the expired session.
pub(crate) async fn retry_once<S: Clone, T, Load, Refresh>(
    session: S,
    load: impl Fn(S) -> Load,
    refresh: impl FnOnce() -> Refresh,
) -> (Option<S>, Result<T>)
where
    Load: std::future::Future<Output = Result<T>>,
    Refresh: std::future::Future<Output = Result<S>>,
{
    let result = load(session).await;
    if !result
        .as_ref()
        .is_err_and(|error| error.is::<SessionExpired>())
    {
        return (None, result);
    }
    match refresh().await {
        Ok(session) => {
            let result = load(session.clone()).await;
            (Some(session), result)
        }
        Err(error) => (None, Err(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn recovery_classifies_only_explicit_authentication_failures() {
        for response in [
            r#"{"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}"#,
            r#"{"error":{"code":401}}"#,
        ] {
            let error = crate::youtube::parse_home(response).err().unwrap();
            assert!(error.is::<SessionExpired>());
        }
        for response in [
            r#"{"error":{"code":403}}"#,
            r#"{"error":{"code":429}}"#,
            r#"{"error":{"code":500}}"#,
            "not json",
        ] {
            let error = crate::youtube::parse_home(response).err().unwrap();
            assert!(!error.is::<SessionExpired>());
        }
        for code in [401, 403, 429, 500] {
            let error = request_error(
                rustypipe::error::Error::HttpStatus(code, "sensitive server text".into()),
                "Request failed",
            );
            assert_eq!(error.is::<SessionExpired>(), code == 401);
            assert!(!error.to_string().contains("sensitive"));
        }
    }

    #[tokio::test]
    async fn recovery_retries_with_the_replacement_session() {
        let loads = AtomicUsize::new(0);
        let refreshes = AtomicUsize::new(0);
        let (replacement, result) = retry_once(
            1,
            async |session| {
                loads.fetch_add(1, Ordering::Relaxed);
                if session == 1 {
                    Err(anyhow::Error::from(SessionExpired).context("Loading Home"))
                } else {
                    Ok(session)
                }
            },
            async || {
                refreshes.fetch_add(1, Ordering::Relaxed);
                Ok(2)
            },
        )
        .await;
        assert_eq!(replacement, Some(2));
        assert_eq!(result.unwrap(), 2);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn recovery_stops_after_one_retry_and_keeps_new_credentials() {
        let loads = AtomicUsize::new(0);
        let refreshes = AtomicUsize::new(0);
        let (replacement, result) = retry_once(
            1,
            async |_| {
                loads.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(SessionExpired.into())
            },
            async || {
                refreshes.fetch_add(1, Ordering::Relaxed);
                Ok(2)
            },
        )
        .await;
        assert_eq!(replacement, Some(2));
        assert!(result.unwrap_err().is::<SessionExpired>());
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn recovery_does_not_refresh_success_or_network_failures() {
        for success in [true, false] {
            let (replacement, result) = retry_once(
                1,
                async |_| {
                    if success {
                        Ok(())
                    } else {
                        bail!("Network unavailable")
                    }
                },
                async || -> Result<i32> { panic!("Must not refresh") },
            )
            .await;
            assert!(replacement.is_none());
            assert_eq!(result.is_ok(), success);
        }
    }

    #[tokio::test]
    async fn recovery_preserves_refresh_failure_without_retrying() {
        let loads = AtomicUsize::new(0);
        let (replacement, result) = retry_once(
            1,
            async |_| {
                loads.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(SessionExpired.into())
            },
            async || bail!("Keyring locked"),
        )
        .await;
        assert!(replacement.is_none());
        assert_eq!(result.unwrap_err().to_string(), "Keyring locked");
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn recovery_rejects_pasted_profiles_and_changed_or_unknown_accounts() {
        let pasted = Selection::pasted_file("/example/cookies.bin".into());
        assert!(
            check_source(&pasted)
                .unwrap_err()
                .to_string()
                .contains("fresh export")
        );
        let account = |user: &str, channel: Option<&str>| -> Account {
            serde_json::from_value(serde_json::json!({
                "user_syncid": user, "channel_syncid": channel
            }))
            .unwrap()
        };
        let original = account("user-a", None);
        let other = account("user-b", None);
        let channel = account("user-a", Some("channel-b"));
        assert!(check_account(Some(&original), Some(&original)).is_ok());
        for refreshed in [Some(&other), Some(&channel), None] {
            assert!(check_account(Some(&original), refreshed).is_err());
        }
        assert!(check_account(None, None).is_err());
    }

    #[tokio::test]
    async fn recovery_can_be_cancelled_without_retrying_a_stale_request() {
        let loads = AtomicUsize::new(0);
        let (started, wait) = tokio::sync::oneshot::channel();
        let mut recovery = Box::pin(retry_once(
            1,
            async |_| {
                loads.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(SessionExpired.into())
            },
            async || {
                let _ = started.send(());
                std::future::pending::<Result<i32>>().await
            },
        ));
        tokio::select! {
            _ = &mut recovery => panic!("Refresh should still be pending"),
            _ = wait => {},
        }
        drop(recovery);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }
}
