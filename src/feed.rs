use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use crate::youtube::{Page, Pixels, Video, View};

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FeedPage {
    pub videos: Vec<Video>,
    pub continuation: Option<String>,
    pub visitor: Option<String>,
    pub title: String,
    pub channel_info: Option<crate::youtube::ChannelInfo>,
    pub pages: usize,
    requested_tokens: HashSet<String>,
}

impl FeedPage {
    /// Disk snapshots provide a first paint; a fresh request owns pagination.
    pub fn disk_snapshot(&self) -> Self {
        Self {
            videos: self.videos.iter().take(200).cloned().collect(),
            title: self.title.clone(),
            channel_info: self.channel_info.clone(),
            pages: 1,
            ..Default::default()
        }
    }

    pub fn accept(
        &mut self,
        view: &View,
        requested: Option<&str>,
        page: Page,
    ) -> anyhow::Result<()> {
        if requested.is_some_and(|token| self.requested_tokens.contains(token))
            || page
                .continuation
                .as_deref()
                .is_some_and(|next| Some(next) == requested || self.requested_tokens.contains(next))
        {
            anyhow::bail!("YouTube repeated a page. Please refresh.");
        }
        if let Some(token) = requested {
            self.requested_tokens.insert(token.to_owned());
        }
        self.append(view, page);
        Ok(())
    }

    pub fn append(&mut self, view: &View, page: Page) {
        if page.channel_info.is_some() {
            self.channel_info = page.channel_info;
        }
        for mut video in page.videos {
            if matches!(view, View::Channel { .. })
                && !video.playlist
                && let Some(info) = &self.channel_info
            {
                video.channel = info.name.clone();
                video.channel_id = info.id.clone();
            }
            if matches!(view, View::History | View::Playlist(_))
                || !self
                    .videos
                    .iter()
                    .any(|v| v.id == video.id && v.playlist == video.playlist)
            {
                self.videos.push(video);
            }
        }
        self.continuation = page.continuation;
        if page.visitor_data.is_some() {
            self.visitor = page.visitor_data;
        }
        if !page.title.is_empty() {
            self.title = page.title;
        }
        self.pages += 1;
    }
}

#[derive(Default)]
pub struct PageCache(VecDeque<(View, FeedPage)>);

impl PageCache {
    pub fn get(&mut self, view: &View) -> Option<FeedPage> {
        let index = self.0.iter().position(|(key, _)| key == view)?;
        let entry = self.0.remove(index)?;
        let page = entry.1.clone();
        self.0.push_back(entry);
        Some(page)
    }

