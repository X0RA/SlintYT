use std::{
    collections::HashSet,
    fs::{self, DirBuilder, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use rustypipe::cache::CacheStorage;
use serde::{Deserialize, Serialize};

pub const FULL_REFRESH_SECONDS: u64 = 7 * 24 * 60 * 60;
const CACHE_VERSION: u32 = 2;
const RECENT_LIMIT: usize = 200;
const OVERLAP_LENGTH: usize = 20;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const RESUME_SECONDS: u64 = 24 * 60 * 60;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    user_syncid: String,
    channel_syncid: Option<String>,
}

impl Account {
    fn valid(&self) -> bool {
        let valid = |id: &str| {
            !id.is_empty()
                && id.len() <= 96
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        valid(&self.user_syncid) && self.channel_syncid.as_deref().is_none_or(valid)
    }

    pub(crate) fn filename(&self) -> String {
        let channel = self.channel_syncid.as_deref().unwrap_or("");
        format!(
            "u{}-{}-c{}-{}.json",
            self.user_syncid.len(),
            self.user_syncid,
            channel.len(),
            channel
        )
    }
}

/// RustyPipe exposes identity through its cache-storage hook. This sink only
/// retains the account/channel IDs and session index in memory. Cookies, tokens and the complete
/// serialized cache are never retained here or written to disk.
#[derive(Clone, Default)]
pub struct IdentityCapture(Arc<RwLock<Option<PlaybackIdentity>>>);

#[derive(Clone, Deserialize)]
struct PlaybackIdentity {
    #[serde(flatten)]
    account: Account,
    session_index: Option<String>,
}

impl IdentityCapture {
    pub fn account(&self) -> Option<Account> {
        self.0.read().unwrap().as_ref().map(|v| v.account.clone())
    }

    pub fn playback_headers(&self) -> Option<(String, Option<String>)> {
        let identity = self.0.read().unwrap();
        let identity = identity.as_ref()?;
        Some((
            identity.session_index.clone()?,
            identity.account.channel_syncid.clone(),
        ))
    }
}

impl CacheStorage for IdentityCapture {
    fn read(&self) -> Option<String> {
        None
    }

    fn write(&self, data: &str) {
        #[derive(Deserialize)]
        struct Identity {
            auth_cookie: Option<PlaybackIdentity>,
        }
        *self.0.write().unwrap() = serde_json::from_str::<Identity>(data)
            .ok()
            .and_then(|identity| identity.auth_cookie)
            .filter(|identity| identity.account.valid());
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryCache {
    pub ids: HashSet<String>,
    pub recent: Vec<String>,
    pub last_sync: u64,
    pub last_full_sync: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
}

/// Where an interrupted full pass stopped, so the next attempt continues from
/// that page instead of rescanning the newest history again. The pass's own
/// observations are kept separately, because only they may replace the index.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resume {
    pub token: String,
    pub visitor: Option<String>,
    pub pages: u64,
    pub started: u64,
    observed: HashSet<String>,
    recent: Vec<String>,
}

impl Resume {
    fn valid(&self) -> bool {
        !self.token.is_empty()
            && self.token.len() <= 4096
            && self.visitor.as_ref().is_none_or(|v| v.len() <= 4096)
            && self.recent.len() <= RECENT_LIMIT
            && self.observed.iter().all(|id| video_id(id))
            && self.recent.iter().all(|id| self.observed.contains(id))
    }

    /// Continuation tokens expire, and a pass that started long ago would
    /// miss too much newer history.
    fn fresh(&self, now: u64) -> bool {
        self.started <= now && now - self.started < RESUME_SECONDS
    }
}

impl HistoryCache {
    pub fn full_due(&self, now: u64) -> bool {
        self.last_full_sync
            .is_none_or(|time| time > now || now - time >= FULL_REFRESH_SECONDS)
    }

    pub fn trusted(&self, now: u64) -> bool {
        let time = self.last_full_sync.unwrap_or(self.last_sync);
        time > 0 && time <= now && now - time < FULL_REFRESH_SECONDS
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    account: Account,
    history: HistoryCache,
}

#[derive(Clone)]
pub struct HistoryStore {
    root: PathBuf,
    legacy_root: Option<PathBuf>,
    account: Account,
}

impl HistoryStore {
    pub fn new(account: Account) -> Result<Self> {
        let dirs = BaseDirs::new().context("Could not locate your config folder.")?;
        Ok(Self {
            root: dirs.config_dir().join("slintyt/history"),
            legacy_root: Some(dirs.cache_dir().join("slintyt/history")),
            account,
        })
    }

    pub fn load(&self) -> Result<Option<HistoryCache>> {
        let file = match fs::File::open(self.root.join(self.account.filename())) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(root) = &self.legacy_root {
                    let legacy = Self {
                        root: root.clone(),
                        legacy_root: None,
                        account: self.account.clone(),
                    };
                    if let Some(history) = legacy.load()? {
                        // Validate and durably save before removing the old copy.
                        // If the config folder cannot be written, still use the old data.
                        if self.save(&history).is_ok() {
                            let _ = fs::remove_file(root.join(self.account.filename()));
                        }
                        return Ok(Some(history));
                    }
                }
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let mut data = Vec::new();
        file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_CACHE_BYTES {
            bail!("History cache is too large");
        }
        let record: Record = serde_json::from_slice(&data)?;
        let mut history = record.history;
        if !matches!(record.version, 1 | CACHE_VERSION)
            || record.account != self.account
            || !record.account.valid()
            || history.ids.iter().any(|id| !video_id(id))
            || history.recent.len() > RECENT_LIMIT
            || history.recent.iter().any(|id| !history.ids.contains(id))
            || history
                .last_full_sync
                .is_some_and(|time| time > history.last_sync)
        {
            bail!("History cache is invalid");
        }
        if history
            .resume
            .as_ref()
            .is_some_and(|resume| !resume.valid())
        {
            // An expired or malformed resume point only costs a rescan.
            history.resume = None;
        }
        if record.version == 1 {
            // The old parser omitted Shorts. Preserve positive evidence but
            // require a complete scan, even if the previous cache was recent.
            history.last_full_sync = None;
            history.recent.clear();
            history.resume = None;
        }
        Ok(Some(history))
    }

    pub fn save(&self, history: &HistoryCache) -> Result<()> {
        let mut dirs = DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dirs.mode(0o700);
        }
        dirs.create(&self.root)?;
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let temporary = self.root.join(format!(
            ".history-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| {
            serde_json::to_writer(
                &mut file,
                &Record {
                    version: CACHE_VERSION,
                    account: self.account.clone(),
                    history: history.clone(),
                },
            )?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&temporary, self.root.join(self.account.filename()))?;
            #[cfg(unix)]
            fs::File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One pass keeps its observations separate from the last good snapshot. A
/// failed full pass must never commit deletions or mark the index complete.
pub struct SyncPass {
    pub full: bool,
    pub observed: HashSet<String>,
    recent: Vec<String>,
    anchors: HashSet<String>,
    overlap: usize,
    overlap_needed: usize,
    found_overlap: bool,
    started: u64,
    /// Continuation token, visitor data and page count to start from.
    pub resume_from: Option<(String, Option<String>, u64)>,
}

impl SyncPass {
    pub fn new(cache: &HistoryCache, now: u64) -> Self {
        let anchors: HashSet<_> = cache.recent.iter().cloned().collect();
        let full = cache.full_due(now) || anchors.is_empty();
        let resume = cache
            .resume
            .as_ref()
            .filter(|resume| full && resume.valid() && resume.fresh(now));
        Self {
            full,
            observed: resume.map(|r| r.observed.clone()).unwrap_or_default(),
            recent: resume.map(|r| r.recent.clone()).unwrap_or_default(),
            overlap_needed: OVERLAP_LENGTH.min(anchors.len()),
            anchors,
            overlap: 0,
            found_overlap: false,
            started: resume.map_or(now, |r| r.started),
            resume_from: resume.map(|r| (r.token.clone(), r.visitor.clone(), r.pages)),
        }
    }

    pub fn page(&mut self, ids: &[String]) -> Result<bool> {
        for id in ids {
            if !video_id(id) {
                bail!("Invalid video in watch history");
            }
            if !self.observed.insert(id.clone()) {
                continue;
            }
            if self.recent.len() < RECENT_LIMIT {
                self.recent.push(id.clone());
            }
            if self.anchors.contains(id) {
                self.overlap += 1;
                if self.overlap >= self.overlap_needed {
                    self.found_overlap = true;
                }
            } else {
                self.overlap = 0;
            }
        }
        Ok(!self.full && self.overlap_needed > 0 && self.found_overlap)
    }

    pub fn finish(&self, cache: &HistoryCache, reached_end: bool, now: u64) -> HistoryCache {
        assert!(reached_end || (!self.full && self.found_overlap));
        let mut ids = if reached_end {
            HashSet::new()
        } else {
            cache.ids.clone()
        };
        ids.extend(self.observed.iter().cloned());
        HistoryCache {
            ids,
            recent: self.recent.clone(),
            last_sync: now,
            last_full_sync: if reached_end {
                Some(now)
            } else {
                cache.last_full_sync
            },
            resume: None,
        }
    }

    /// `next` is the continuation token and visitor data of the first page
    /// this pass has not processed yet, along with the pages already loaded.
    pub fn partial(
        &self,
        cache: &HistoryCache,
        next: Option<(&str, Option<&str>, u64)>,
        now: u64,
    ) -> HistoryCache {
        let mut result = cache.clone();
        result.ids.extend(self.observed.iter().cloned());
        // Preserve the last full-check time, so a failed weekly reconciliation
        // stays due instead of making old cached entries look newly verified.
        result.last_sync = now;
        result.resume = next
            .filter(|_| self.full)
            .map(|(token, visitor, pages)| Resume {
                token: token.into(),
                visitor: visitor.map(str::to_owned),
                pages,
                started: self.started,
                observed: self.observed.clone(),
                recent: self.recent.clone(),
            });
        result
    }

    /// The resume point failed before loading a page. Forget it, so the next
    /// attempt starts over from the newest history.
    pub fn abandon_resume(&mut self) {
        self.resume_from = None;
        self.observed.clear();
        self.recent.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_moves_to_config_only_after_a_valid_durable_save() {
        let root = tempfile::tempdir().unwrap();
        let legacy = HistoryStore {
            root: root.path().join("cache/history"),
            legacy_root: None,
            account: account("user", Some("channel")),
        };
        let original = full_cache(1_000_000);
        legacy.save(&original).unwrap();
        let store = HistoryStore {
            root: root.path().join("config/history"),
            legacy_root: Some(legacy.root.clone()),
            account: legacy.account.clone(),
        };
        assert_eq!(store.load().unwrap().unwrap().ids, original.ids);
        assert!(!legacy.root.join(legacy.account.filename()).exists());
        assert_eq!(
            store.load().unwrap().unwrap().last_full_sync,
            original.last_full_sync
        );

        // Existing config data wins even if an old cache file remains.
        legacy.save(&HistoryCache::default()).unwrap();
        assert_eq!(store.load().unwrap().unwrap().ids, original.ids);

        // An inaccessible destination must not remove the old copy.
        let blocked = root.path().join("not-a-directory");
        fs::write(&blocked, "file").unwrap();
        let broken = HistoryStore {
            root: blocked,
            ..store
        };
        assert!(broken.load().is_err());
        assert!(legacy.root.join(legacy.account.filename()).exists());
    }

    fn id(n: usize) -> String {
        format!("v{n:010}")
    }
    fn ids(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(id).collect()
    }
    fn account(user: &str, channel: Option<&str>) -> Account {
        Account {
            user_syncid: user.into(),
            channel_syncid: channel.map(str::to_owned),
        }
    }
    fn full_cache(now: u64) -> HistoryCache {
        HistoryCache {
            ids: ids(0..500).into_iter().collect(),
            recent: ids(0..200),
            last_sync: now,
            last_full_sync: Some(now),
            resume: None,
        }
    }

    #[test]
    fn legacy_cache_keeps_positive_evidence_but_requires_a_full_short_aware_scan() {
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().into(),
            account: account("test", None),
        };
        let cache = full_cache(1_000_000);
        store.save(&cache).unwrap();
        let path = store.root.join(store.account.filename());
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record["version"] = serde_json::json!(1);
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.ids, cache.ids);
        assert!(loaded.recent.is_empty());
        assert!(SyncPass::new(&loaded, 1_000_010).full);
        assert!(loaded.last_full_sync.is_none());
    }

    #[test]
    fn restart_uses_disk_snapshot_and_stops_after_recent_overlap() {
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().join("history"),
            account: account("user_a", None),
        };
        let original = full_cache(1_000_000);
        store.save(&original).unwrap();
        let cache = store.load().unwrap().unwrap();
        let mut pass = SyncPass::new(&cache, 1_000_060);
        assert!(!pass.full);
        let mut page = ids(500..505);
        page.extend(ids(0..50));
        assert!(pass.page(&page).unwrap());
        let next = pass.finish(&cache, false, 1_000_060);
        assert_eq!(next.ids.len(), 505);
        assert_eq!(next.last_full_sync, cache.last_full_sync);
        assert_eq!(&next.recent[..5], ids(500..505));
        store.save(&next).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), next);
    }

    #[test]
    fn isolated_rewatches_and_duplicate_ids_do_not_end_the_scan() {
        let cache = full_cache(1_000_000);
        let mut pass = SyncPass::new(&cache, 1_000_060);
        assert!(!pass.page(&[id(0), id(500)]).unwrap());
        assert!(!pass.page(&vec![id(1); 100]).unwrap());
        assert!(pass.page(&ids(2..22)).unwrap());
        assert!(pass.finish(&cache, false, 1_000_060).ids.contains(&id(500)));
    }

    #[test]
    fn weekly_reconciliation_replaces_history_and_removes_deleted_entries() {
        let cache = full_cache(1_000_000);
        let now = 1_000_000 + FULL_REFRESH_SECONDS;
        let mut pass = SyncPass::new(&cache, now);
        assert!(pass.full);
        assert!(!cache.trusted(now));
        assert!(!pass.page(&ids(0..100)).unwrap());
        assert!(!pass.page(&ids(100..400)).unwrap());
        let next = pass.finish(&cache, true, now);
        assert_eq!(next.ids.len(), 400);
        assert!(!next.ids.contains(&id(450)));
        assert_eq!(next.last_full_sync, Some(now));
        assert!(next.trusted(now));
    }

    #[test]
    fn clear_history_is_detected_even_during_an_incremental_refresh() {
        let cache = full_cache(1_000_000);
        let pass = SyncPass::new(&cache, 1_000_060);
        let next = pass.finish(&cache, true, 1_000_060);
        assert!(next.ids.is_empty());
        assert!(next.recent.is_empty());
        assert_eq!(next.last_full_sync, Some(1_000_060));
    }

    #[test]
    fn failed_reconciliation_preserves_the_last_snapshot_and_stays_due() {
        let cache = full_cache(1_000_000);
        let now = 1_000_000 + FULL_REFRESH_SECONDS;
        let mut pass = SyncPass::new(&cache, now);
        pass.page(&ids(500..505)).unwrap();
        let partial = pass.partial(&cache, None, now);
        assert!(partial.ids.contains(&id(450)));
        assert!(partial.ids.contains(&id(503)));
        assert!(partial.full_due(now));
        assert!(!partial.trusted(now));
        assert_eq!(partial.last_full_sync, cache.last_full_sync);
    }

    #[test]
    fn interrupted_initial_history_remains_incomplete_after_restart() {
        let mut pass = SyncPass::new(&HistoryCache::default(), 1_000_000);
        assert!(!pass.page(&ids(0..50)).unwrap());
        let partial = pass.partial(&HistoryCache::default(), None, 1_000_000);
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().join("history"),
            account: account("user", None),
        };
        store.save(&partial).unwrap();
        let cache = store.load().unwrap().unwrap();
        assert_eq!(cache.ids.len(), 50);
        assert!(cache.last_full_sync.is_none());
        assert!(SyncPass::new(&cache, 1_000_100).full);
    }

    #[test]
    fn interrupted_full_pass_resumes_from_its_last_page_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().join("history"),
            account: account("user", None),
        };
        // Stale IDs from an older attempt must not survive a resumed pass.
        let mut stale = HistoryCache::default();
        stale.ids.insert(id(900));
        let mut pass = SyncPass::new(&stale, 1_000_000);
        assert!(pass.resume_from.is_none());
        pass.page(&ids(0..50)).unwrap();
        store
            .save(&pass.partial(&stale, Some(("page-44", Some("visitor"), 43)), 1_000_100))
            .unwrap();

        let cache = store.load().unwrap().unwrap();
        let mut resumed = SyncPass::new(&cache, 1_000_200);
        assert_eq!(
            resumed.resume_from,
            Some(("page-44".into(), Some("visitor".into()), 43))
        );
        assert_eq!(resumed.observed.len(), 50);
        resumed.page(&ids(50..80)).unwrap();
        let done = resumed.finish(&cache, true, 1_000_300);
        assert_eq!(done.ids.len(), 80);
        assert!(!done.ids.contains(&id(900)));
        assert_eq!(&done.recent[..50], ids(0..50));
        assert!(done.resume.is_none());
        assert_eq!(done.last_full_sync, Some(1_000_300));
        // With recent anchors restored, the follow-up pass is incremental.
        assert!(!SyncPass::new(&done, 1_000_400).full);
    }

    #[test]
    fn stale_or_abandoned_resume_points_start_over() {
        let mut pass = SyncPass::new(&HistoryCache::default(), 1_000_000);
        pass.page(&ids(0..50)).unwrap();
        let partial = pass.partial(
            &HistoryCache::default(),
            Some(("next", None, 10)),
            1_000_000,
        );
        assert!(
            SyncPass::new(&partial, 1_000_000 + RESUME_SECONDS)
                .resume_from
                .is_none()
        );
        let mut resumed = SyncPass::new(&partial, 1_000_100);
        assert!(resumed.resume_from.is_some());
        resumed.abandon_resume();
        let retry = resumed.partial(&partial, None, 1_000_200);
        assert!(retry.resume.is_none());
        assert_eq!(retry.ids.len(), 50);
        assert!(SyncPass::new(&retry, 1_000_300).resume_from.is_none());
        // Incremental refreshes never record a resume point.
        let mut incremental = SyncPass::new(&full_cache(1_000_000), 1_000_060);
        incremental.page(&ids(500..505)).unwrap();
        assert!(
            incremental
                .partial(&full_cache(1_000_000), Some(("next", None, 1)), 1_000_060)
                .resume
                .is_none()
        );
    }

    #[test]
    fn user_and_brand_channel_caches_are_isolated_and_private() {
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().join("history"),
            account: account("user_a", None),
        };
        store.save(&full_cache(1_000_000)).unwrap();
        let other_user = HistoryStore {
            account: account("user_b", None),
            ..store.clone()
        };
        let other_channel = HistoryStore {
            account: account("user_a", Some("brand")),
            ..store.clone()
        };
        assert!(other_user.load().unwrap().is_none());
        assert!(other_channel.load().unwrap().is_none());
        // A renamed or misplaced file is also checked against its embedded account.
        fs::copy(
            store.root.join(store.account.filename()),
            store.root.join(other_user.account.filename()),
        )
        .unwrap();
        assert!(other_user.load().is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.root.join(store.account.filename()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn identity_hook_drops_credentials_and_survives_cookie_rotation() {
        let capture = IdentityCapture::default();
        capture.write(r#"{"auth_cookie":{"cookie":"synthetic_old_cookie","user_syncid":"12345","channel_syncid":"98765","session_index":"0"},"oauth_token":{"access_token":"synthetic_token"}}"#);
        let first = capture.account().unwrap();
        capture.write(r#"{"auth_cookie":{"cookie":"synthetic_new_cookie","user_syncid":"12345","channel_syncid":"98765","session_index":"2"}}"#);
        assert_eq!(first, capture.account().unwrap());
        assert!(capture.read().is_none());
        let json = serde_json::to_string(&first).unwrap();
        assert!(!json.contains("cookie"));
        assert!(!json.contains("token"));
        capture.write(r#"{"auth_cookie":{"user_syncid":"other_account"}}"#);
        assert_ne!(first, capture.account().unwrap());
        capture.write(r#"{"auth_cookie":{"user_syncid":"../../escape"}}"#);
        assert!(capture.account().is_none());
    }

    #[test]
    fn bad_schema_corrupt_data_and_clock_changes_cannot_create_completeness() {
        let root = tempfile::tempdir().unwrap();
        let store = HistoryStore {
            legacy_root: None,
            root: root.path().join("history"),
            account: account("user", None),
        };
        store.save(&full_cache(1_000_000)).unwrap();
        let path = store.root.join(store.account.filename());
        let original = fs::read(&path).unwrap();
        for (key, value) in [
            ("version", serde_json::json!(999)),
            (
                "history",
                serde_json::json!({"ids":["bad"],"recent":[],"last_sync":0,"last_full_sync":null}),
            ),
        ] {
            let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
            record[key] = value;
            fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            assert!(store.load().is_err());
        }
        fs::write(&path, "interrupted JSON").unwrap();
        assert!(store.load().is_err());
        let cache = full_cache(1_000_000);
        assert!(cache.full_due(999_999));
        assert!(!cache.trusted(999_999));
    }
}
