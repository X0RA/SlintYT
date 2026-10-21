use std::{collections::HashSet, time::Instant};

use crate::{history::HistoryCache, youtube::Video};

pub const UNKNOWN: i32 = 0;
pub const WATCHED: i32 = 1;
pub const UNWATCHED: i32 = 2;

#[derive(Clone, Copy, Default)]
pub enum Phase {
    #[default]
    Building,
    Refreshing,
    Reconciling,
    Idle,
}

#[derive(Default)]
pub struct WatchIndex {
    pub ids: HashSet<String>,
    pub hints_since: Option<Instant>,
    pub complete: bool,
    pub failed: bool,
    pub phase: Phase,
    pub cache_unavailable: bool,
    pub scanned: usize,
    pub pages: u64,
}

pub struct SyncStatus {
    pub running: bool,
    pub complete: bool,
    pub failed: bool,
    pub count: usize,
    pub title: String,
    pub detail: String,
    pub warning: String,
}

impl WatchIndex {
    pub fn begin(&mut self, cache: &HistoryCache, full: bool, now: u64) {
        self.ids = if cache.trusted(now) {
            cache.ids.clone()
        } else {
            HashSet::new()
        };
        if full {
            self.hints_since = Some(Instant::now());
        }
        self.complete = false;
        self.failed = false;
        self.scanned = 0;
        self.pages = 0;
        self.phase = if full && cache.last_full_sync.is_some() {
            Phase::Reconciling
        } else if full {
            Phase::Building
        } else {
            Phase::Refreshing
        };
    }

    pub fn sync_status(&self) -> SyncStatus {
        let running = !self.complete && !self.failed && !matches!(self.phase, Phase::Idle);
        let title = if self.failed {
            "Sync incomplete"
        } else if self.complete {
            "Up to date"
        } else {
            match self.phase {
                Phase::Building => "Syncing watch history…",
                Phase::Refreshing => "Checking recent watches…",
                Phase::Reconciling => "Checking history changes…",
                Phase::Idle => "Not synced yet",
            }
        };
        SyncStatus {
            running,
            complete: self.complete && !self.failed,
            failed: self.failed,
            count: self.ids.len(),
            title: title.into(),
            detail: if running {
                format!(
                    "{} {} checked this sync · {} {} loaded",
                    self.scanned,
                    if self.scanned == 1 { "video" } else { "videos" },
                    self.pages,
                    if self.pages == 1 { "page" } else { "pages" }
                )
            } else if self.failed {
                "Could not finish syncing. Your synced history is kept; try again.".into()
            } else if self.complete {
                "Your available YouTube watch history is synced.".into()
            } else {
                "Sync your history to update watched labels.".into()
            },
            warning: if self.cache_unavailable {
                "History could not be saved on this device. It will be checked again next time."
                    .into()
            } else {
                String::new()
            },
        }
    }

    pub fn annotate(&self, items: &mut [Video]) {
        for item in items.iter_mut().filter(|v| !v.playlist) {
            // Page hints are observations, never additions to the history index.
            // Reconciliation supersedes observations made before its snapshot.
            let fresh = self
                .hints_since
                .is_none_or(|since| item.observed_at.is_some_and(|at| at >= since));
            let hint = if fresh { item.watch_hint } else { UNKNOWN };
            if !fresh {
                item.watch_progress = None;
            }
            item.watched = if hint == WATCHED || self.ids.contains(&item.id) {
                WATCHED
            } else if hint == UNWATCHED || self.complete {
                UNWATCHED
            } else {
                UNKNOWN
            };
        }
    }

