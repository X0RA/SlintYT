//! Twitch videos watched or bookmarked here, channels followed, and recent
//! searches. Twitch has no account in the app, so all of it stays on this
//! computer, in one file in the config folder.
use crate::twitch::SearchResult;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

/// Videos only watched, and not bookmarked, are forgotten oldest first past this.
const WATCHED_LIMIT: usize = 1000;
/// Starting over is more useful than resuming a few seconds in, or at the credits.
const RESUME_AFTER: f64 = 10.;
const RESUME_BEFORE_END: f64 = 30.;
const SEARCH_LIMIT: usize = 12;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub video: SearchResult,
    /// Seconds into the video, and its length as the player measured it.
    pub position: f64,
    pub length: f64,
    pub watched_at: u64,
    pub bookmarked_at: Option<u64>,
}

impl Entry {
    fn length(&self) -> f64 {
        if self.length > 0. {
            self.length
        } else {
            self.video.seconds as f64
        }
    }
}

/// A followed channel, as its page showed it when it was followed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Follow {
    pub login: String,
    pub name: String,
    pub picture: String,
    pub followed_at: u64,
    /// The newest video ID when the channel was last opened. Twitch's video
    /// IDs only grow, so larger ones are new. None until first checked.
    pub seen: Option<u64>,
}

/// `mode` follows the search radio: 0 for videos, 1 for users.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentSearch {
    pub query: String,
    pub mode: i32,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    videos: BTreeMap<String, Entry>,
    follows: BTreeMap<String, Follow>,
    searches: Vec<RecentSearch>,
}

#[derive(Default)]
pub struct Library {
    path: Option<PathBuf>,
    videos: BTreeMap<String, Entry>,
    follows: BTreeMap<String, Follow>,
    searches: Vec<RecentSearch>,
}

static LIBRARY: LazyLock<Mutex<Library>> = LazyLock::new(|| {
    Mutex::new(
        crate::config::Store::new()
            .map(|store| Library::load(store.directory().join("twitch-library.json")))
            .unwrap_or_default(),
    )
});

/// The library shared by the Twitch pages and the player.
pub fn shared() -> std::sync::MutexGuard<'static, Library> {
    LIBRARY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Accepts the player's `twitch:123` as well as a bare `123`.
