use super::{Selection, keyring_name};
use anyhow::{Context, Result, bail};
use std::{collections::HashMap, path::Path};
use zeroize::Zeroizing;

const KEYRING_ERROR: &str =
    "Could not access the browser keyring. Unlock it or choose the matching keyring, then retry.";
const LOCKED: &str =
    "The browser's keyring is locked. Unlock it in your keyring manager, then retry.";
const AMBIGUOUS: &str = "Could not identify a unique keyring entry for this browser. Check that the selected browser and keyring match the profile.";

pub(super) fn configured_keyring(selection: &Selection, config: &Path) -> Option<String> {
    if let Some(keyring) = &selection.keyring {
        return Some(keyring.clone());
    }
    let root = selection.profile.parent()?.file_name()?.to_str()?;
    let names = if matches!(root, "net.imput.helium" | "net.imput.helium.dev" | "helium") {
        vec!["helium-flags.conf".into(), "chromium-flags.conf".into()]
    } else {
        vec![format!("{}-flags.conf", selection.browser)]
    };
    for name in names {
        let Ok(flags) = std::fs::read_to_string(config.join(name)) else {
            continue;
        };
        let Some(tokens) = shlex::split(&flags) else {
            continue;
        };
        let store = tokens
            .iter()
            .enumerate()
            .filter_map(|(i, token)| {
                token.strip_prefix("--password-store=").or_else(|| {
                    (token == "--password-store")
                        .then(|| tokens.get(i + 1).map(String::as_str))
                        .flatten()
                })
            })
            .next_back();
        let backend = match store {
            Some("gnome" | "gnome-libsecret") => "GNOMEKEYRING",
            Some("kwallet") => "KWALLET",
            Some("kwallet5") => "KWALLET5",
            Some("kwallet6") => "KWALLET6",
            Some("basic") => "BASICTEXT",
            _ => continue,
        };
        return Some(backend.into());
    }
    None
}

pub(super) async fn password(selection: &Selection) -> Result<Zeroizing<Vec<u8>>> {
    let dirs = directories::BaseDirs::new().context(KEYRING_ERROR)?;
    let backend = configured_keyring(selection, dirs.config_dir()).unwrap_or_else(|| {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if desktop.split(':').any(|v| v.eq_ignore_ascii_case("KDE")) {
            match std::env::var("KDE_SESSION_VERSION").as_deref() {
                Ok("6") => "KWALLET6",
                Ok("5") => "KWALLET5",
                _ => "KWALLET",
            }
            .into()
        } else {
            "GNOMEKEYRING".into()
        }
    });
    let name = keyring_name(&selection.browser);
    match backend.as_str() {
        "GNOMEKEYRING" => gnome(name).await,
        "KWALLET" | "KWALLET5" | "KWALLET6" => kwallet(name, &backend).await,
        // Linux basic encryption uses v10, and needs no keyring at all. Reaching
        // this branch means v11 cookies do not match the selected password store.
        _ => bail!(
            "This profile has keyring-encrypted cookies. Select the browser's GNOME keyring or KWallet password store."
        ),
    }
}

// Decide by metadata before reading any secret. Electron applications can use
// the same label as Chromium, so a label-only lookup is not sufficient.
pub(super) fn select_identity(
    entries: &[(String, HashMap<String, String>)],
    name: &str,
) -> Result<usize> {
    let label = format!("{name} Safe Storage");
    let application = name.to_lowercase();
    let owned: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, (l, attrs))| *l == label && attrs.get("application") == Some(&application))
        .map(|(i, _)| i)
        .collect();
    let candidates = if owned.is_empty() {
        entries
            .iter()
            .enumerate()
            .filter(|(_, (l, attrs))| {
                *l == label && attrs.get("application").is_none_or(String::is_empty)
            })
            .map(|(i, _)| i)
            .collect()
    } else {
        owned
    };
    if candidates.len() != 1 {
        bail!(AMBIGUOUS);
    }
    Ok(candidates[0])
}

async fn gnome(name: &str) -> Result<Zeroizing<Vec<u8>>> {
    use secret_service::{EncryptionType, SecretService};
    let service = SecretService::connect(EncryptionType::Dh)
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    let collection = service
        .get_default_collection()
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    if collection
        .is_locked()
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?
    {
        bail!(LOCKED);
    }
    let items = collection
        .get_all_items()
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    let mut identities = Vec::with_capacity(items.len());
    for item in &items {
        identities.push((
            item.get_label()
                .await
                .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?,
            item.get_attributes()
                .await
                .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?,
        ));
    }
    let item = &items[select_identity(&identities, name)?];
    if item
        .is_locked()
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?
    {
        bail!(LOCKED);
    }
    item.get_secret()
        .await
        .map(Zeroizing::new)
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))
}

async fn kwallet(name: &str, backend: &str) -> Result<Zeroizing<Vec<u8>>> {
    let suffix = match backend {
        "KWALLET6" => "6",
        "KWALLET5" => "5",
        _ => "",
    };
    let connection = zbus::Connection::session()
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    let service = format!("org.kde.kwalletd{suffix}");
    let path = format!("/modules/kwalletd{suffix}");
    let proxy = zbus::Proxy::new(&connection, service, path, "org.kde.KWallet")
        .await
        .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    let result = async {
        let wallet: String = proxy.call("networkWallet", &()).await?;
        let unlocked: bool = proxy.call("isOpen", &(wallet.as_str(),)).await?;
        if !unlocked {
            return Ok::<_, zbus::Error>(None);
        }
        let app = "slintyt";
        let handle: i32 = proxy.call("open", &(wallet.as_str(), 0_i64, app)).await?;
        if handle < 0 {
            return Ok(None);
        }
        let folder = format!("{name} Keys");
        let entry = format!("{name} Safe Storage");
        let exists: bool = proxy
            .call("hasEntry", &(handle, folder.as_str(), entry.as_str(), app))
            .await?;
        if !exists {
            return Ok(None);
        }
        let password: String = proxy
            .call(
                "readPassword",
                &(handle, folder.as_str(), entry.as_str(), app),
            )
            .await?;
        Ok(Some(Zeroizing::new(password.into_bytes())))
    }
    .await
    .map_err(|_| anyhow::anyhow!(KEYRING_ERROR))?;
    result.context("The KWallet entry is missing or locked. Unlock the selected wallet and retry.")
}