    pub fn note(&self) -> String {
        let note = if self.failed {
            "History refresh incomplete. Unconfirmed statuses are Unknown.".into()
        } else if self.complete {
            "Watch history up to date. Status is based on your available history and progress."
                .into()
        } else {
            match self.phase {
                Phase::Building => format!(
                    "Building watch history cache… {} watched videos found. Unknown labels update as history loads.",
                    self.ids.len()
                ),
                Phase::Refreshing => format!(
                    "Using cached watch history · {} watched videos. Refreshing recent watches…",
                    self.ids.len()
                ),
                Phase::Reconciling => format!(
                    "Checking for watch history changes… {} videos verified. Older cached statuses stay Unknown until checked.",
                    self.ids.len()
                ),
                Phase::Idle => {
                    "Watch history has not been verified. Unconfirmed statuses are Unknown.".into()
                }
            }
        };
        if self.cache_unavailable {
            format!(
                "{note} History could not be cached on this device; it will be checked again next time."
            )
        } else {
            note
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_status_reports_live_counts_and_distinguishes_failure_from_completion() {
        let cache = HistoryCache {
            ids: ["abcdefghijk".into(), "lmnopqrstuv".into()].into(),
            last_sync: 1_000_000,
            last_full_sync: Some(1_000_000),
            ..Default::default()
        };
        let mut index = WatchIndex::default();
        index.begin(&cache, false, 1_000_060);
        let status = index.sync_status();
        assert!(status.running);
        assert!(!status.complete);
        assert_eq!(status.count, 2);
        index.ids.insert("12345678901".into());
        index.scanned = 1;
        index.pages = 1;
        assert_eq!(index.sync_status().count, 3);
        assert!(index.sync_status().detail.contains("1 video checked"));
        index.failed = true;
        let failed = index.sync_status();
        assert!(!failed.running && failed.failed && !failed.complete);
        assert_eq!(failed.count, 3);
        index.failed = false;
        index.complete = true;
        index.phase = Phase::Idle;
        index.cache_unavailable = true;
        let complete = index.sync_status();
        assert!(complete.complete && !complete.running);
        assert_eq!(complete.title, "Up to date");
        assert!(!complete.warning.is_empty());
        index.begin(&HistoryCache::default(), true, 1_000_060);
        assert_eq!(index.scanned, 0);
        assert_eq!(index.pages, 0);
        assert_eq!(index.sync_status().count, 0);
        assert!(index.sync_status().running);
    }

    #[test]
    fn cached_hints_cannot_resurrect_reconciled_deletions() {
        let old = Instant::now();
        let mut cached = vec![Video {
            id: "abcdefghijk".into(),
            watch_hint: WATCHED,
            watch_progress: Some(30),
            observed_at: Some(old),
            ..Default::default()
        }];
        let mut index = WatchIndex::default();
        index.annotate(&mut cached);
        assert_eq!(cached[0].watched, WATCHED);
        assert!(index.ids.is_empty());
        index.hints_since = Some(old + std::time::Duration::from_nanos(1));
        index.complete = true;
        index.annotate(&mut cached);
        assert_eq!(cached[0].watched, UNWATCHED);
        assert_eq!(cached[0].watch_progress, None);
        let mut fresh = vec![Video {
            id: "abcdefghijk".into(),
            ..Default::default()
        }];
        index.annotate(&mut fresh);
        assert_eq!(fresh[0].watched, UNWATCHED);
        // A genuinely newer response can still report a rewatch.
        cached[0].observed_at = Some(Instant::now());
        cached[0].watch_hint = WATCHED;
        index.annotate(&mut cached);
        assert_eq!(cached[0].watched, WATCHED);
    }

    #[test]
    fn incomplete_history_never_labels_missing_videos_unwatched() {
        let mut index = WatchIndex::default();
        index.ids.insert("known".into());
        let mut items = vec![
            Video {
                id: "known".into(),
                ..Default::default()
            },
            Video {
                id: "old".into(),
                ..Default::default()
            },
        ];
        index.annotate(&mut items);
        assert_eq!(items[0].watched, WATCHED);
        assert_eq!(items[1].watched, UNKNOWN);
        index.complete = true;
        index.annotate(&mut items);
        assert_eq!(items[1].watched, UNWATCHED);
    }

    #[test]
    fn progress_and_explicit_status_survive_history_failures() {
        let index = WatchIndex {
            failed: true,
            ..Default::default()
        };
        let mut items = vec![
            Video {
                id: "progress".into(),
                watch_hint: WATCHED,
                ..Default::default()
            },
            Video {
                id: "zero".into(),
                watch_hint: UNWATCHED,
                ..Default::default()
            },
            Video {
                id: "list".into(),
                playlist: true,
                ..Default::default()
            },
        ];
        index.annotate(&mut items);
        assert!(!index.ids.contains("progress"));
        assert_eq!(items[0].watched, WATCHED);
        assert_eq!(items[1].watched, UNWATCHED);
        assert_eq!(items[2].watched, UNKNOWN);
    }
    #[test]
    fn expired_or_deleted_cached_badges_are_recomputed_from_server_evidence() {
        let mut cache = HistoryCache::default();
        cache.ids.insert("abcdefghijk".into());
        cache.last_sync = 1_000_000;
        cache.last_full_sync = Some(1_000_000);
        let mut index = WatchIndex::default();
        let mut items = vec![
            Video {
                id: "abcdefghijk".into(),
                ..Default::default()
            },
            Video {
                id: "lmnopqrstuv".into(),
                ..Default::default()
            },
        ];
        index.begin(&cache, false, 1_000_060);
        index.annotate(&mut items);
        assert_eq!(items[0].watched, WATCHED);
        assert_eq!(items[1].watched, UNKNOWN);
        index.begin(
            &cache,
            true,
            1_000_000 + crate::history::FULL_REFRESH_SECONDS,
        );
        index.annotate(&mut items);
        assert_eq!(items[0].watched, UNKNOWN);
        index.complete = true;
        index.annotate(&mut items);
        assert_eq!(items[0].watched, UNWATCHED);
        index.complete = false;
        index.failed = true;
        index.annotate(&mut items);
        assert_eq!(items[0].watched, UNKNOWN);
    }
}