fn key(id: &str) -> Option<&str> {
    let id = id.strip_prefix("twitch:").unwrap_or(id);
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

impl Library {
    /// A missing or unreadable file starts an empty library.
    pub fn load(path: PathBuf) -> Self {
        let saved = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            videos: saved
                .videos
                .into_iter()
                .filter(|(id, _)| key(id).is_some())
                .collect(),
            follows: saved
                .follows
                .into_iter()
                .filter(|(login, _)| valid_login(login))
                .collect(),
            searches: saved.searches,
        }
    }

    /// Keeps a video's title, picture and so on for the bookmarks list. Its
    /// progress and bookmark are left alone.
    pub fn remember(&mut self, mut video: SearchResult) {
        let Some(id) = key(&video.id).map(str::to_owned) else {
            return;
        };
        video.id = id.clone();
        let entry = self.videos.entry(id).or_default();
        // A user's video list leaves out the channel, which search includes.
        if video.subtitle.is_empty() {
            video.subtitle = std::mem::take(&mut entry.video.subtitle);
        }
        entry.video = video;
    }

    pub fn known(&self, id: &str) -> bool {
        key(id).is_some_and(|id| self.videos.contains_key(id))
    }

    /// Returns whether anything changed, so unchanged ticks skip the disk.
    pub fn record(&mut self, id: &str, position: f64, length: f64, now: u64) -> bool {
        let Some(entry) = key(id).and_then(|id| self.videos.get_mut(id)) else {
            return false;
        };
        if !position.is_finite() || position < 0. {
            return false;
        }
        let length = if length.is_finite() && length > 0. {
            length
        } else {
            entry.length
        };
        if (entry.position - position).abs() < 1. && entry.length == length {
            return false;
        }
        entry.position = position;
        entry.length = length;
        entry.watched_at = now;
        true
    }

    /// Returns whether the video is bookmarked now, or None for a video this
    /// library has never seen.
    pub fn toggle_bookmark(&mut self, id: &str, now: u64) -> Option<bool> {
        let entry = self.videos.get_mut(key(id)?)?;
        entry.bookmarked_at = match entry.bookmarked_at {
            Some(_) => None,
            None => Some(now),
        };
        Some(entry.bookmarked_at.is_some())
    }

    pub fn bookmarked(&self, id: &str) -> bool {
        self.entry(id).is_some_and(|e| e.bookmarked_at.is_some())
    }

    /// Percent watched, or 0 for a video never played here.
    pub fn progress(&self, id: &str) -> f32 {
        self.entry(id)
            .filter(|e| e.watched_at > 0 && e.length() > 0.)
            .map_or(0., |e| {
                (e.position / e.length() * 100.).clamp(0., 100.) as f32
            })
    }

    /// Where to pick a video back up, unless it barely started or was finished.
    pub fn resume(&self, id: &str) -> Option<f64> {
        let entry = self.entry(id)?;
        let length = entry.length();
        (entry.position >= RESUME_AFTER
            && (length <= 0. || entry.position < length - RESUME_BEFORE_END))
            .then_some(entry.position)
    }

    pub fn bookmarks(&self) -> Vec<Entry> {
        self.videos
            .values()
            .filter(|e| e.bookmarked_at.is_some())
            .cloned()
            .collect()
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.videos.get(key(id)?)
    }

    /// Returns whether the channel is followed now.
    pub fn toggle_follow(&mut self, mut follow: Follow, now: u64) -> bool {
        if !valid_login(&follow.login) {
            return false;
        }
        if self.follows.remove(&follow.login).is_some() {
            return false;
        }
        follow.followed_at = now;
        follow.seen = None;
        self.follows.insert(follow.login.clone(), follow);
        true
    }

    pub fn following(&self, login: &str) -> bool {
        self.follows.contains_key(login)
    }

    pub fn follow(&self, login: &str) -> Option<&Follow> {
        self.follows.get(login)
    }

    pub fn follows(&self) -> Vec<Follow> {
        self.follows.values().cloned().collect()
    }

    /// Records the newest video as seen. Returns whether anything changed.
    pub fn mark_seen(&mut self, login: &str, newest: u64) -> bool {
        match self.follows.get_mut(login) {
            Some(follow) if follow.seen < Some(newest) => {
                follow.seen = Some(newest);
                true
            }
            _ => false,
        }
    }

    /// A repeated search moves to the front instead of appearing twice.
    pub fn add_search(&mut self, query: &str, mode: i32) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        self.searches
            .retain(|s| s.mode != mode || !s.query.eq_ignore_ascii_case(query));
        self.searches.insert(
            0,
            RecentSearch {
                query: query.into(),
                mode,
            },
        );
        self.searches.truncate(SEARCH_LIMIT);
    }

    pub fn remove_search(&mut self, index: usize) {
        if index < self.searches.len() {
            self.searches.remove(index);
        }
    }

    pub fn clear_searches(&mut self) {
        self.searches.clear();
    }

    pub fn searches(&self) -> &[RecentSearch] {
        &self.searches
    }

    pub fn save(&mut self) -> Result<()> {
        self.prune();
        let Some(path) = &self.path else {
            return Ok(());
        };
        write(
            path,
            &Saved {
                videos: self.videos.clone(),
                follows: self.follows.clone(),
                searches: self.searches.clone(),
            },
        )
    }

    /// Drops videos that were only looked up, then the oldest watched ones.
    fn prune(&mut self) {
        self.videos
            .retain(|_, e| e.bookmarked_at.is_some() || e.watched_at > 0);
        let mut watched: Vec<_> = self
            .videos
            .iter()
            .filter(|(_, e)| e.bookmarked_at.is_none())
            .map(|(id, e)| (e.watched_at, id.clone()))
            .collect();
        if watched.len() > WATCHED_LIMIT {
            watched.sort();
            for (_, id) in &watched[..watched.len() - WATCHED_LIMIT] {
                self.videos.remove(id);
            }
        }
    }
}