    pub fn put(&mut self, view: &View, page: &FeedPage) {
        self.0.retain(|(key, _)| key != view);
        self.0.push_back((view.clone(), page.clone()));
        while self.0.len() > 24 {
            self.0.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Editing invalidates continuation tokens as well as the visible rows.
    /// Only the currently edited playlist needs an immediate fresh load.
    pub fn invalidate_playlist(
        &mut self,
        playlist: &str,
        current: &View,
        feed: &mut FeedPage,
    ) -> Option<usize> {
        let view = View::Playlist(playlist.to_owned());
        self.0
            .retain(|(key, _)| key != &view && key != &View::Playlists);
        if current != &view {
            return None;
        }
        let pages = feed.pages.max(1);
        *feed = FeedPage::default();
        Some(pages)
    }
}

/// Public thumbnail pixels can be reused immediately when restoring a page.
/// Keep the cache bounded, including when users browse many large playlists.
type ThumbnailEntries = VecDeque<(String, Arc<Pixels>)>;

#[derive(Clone, Default)]
pub struct ThumbnailCache(Arc<Mutex<ThumbnailEntries>>);

impl ThumbnailCache {
    pub fn get(&self, url: &str) -> Option<Arc<Pixels>> {
        let mut entries = self.0.lock().unwrap();
        let index = entries.iter().position(|(key, _)| key == url)?;
        let entry = entries.remove(index)?;
        let pixels = entry.1.clone();
        entries.push_back(entry);
        Some(pixels)
    }

    pub fn put(&self, url: String, pixels: Arc<Pixels>) {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|(key, _)| key != &url);
        entries.push_back((url, pixels));
        while entries.len() > 96 {
            entries.pop_front();
        }
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_snapshot_restores_content_without_stale_pagination() {
        let mut feed = FeedPage::default();
        feed.accept(&View::Home, None, page(&["a"], Some("old-token")))
            .unwrap();
        let encoded = serde_json::to_vec(&feed.disk_snapshot()).unwrap();
        let mut restored: FeedPage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.videos[0].id, "a");
        assert_eq!(restored.pages, 1);
        assert!(restored.continuation.is_none());
        assert!(restored.visitor.is_none());
        restored
            .accept(&View::Home, None, page(&["b"], Some("old-token")))
            .unwrap();
    }

    #[test]
    fn playlist_edits_discard_cached_rows_and_continuations_before_loading_more() {
        let view = View::Playlist("PL_test".into());
        let mut feed = FeedPage::default();
        feed.accept(&view, None, page(&["a"], Some("page-2")))
            .unwrap();
        feed.accept(&view, Some("page-2"), page(&["b"], Some("page-3")))
            .unwrap();
        let mut cache = PageCache::default();
        cache.put(&view, &feed);
        cache.put(&View::Playlists, &feed);
        assert_eq!(
            cache.invalidate_playlist("PL_test", &view, &mut feed),
            Some(2)
        );
        assert!(feed.videos.is_empty());
        assert!(feed.continuation.is_none());
        assert!(feed.requested_tokens.is_empty());
        assert!(cache.get(&view).is_none());
        assert!(cache.get(&View::Playlists).is_none());
        feed.accept(&view, None, page(&["b"], Some("fresh")))
            .unwrap();
        feed.accept(&view, Some("fresh"), page(&["c"], None))
            .unwrap();
        assert_eq!(
            feed.videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn delayed_playlist_edits_leave_another_views_rows_intact() {
        let edited = View::Playlist("PL_old".into());
        for current in [View::Home, View::History, View::Playlist("PL_new".into())] {
            let mut cache = PageCache::default();
            let mut feed = FeedPage::default();
            feed.append(&current, page(&["keep-me"], Some("next")));
            cache.put(&edited, &feed);
            cache.put(&current, &feed);
            assert_eq!(
                cache.invalidate_playlist("PL_old", &current, &mut feed),
                None
            );
            assert_eq!(feed.videos[0].id, "keep-me");
            assert_eq!(feed.continuation.as_deref(), Some("next"));
            assert!(cache.get(&edited).is_none());
            assert!(cache.get(&current).is_some());
        }
    }

    #[test]
    fn pagination_cycles_are_rejected_across_cached_load_more_requests() {
        let view = View::History;
        let mut feed = FeedPage::default();
        feed.accept(&view, None, page(&["a"], Some("first")))
            .unwrap();
        let mut cache = PageCache::default();
        cache.put(&view, &feed);
        let mut feed = cache.get(&view).unwrap();
        assert!(
            feed.accept(&view, Some("first"), page(&["b"], Some("first")))
                .is_err()
        );
        assert_eq!(feed.videos.len(), 1);
        feed.accept(&view, Some("first"), page(&["b"], Some("second")))
            .unwrap();
        cache.put(&view, &feed);
        let mut feed = cache.get(&view).unwrap();
        assert!(
            feed.accept(&view, Some("second"), page(&["c"], Some("first")))
                .is_err()
        );
        assert_eq!(feed.videos.len(), 2);
        assert_eq!(feed.continuation.as_deref(), Some("second"));
        feed.accept(&view, Some("second"), page(&["c"], None))
            .unwrap();
        assert_eq!(feed.videos.len(), 3);
    }

    fn page(ids: &[&str], token: Option<&str>) -> Page {
        Page {
            videos: ids
                .iter()
                .map(|id| Video {
                    id: (*id).into(),
                    title: format!("Title {id}"),
                    ..Default::default()
                })
                .collect(),
            continuation: token.map(str::to_owned),
            visitor_data: Some("visitor".into()),
            title: "Example playlist".into(),
            ..Default::default()
        }
    }

    #[test]
    fn navigation_restores_all_loaded_pages_and_refresh_replaces_deleted_items() {
        let view = View::Playlist("PL_example".into());
        let mut cached = FeedPage::default();
        cached.append(&view, page(&["a", "b"], Some("page-two")));
        cached.append(&view, page(&["a", "c"], Some("page-three")));
        let mut cache = PageCache::default();
        cache.put(&view, &cached);
        cache.put(&View::History, &FeedPage::default());
        let restored = cache.get(&view).unwrap();
        assert_eq!(
            restored
                .videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "a", "c"]
        );
        assert_eq!(restored.pages, 2);
        assert_eq!(restored.continuation.as_deref(), Some("page-three"));
        assert_eq!(restored.title, "Example playlist");
        let mut refreshed = FeedPage::default();
        refreshed.append(&view, page(&["a", "c"], None));
        cache.put(&view, &refreshed);
        assert_eq!(cache.get(&view).unwrap().videos.len(), 2);
        cache.clear();
        assert!(cache.get(&view).is_none());
        assert!(cache.get(&View::History).is_none());
    }

    #[test]
    fn saved_playlists_deduplicate_but_history_keeps_repeat_watches() {
        let mut playlists = FeedPage::default();
        playlists.append(&View::Playlists, page(&["a", "a"], None));
        assert_eq!(playlists.videos.len(), 1);
        let mut history = FeedPage::default();
        history.append(&View::History, page(&["a", "a"], None));
        assert_eq!(history.videos.len(), 2);
    }
    #[test]
    fn home_and_search_pages_restore_without_mixing_queries() {
        let mut cache = PageCache::default();
        for (view, id) in [
            (View::Home, "home"),
            (View::Search("first query".into()), "first"),
            (View::Search("second query".into()), "second"),
        ] {
            let mut feed = FeedPage::default();
            feed.append(&view, page(&[id], None));
            cache.put(&view, &feed);
        }
        assert_eq!(cache.get(&View::Home).unwrap().videos[0].id, "home");
        assert_eq!(
            cache
                .get(&View::Search("first query".into()))
                .unwrap()
                .videos[0]
                .id,
            "first"
        );
        assert!(cache.get(&View::Search("unknown query".into())).is_none());
    }
}
