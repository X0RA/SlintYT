use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::browser::Selection;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountProfile {
    pub name: String,
    pub avatar: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedProfile {
    #[serde(flatten)]
    pub selection: Selection,
    #[serde(default)]
    pub youtube: AccountProfile,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Settings {
    /// `browser` was the sole field before saved profiles were introduced.
    #[serde(default, alias = "browser")]
    pub current_profile: Option<Selection>,
    #[serde(default)]
    pub profiles: Vec<SavedProfile>,
}

impl Settings {
    fn remember(&mut self, selection: &Selection) {
        if let Some(profile) = self.profiles.iter_mut().find(|p| {
            p.selection.browser == selection.browser && p.selection.profile == selection.profile
        }) {
            profile.selection = selection.clone();
        } else {
            self.profiles.push(SavedProfile {
                selection: selection.clone(),
                youtube: AccountProfile::default(),
            });
        }
        self.current_profile = Some(selection.clone());
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new() -> Result<Self> {
        let dirs = BaseDirs::new().context("Could not locate your config folder.")?;
        Ok(Self {
            root: dirs.config_dir().join("slintyt"),
        })
    }

    pub fn load(&self) -> Result<Option<Selection>> {
        Ok(self.load_settings()?.current_profile)
    }

    pub fn load_settings(&self) -> Result<Settings> {
        let path = self.root.join("settings.json");
        match fs::read(path) {
            Ok(data) => {
                let mut settings: Settings = serde_json::from_slice(&data).context(
                    "Saved browser settings are invalid. Please choose your browser again.",
                )?;
                if let Some(current) = settings.current_profile.clone()
                    && !settings.profiles.iter().any(|p| p.selection == current)
                {
                    settings.remember(&current);
                }
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).context("Could not read the saved browser selection."),
        }
    }

    pub fn save(&self, browser: &Selection) -> Result<()> {
        let mut settings = self.load_settings()?;
        settings.remember(browser);
        self.write(&settings)
    }

    /// Updating display metadata must never change the active browser selection.
    pub fn save_account(&self, selection: &Selection, account: AccountProfile) -> Result<()> {
        let mut settings = self.load_settings()?;
        if let Some(profile) = settings
            .profiles
            .iter_mut()
            .find(|p| &p.selection == selection)
        {
            profile.youtube = account;
            self.write(&settings)?;
        }
        Ok(())
    }

    fn write(&self, settings: &Settings) -> Result<()> {
        fs::create_dir_all(&self.root).context("Could not create the SlintYT config folder.")?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let temporary = self.root.join("settings.json.tmp");
        let mut file = options
            .open(&temporary)
            .context("Could not save the browser selection.")?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.sync_all()?;
        fs::rename(temporary, self.root.join("settings.json"))
            .context("Could not finish saving the browser selection.")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_metadata_survives_switches_without_changing_current_selection() {
        let root = tempfile::tempdir().unwrap();
        let store = Store {
            root: root.path().into(),
        };
        let a = Selection {
            browser: "chromium".into(),
            profile: root.path().join("Default"),
            keyring: None,
        };
        let b = Selection {
            profile: root.path().join("Profile 2"),
            ..a.clone()
        };
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        let account = AccountProfile {
            name: "Example creator".into(),
            avatar: "https://yt3.ggpht.com/example".into(),
        };
        store.save_account(&a, account.clone()).unwrap();
        let settings = store.load_settings().unwrap();
        assert_eq!(settings.current_profile, Some(b.clone()));
        assert_eq!(settings.profiles[0].youtube, account);
        store.save(&a).unwrap();
        assert_eq!(store.load_settings().unwrap().profiles[0].youtube, account);
        assert_eq!(store.load().unwrap(), Some(a));
        // Delayed metadata is not authorization to add an unverified profile.
        let unknown = Selection {
            profile: root.path().join("Unknown"),
            ..b
        };
        store
            .save_account(&unknown, AccountProfile::default())
            .unwrap();
        assert_eq!(store.load_settings().unwrap().profiles.len(), 2);
    }

    #[test]
    fn legacy_selection_is_preserved_and_successful_profiles_are_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let store = Store {
            root: root.path().into(),
        };
        let first = Selection {
            browser: "firefox".into(),
            profile: root.path().join("personal"),
            keyring: None,
        };
        fs::write(
            store.root.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"browser": first})).unwrap(),
        )
        .unwrap();
        assert_eq!(store.load().unwrap(), Some(first.clone()));
        assert_eq!(store.load_settings().unwrap().profiles[0].selection, first);
        let second = Selection {
            profile: root.path().join("work"),
            ..first.clone()
        };
        store.save(&second).unwrap();
        let updated = Selection {
            keyring: Some("GNOMEKEYRING".into()),
            ..first.clone()
        };
        store.save(&updated).unwrap();
        store.save(&updated).unwrap();
        let settings = store.load_settings().unwrap();
        assert_eq!(settings.current_profile, Some(updated.clone()));
        assert_eq!(
            settings
                .profiles
                .iter()
                .map(|p| &p.selection)
                .collect::<Vec<_>>(),
            vec![&updated, &second]
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(store.root.join("settings.json")).unwrap()).unwrap();
        assert!(raw.get("browser").is_none());
        assert!(raw.get("current_profile").is_some());
    }

    #[test]
    fn settings_round_trip_and_corrupt_file_error() {
        let root = tempfile::tempdir().unwrap();
        let store = Store {
            root: root.path().join("slintyt"),
        };
        assert!(store.load().unwrap().is_none());
        let selection = Selection {
            browser: "firefox".into(),
            profile: root.path().join("Personal profile"),
            keyring: Some("GNOMEKEYRING".into()),
        };
        store.save(&selection).unwrap();
        assert_eq!(store.load().unwrap(), Some(selection));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.root.join("settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::write(store.root.join("settings.json"), "broken").unwrap();
        assert!(store.load().is_err());
    }
}