/// Twitch logins are short and only use letters, digits and underscores.
fn valid_login(login: &str) -> bool {
    (1..=25).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// How many of a channel's newest video IDs are newer than the one last seen.
pub fn new_videos(ids: &[u64], seen: Option<u64>) -> usize {
    seen.map_or(0, |seen| ids.iter().filter(|id| **id > seen).count())
}

fn write(path: &Path, saved: &Saved) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Could not create the SlintYT config folder.")?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(saved)?)
        .context("Could not save your Twitch library.")?;
    fs::rename(temporary, path).context("Could not save your Twitch library.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str, seconds: u64) -> SearchResult {
        SearchResult {
            id: id.into(),
            title: format!("VOD {id}"),
            subtitle: "Runner".into(),
            seconds,
            ..SearchResult::default()
        }
    }

    #[test]
    fn progress_and_bookmarks_survive_a_reload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/twitch-library.json");
        let mut library = Library::load(path.clone());
        library.remember(video("1", 3600));
        library.remember(video("2", 600));
        library.remember(video("3", 600));
        assert!(library.record("twitch:1", 1800., 3600., 10));
        assert!(!library.record("1", 1800.4, 3600., 11));
        assert_eq!(library.toggle_bookmark("2", 20), Some(true));
        assert_eq!(library.toggle_bookmark("9", 20), None);
        library.save().unwrap();
        let library = Library::load(path);
        assert_eq!(library.progress("twitch:1"), 50.);
        assert_eq!(library.resume("1"), Some(1800.));
        assert!(library.bookmarked("2") && !library.bookmarked("1"));
        // Only looked up, so it was not kept.
        assert!(!library.known("3"));
        assert_eq!(library.bookmarks().len(), 1);
    }

    #[test]
    fn resumes_only_between_the_start_and_the_end() {
        let mut library = Library::default();
        library.remember(video("1", 3600));
        assert_eq!(library.resume("1"), None);
        library.record("1", 5., 0., 1);
        assert_eq!(library.resume("1"), None);
        library.record("1", 120., 0., 2);
        assert_eq!(library.resume("1"), Some(120.));
        library.record("1", 3590., 3600., 3);
        assert_eq!(library.resume("1"), None);
        assert!(library.progress("1") > 99.);
        assert!(!library.record("1", f64::NAN, 3600., 4));
    }

    #[test]
    fn remembering_again_keeps_progress_and_the_channel() {
        let mut library = Library::default();
        library.remember(video("1", 3600));
        library.record("1", 60., 3600., 1);
        library.toggle_bookmark("1", 2);
        let mut again = video("1", 3600);
        again.subtitle.clear();
        again.title = "Renamed".into();
        library.remember(again);
        let entry = &library.bookmarks()[0];
        assert_eq!(
            (entry.video.title.as_str(), entry.video.subtitle.as_str()),
            ("Renamed", "Runner")
        );
        assert_eq!(entry.position, 60.);
        library.remember(video("abc", 1));
        assert!(!library.known("abc"));
    }

    fn follow(login: &str) -> Follow {
        Follow {
            login: login.into(),
            name: login.to_uppercase(),
            ..Follow::default()
        }
    }

    #[test]
    fn follows_and_searches_survive_a_reload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("twitch-library.json");
        let mut library = Library::load(path.clone());
        assert!(library.toggle_follow(follow("runner"), 5));
        assert!(!library.toggle_follow(follow("bad login"), 5));
        assert!(library.toggle_follow(follow("other"), 6));
        assert!(!library.toggle_follow(follow("other"), 7));
        assert!(library.mark_seen("runner", 100));
        assert!(!library.mark_seen("runner", 90));
        assert!(!library.mark_seen("nobody", 100));
        library.add_search("speedrun", 0);
        library.add_search("runner", 1);
        library.add_search("  SPEEDRUN ", 0);
        library.add_search("", 0);
        library.save().unwrap();
        let library = Library::load(path);
        assert!(library.following("runner") && !library.following("other"));
        let runner = &library.follows()[0];
        assert_eq!((runner.followed_at, runner.seen), (5, Some(100)));
        let searches: Vec<_> = library
            .searches()
            .iter()
            .map(|s| (s.query.as_str(), s.mode))
            .collect();
        assert_eq!(searches, [("SPEEDRUN", 0), ("runner", 1)]);
    }

    #[test]
    fn searches_are_capped_and_can_be_removed() {
        let mut library = Library::default();
        for i in 0..SEARCH_LIMIT + 3 {
            library.add_search(&format!("q{i}"), 0);
        }
        assert_eq!(library.searches().len(), SEARCH_LIMIT);
        assert_eq!(
            library.searches()[0].query,
            format!("q{}", SEARCH_LIMIT + 2)
        );
        library.remove_search(0);
        library.remove_search(99);
        assert_eq!(library.searches().len(), SEARCH_LIMIT - 1);
        library.clear_searches();
        assert!(library.searches().is_empty());
    }

    #[test]
    fn counts_videos_newer_than_the_last_one_seen() {
        let ids = [105, 104, 100, 99];
        assert_eq!(new_videos(&ids, Some(100)), 2);
        assert_eq!(new_videos(&ids, Some(105)), 0);
        assert_eq!(new_videos(&ids, None), 0);
    }

    #[test]
    fn forgets_the_oldest_watched_videos_but_never_bookmarks() {
        let mut library = Library::default();
        for i in 0..WATCHED_LIMIT as u64 + 2 {
            let id = (i + 1).to_string();
            library.remember(video(&id, 600));
            library.record(&id, 60., 600., i + 1);
        }
        library.toggle_bookmark("1", 5);
        library.save().unwrap();
        assert!(library.known("1"));
        assert!(!library.known("2"));
        assert!(library.known("3"));
        assert_eq!(library.videos.len(), WATCHED_LIMIT + 1);
    }
}
