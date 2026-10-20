use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rustypipe::{
    client::{ClientType, RustyPipe},
    model::{
        PlaylistItem,
        paginator::{ContinuationEndpoint, Paginator},
    },
};
use serde_json::{Value, json};

use crate::{
    browser::Selection,
    cookies,
    history::{self, HistoryCache, HistoryStore, IdentityCapture, SyncPass},
    watch::{UNKNOWN, UNWATCHED, WATCHED, WatchIndex},
};
mod pages;
pub(crate) mod renderers;
pub use pages::{ChannelInfo, avatar_url, item as video_item, number};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub metadata: String,
    pub duration: String,
    pub thumbnail: String,
    pub playlist: bool,
    pub watched: i32,
    pub watch_hint: i32,
    pub watch_progress: Option<u8>,
    #[serde(skip)]
    pub observed_at: Option<Instant>,
    pub channel_id: String,
    pub avatar: String,
    pub group: String,
    pub set_id: String,
    pub subscribed: bool,
}

#[derive(Default)]
pub struct Page {
    pub videos: Vec<Video>,
    pub continuation: Option<String>,
    pub visitor_data: Option<String>,
    pub title: String,
    pub channel_info: Option<ChannelInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum View {
    Home,
    History,
    Search(String),
    Playlists,
    Playlist(String),
    Subscriptions,
    Channels,
    Channel {
        id: String,
        tab: i32,
        sort: i32,
        query: String,
    },
}

impl View {
    pub fn kind(&self) -> i32 {
        match self {
            Self::Home => 0,
            Self::History => 1,
            Self::Search(_) => 2,
            Self::Playlists => 3,
            Self::Playlist(_) => 4,
            Self::Subscriptions => 5,
            Self::Channels => 6,
            Self::Channel { .. } => 8,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Home => "Recommended for you".into(),
            Self::History => "Watch history".into(),
            Self::Search(query) => format!("Results for “{query}”"),
            Self::Playlists => "Your playlists".into(),
            Self::Playlist(_) => "Playlist".into(),
            Self::Subscriptions => "Subscriptions".into(),
            Self::Channels => "All channels".into(),
            Self::Channel { .. } => "Channel".into(),
        }
    }

    pub fn subtitle(&self) -> String {
        match self {
            Self::Home => "A home feed from your YouTube account".into(),
            Self::History => "Videos you’ve watched on YouTube".into(),
            Self::Search(_) => "Videos and playlists from YouTube".into(),
            Self::Playlists => "Open a saved playlist or paste a playlist link".into(),
            Self::Playlist(id) => format!("Playlist · {id}"),
            Self::Subscriptions => "Latest uploads".into(),
            Self::Channels => "Subscribed channels".into(),
            Self::Channel { .. } => String::new(),
        }
    }
}

pub fn playlist_id(input: &str) -> Result<String> {
    let input = input.trim();
    let id = if input.contains("://") {
        let url =
            reqwest::Url::parse(input).context("Enter a YouTube playlist link or playlist ID.")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.host_str().is_some_and(|h| {
                h == "youtube.com" || h.ends_with(".youtube.com") || h == "youtu.be"
            })
        {
            bail!("Enter a YouTube playlist link or playlist ID.");
        }
        url.query_pairs().find(|(key, _)| key == "list").map(|(_, value)| value.into_owned()).context("This YouTube link has no playlist. Paste a link containing list= or a playlist ID.")?
    } else {
        input.to_owned()
    };
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("Enter a valid YouTube playlist ID.");
    }
    Ok(id)
}

pub struct Session {
    pub cache: Option<crate::cache::Cache>,
    pub progress: Option<Arc<crate::player::progress::Client>>,
    playback_cookies: Arc<str>,
    client: RustyPipe,
    watch_index: Arc<RwLock<WatchIndex>>,
    watch_updates: tokio::sync::watch::Receiver<u64>,
    watch_task: tokio::task::JoinHandle<()>,
    watch_refresh: Arc<tokio::sync::Notify>,
    known_subscriptions: RwLock<std::collections::HashMap<String, bool>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.watch_task.abort();
    }
}

pub(crate) async fn authenticated_client(
    selection: &Selection,
) -> Result<(RustyPipe, String, IdentityCapture)> {
    authenticate(selection, || {}).await
}

/// `session_read` runs once the browser cookies are loaded, before YouTube is contacted.
async fn authenticate(
    selection: &Selection,
    session_read: impl FnOnce(),
) -> Result<(RustyPipe, String, IdentityCapture)> {
    let cookies = cookies::read(selection).await?;
    let identity = IdentityCapture::default();
    let client = RustyPipe::builder()
        .storage(Box::new(identity.clone()))
        .no_reporter()
        .build()?;
    client.user_auth_set_cookie_txt(&cookies).await
            .map_err(|_| anyhow::anyhow!("This profile has no usable YouTube sign-in. Sign in to YouTube in that profile and retry."))?;
    session_read();
    client.user_auth_check_cookie().await
            .map_err(|_| anyhow::anyhow!("Could not verify this YouTube session. Check your internet connection and sign in to YouTube in the selected profile, then retry."))?;
    Ok((client, cookies, identity))
}

impl Session {
    pub async fn connect(selection: &Selection, session_read: impl FnOnce()) -> Result<Self> {
        let (client, cookies, identity) = authenticate(selection, session_read).await?;
        let cache_store = identity
            .account()
            .and_then(|account| crate::cache::Cache::new(&account.filename()));
        let store = identity
            .account()
            .and_then(|account| HistoryStore::new(account).ok());
        let store_for_load = store.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            store_for_load.as_ref().map(HistoryStore::load).transpose()
        })
        .await;
        let (cache, unavailable) = match loaded {
            Ok(Ok(cache)) => (cache.flatten().unwrap_or_default(), store.is_none()),
            _ => (HistoryCache::default(), true),
        };
        let mut index = WatchIndex {
            cache_unavailable: unavailable,
            ..Default::default()
        };
        index.begin(
            &cache,
            SyncPass::new(&cache, history::now()).full,
            history::now(),
        );
        let watch_index = Arc::new(RwLock::new(index));
        let watch_refresh = Arc::new(tokio::sync::Notify::new());
        let (updates, watch_updates) = tokio::sync::watch::channel(0);
        let watch_task = tokio::spawn(sync_history(
            client.clone(),
            watch_index.clone(),
            updates,
            store,
            cache,
            watch_refresh.clone(),
        ));
        let progress =
            crate::player::progress::Client::new(client.clone(), &cookies, &identity).ok();
        Ok(Self {
            progress,
            cache: cache_store,
            playback_cookies: cookies.into(),
            client,
            watch_index,
            watch_updates,
            watch_task,
            watch_refresh,
            known_subscriptions: RwLock::default(),
        })
    }

    pub fn refresh_watch_history(&self) {
        self.watch_refresh.notify_one();
    }

    pub async fn cached_feed(&self, view: &View) -> Option<crate::feed::FeedPage> {
        let key = serde_json::to_string(view).ok()?;
        let mut page: crate::feed::FeedPage = self.cache.as_ref()?.get("pages-v1", &key).await?;
        // Old response hints must not override a newer history reconciliation.
        for video in &mut page.videos {
            video.watch_hint = UNKNOWN;
            video.watched = UNKNOWN;
            video.watch_progress = None;
        }
        self.annotate(&mut page.videos);
        Some(page)
    }

    pub fn cache_feed(&self, view: &View, page: &crate::feed::FeedPage) {
        if let Some(cache) = &self.cache
            && let Ok(key) = serde_json::to_string(view)
        {
            cache.put(
                "pages-v1",
                &key,
                &page.disk_snapshot(),
                7 * crate::cache::DAY,
            );
        }
    }

    pub fn playback_cookies(&self) -> Arc<str> {
        self.playback_cookies.clone()
    }

    pub fn watch_updates(&self) -> tokio::sync::watch::Receiver<u64> {
        self.watch_updates.clone()
    }

    pub fn annotate(&self, videos: &mut [Video]) {
        self.watch_index.read().unwrap().annotate(videos);
    }

    pub fn watch_note(&self) -> String {
        self.watch_index.read().unwrap().note()
    }

    pub async fn page(
        &self,
        view: &View,
        continuation: Option<&str>,
        visitor: Option<&str>,
    ) -> Result<Page> {
        let started = Instant::now();
        let mut page = self.page_inner(view, continuation, visitor).await?;
        for video in &mut page.videos {
            video.observed_at = Some(started);
        }
        Ok(page)
    }

    async fn page_inner(
        &self,
        view: &View,
        continuation: Option<&str>,
        visitor: Option<&str>,
    ) -> Result<Page> {
        if matches!(
            view,
            View::Subscriptions | View::Channels | View::Channel { .. }
        ) {
            return self.extra_page(view, continuation, visitor).await;
        }
        if *view == View::History {
            history_page(&self.client, continuation, visitor).await
        } else if *view == View::Playlists {
            let query = self.client.query().authenticated();
            let result: Paginator<PlaylistItem> = if let Some(token) = continuation {
                query.continuation(token, ContinuationEndpoint::Browse, visitor).await
            } else { query.saved_playlists().await }
                .map_err(|_| anyhow::anyhow!("Your playlists could not be loaded. Check your connection or reconnect your browser."))?;
            let mut page = Page {
                videos: result
                    .items
                    .into_iter()
                    .map(|item| Video {
                        id: item.id,
                        title: item.name,
                        channel: item.channel.map(|c| c.name).unwrap_or_default(),
                        metadata: item
                            .video_count
                            .map(|n| format!("{n} videos"))
                            .unwrap_or_default(),
                        thumbnail: item
                            .thumbnail
                            .iter()
                            .max_by_key(|t| t.width)
                            .map(|t| t.url.clone())
                            .unwrap_or_default(),
                        playlist: true,
                        ..Default::default()
                    })
                    .collect(),
                continuation: result.ctoken,
                visitor_data: result.visitor_data,
                title: String::new(),
                channel_info: None,
            };
            if continuation.is_none() {
                for (id, title) in [("LL", "Liked videos"), ("WL", "Watch later")] {
                    let video = if let Some(index) = page.videos.iter().position(|v| v.id == id) {
                        page.videos.remove(index)
                    } else {
                        Video {
                            id: id.into(),
                            title: title.into(),
                            playlist: true,
                            ..Default::default()
                        }
                    };
                    page.videos.insert(0, video);
                }
            }
            Ok(page)
        } else {
            let body = if let Some(token) = continuation {
                json!({"continuation": token})
            } else {
                match view {
                    View::Home => json!({"browseId": "FEwhat_to_watch"}),
                    View::Search(query) => json!({"query": query, "params": "8AEB"}),
                    View::Playlist(id) => json!({"browseId": format!("VL{id}")}),
                    _ => unreachable!(),
                }
            };
            let endpoint = if matches!(view, View::Search(_)) {
                "search"
            } else {
                "browse"
            };
            let response = self
                .client
                .query()
                .authenticated()
                .visitor_data_opt(visitor)
                .raw(ClientType::Desktop, endpoint, &body)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "YouTube could not load this page. Check your connection and retry."
                    )
                })?;
            if matches!(view, View::Playlist(_)) {
                parse_response(&response, true)
            } else {
                parse_home(&response)
            }
        }
    }
}

// Display and indexing deliberately share this request and parser. A lossy
// secondary parser must never turn an incomplete history into a complete cache.
async fn history_page(
    client: &RustyPipe,
    token: Option<&str>,
    visitor: Option<&str>,
) -> Result<Page> {
    let body = token
        .map(|token| json!({"continuation": token}))
        .unwrap_or_else(|| json!({"browseId": "FEhistory"}));
    let response = client.query().authenticated().visitor_data_opt(visitor)
        .raw(ClientType::Desktop, "browse", &body).await
        .map_err(|_| anyhow::anyhow!("YouTube history could not be loaded. Check your connection or reconnect your browser profile."))?;
    parse_page(&response, ParseMode::History)
}

async fn save_history(store: &Option<HistoryStore>, cache: &HistoryCache) -> bool {
    let Some(store) = store.clone() else {
        return false;
    };
    let cache = cache.clone();
    matches!(
        tokio::task::spawn_blocking(move || store.save(&cache)).await,
        Ok(Ok(()))
    )
}

async fn sync_history(
    client: RustyPipe,
    index: Arc<RwLock<WatchIndex>>,
    updates: tokio::sync::watch::Sender<u64>,
    store: Option<HistoryStore>,
    mut cache: HistoryCache,
    refresh: Arc<tokio::sync::Notify>,
) {
    let mut version = 0_u64;
    loop {
        let pass_started = Instant::now();
        let mut pass = SyncPass::new(&cache, history::now());
        index
            .write()
            .unwrap()
            .begin(&cache, pass.full, history::now());
        updates.send_replace({
            version += 1;
            version
        });
        let mut token = None::<String>;
        let mut visitor = None::<String>;
        let mut seen_tokens = HashSet::new();
        let mut finished = false;
        // Only a successful end-of-history response may remove cached IDs.
        // Invalid IDs, repeated tokens and timeouts keep uncertainty. Large
        // accounts must be able to reach the end rather than restart at a cap.
        for page_number in 1_u64.. {
            let request = history_page(&client, token.as_deref(), visitor.as_deref());
            let Ok(Ok(page)) = tokio::time::timeout(Duration::from_secs(60), request).await else {
                break;
            };
            let ids: Vec<_> = page
                .videos
                .into_iter()
                .filter(|v| !v.playlist)
                .map(|v| v.id)
                .collect();
            let Ok(overlap) = pass.page(&ids) else {
                break;
            };
            index.write().unwrap().ids.extend(ids);
            let reached_end = page.continuation.is_none();
            if reached_end || overlap {
                cache = pass.finish(&cache, reached_end, history::now());
                let saved = save_history(&store, &cache).await;
                {
                    let mut state = index.write().unwrap();
                    state.ids = cache.ids.clone();
                    if reached_end {
                        state.hints_since = Some(pass_started);
                    }
                    state.complete = true;
                    state.failed = false;
                    state.phase = crate::watch::Phase::Idle;
                    state.cache_unavailable = !saved;
                }
                finished = true;
                updates.send_replace({
                    version += 1;
                    version
                });
                break;
            }
            updates.send_replace({
                version += 1;
                version
            });
            // Save initial progress without claiming the index is complete.
            if cache.last_full_sync.is_none() && page_number % 10 == 0 {
                let partial = pass.partial(&cache, history::now());
                let saved = save_history(&store, &partial).await;
                index.write().unwrap().cache_unavailable = !saved;
            }
            let Some(next) = page.continuation else {
                unreachable!();
            };
            if !seen_tokens.insert(next.clone()) {
                break;
            }
            token = Some(next);
            if page.visitor_data.is_some() {
                visitor = page.visitor_data;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !finished {
            let partial = pass.partial(&cache, history::now());
            let saved = save_history(&store, &partial).await;
            cache = partial;
            let mut state = index.write().unwrap();
            state.failed = true;
            state.cache_unavailable = !saved;
            updates.send_replace({
                version += 1;
                version
            });
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10 * 60)) => {},
            _ = refresh.notified() => {},
        }
    }
}

pub fn parse_home(response: &str) -> Result<Page> {
    parse_response(response, false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParseMode {
    Feed,
    Playlist,
    History,
}

fn parse_response(response: &str, preserve_duplicates: bool) -> Result<Page> {
    parse_page(
        response,
        if preserve_duplicates {
            ParseMode::Playlist
        } else {
            ParseMode::Feed
        },
    )
}

fn parse_page(response: &str, mode: ParseMode) -> Result<Page> {
    let preserve_duplicates = mode != ParseMode::Feed;
    let history_date = if mode == ParseMode::History {
        Some("")
    } else {
        None
    };
    let response: Value =
        serde_json::from_str(response).context("YouTube returned an unreadable page.")?;
    if response.get("error").is_some() {
        bail!("YouTube rejected the request. Try reconnecting your browser profile.");
    }
    if response
        .pointer("/responseContext/mainAppWebResponseContext/loggedOut")
        .and_then(Value::as_bool)
        == Some(true)
    {
        bail!(
            "YouTube has signed this profile out. Sign in again in your browser, then reconnect."
        );
    }
    if response["alerts"].as_array().is_some_and(|alerts| {
        alerts
            .iter()
            .any(|a| a["alertRenderer"]["type"].as_str() == Some("ERROR"))
    }) {
        if mode == ParseMode::Playlist {
            bail!("This playlist is unavailable or private to another account.");
        }
        bail!("YouTube could not load this page. Please refresh or reconnect your browser.");
    }
    let title = response
        .pointer("/metadata/playlistMetadataRenderer/title")
        .map(text)
        .or_else(|| {
            response
                .pointer("/header/playlistHeaderRenderer/title")
                .map(text)
        })
        .or_else(|| {
            response
                .pointer("/header/pageHeaderRenderer/pageTitle")
                .map(text)
        })
        .unwrap_or_default();
    let mut page = Page {
        title: if mode == ParseMode::History {
            String::new()
        } else {
            title
        },
        visitor_data: response
            .pointer("/responseContext/visitorData")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..Page::default()
    };
    let mut seen = HashSet::new();
    let mut recognized = false;
    let has_continuation_items = [
        "onResponseReceivedActions",
        "onResponseReceivedCommands",
        "onResponseReceivedEndpoints",
    ]
    .iter()
    .any(|key| {
        response[*key].as_array().is_some_and(|actions| {
            actions.iter().any(|a| {
                a.get("appendContinuationItemsAction").is_some()
                    || a.get("reloadContinuationItemsCommand").is_some()
            })
        })
    });
    for key in [
        "contents",
        "onResponseReceivedActions",
        "onResponseReceivedCommands",
        "onResponseReceivedEndpoints",
        "continuationContents",
    ] {
        if let Some(root) = response.get(key) {
            // Continuation responses can include a selected-tab placeholder
            // with no content. The actual items live in the actions below.
            // A placeholder alone must still fail rather than erase the cache.
            if key == "contents" && has_continuation_items && browse_placeholder(root) {
                continue;
            }
            if key.starts_with("onResponseReceived") {
                let actions = root
                    .as_array()
                    .context("YouTube returned an unfamiliar continuation response.")?;
                if actions.is_empty() {
                    recognized = true;
                }
                for action in actions {
                    // Telemetry and advertisement commands accompany real page
                    // content; their presence alone is not an empty page.
                    if action.get("appendContinuationItemsAction").is_some()
                        || action.get("reloadContinuationItemsCommand").is_some()
                    {
                        recognized = true;
                        collect(
                            action,
                            &mut page,
                            &mut seen,
                            preserve_duplicates,
                            history_date,
                        )?;
                    }
                }
                continue;
            }
            recognized = true;
            // Browse tabs and search sidebars can contain unrelated suggestions.
            let primary = if key == "contents" {
                browse_primary(root, mode).unwrap_or(root)
            } else {
                root
            };
            collect(
                primary,
                &mut page,
                &mut seen,
                preserve_duplicates,
                history_date,
            )?;
        }
    }
    if !recognized {
        bail!("YouTube returned an unfamiliar response. Please refresh or reconnect your browser.");
    }
    Ok(page)
}

fn browse_placeholder(contents: &Value) -> bool {
    contents
        .pointer("/twoColumnBrowseResultsRenderer/tabs")
        .or_else(|| contents.pointer("/singleColumnBrowseResultsRenderer/tabs"))
        .and_then(Value::as_array)
        .is_some_and(|tabs| {
            !tabs.is_empty()
                && tabs.iter().all(|tab| {
                    tab.get("tabRenderer")
                        .and_then(Value::as_object)
                        .is_some_and(|tab| {
                            tab.keys().all(|key| {
                                matches!(
                                    key.as_str(),
                                    "selected"
                                        | "trackingParams"
                                        | "title"
                                        | "endpoint"
                                        | "tabIdentifier"
                                )
                            })
                        })
                })
        })
}

fn browse_primary(contents: &Value, mode: ParseMode) -> Option<&Value> {
    let tabs = contents
        .pointer("/twoColumnBrowseResultsRenderer/tabs")
        .or_else(|| contents.pointer("/singleColumnBrowseResultsRenderer/tabs"))?
        .as_array()?;
    let selected = tabs
        .iter()
        .find(|tab| {
            tab.pointer("/tabRenderer/selected")
                .and_then(Value::as_bool)
                == Some(true)
        })
        .or_else(|| tabs.first())?;
    let content = selected.pointer("/tabRenderer/content")?;
    if mode == ParseMode::Playlist {
        // The first section is the playlist itself. Later sections and their
        // continuation tokens load recommendations, not more playlist videos.
        // Newer layouts put lockups directly in this section; older ones wrap
        // them in playlistVideoListRenderer. Rich-grid layouts need no unwrap.
        if let Some(section) =
            content.pointer("/sectionListRenderer/contents/0/itemSectionRenderer")
        {
            return Some(section);
        }
    }
    Some(content)
}

pub(crate) fn find_renderer<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_renderer(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_renderer(v, key)),
        _ => None,
    }
}

fn collect(
    value: &Value,
    page: &mut Page,
    seen: &mut HashSet<String>,
    preserve_duplicates: bool,
    history_date: Option<&str>,
) -> Result<()> {
    let unfamiliar = || {
        #[cfg(test)]
        eprintln!("Unfamiliar content structure: {}", tests::schema(value, 4));
        anyhow::anyhow!(
            "YouTube returned an unfamiliar page format. Please refresh or update the app."
        )
    };
    match value {
        Value::Array(items) => {
            for item in items {
                collect(item, page, seen, preserve_duplicates, history_date)?;
            }
            Ok(())
        }
        Value::Object(object) => {
            // These are explicitly non-video items, not unrecognized video data.
            if [
                "miniGameCardViewModel",
                "adSlotRenderer",
                "promotedSparklesWebRenderer",
                "adPlacements",
                "channelRenderer",
                "radioRenderer",
                "messageRenderer",
                "backgroundPromoRenderer",
                "feedNudgeRenderer",
                "searchPyvRenderer",
                "didYouMeanRenderer",
                "showingResultsForRenderer",
                "continuationItemRendererDisabled",
            ]
            .iter()
            .any(|key| object.contains_key(*key))
            {
                return Ok(());
            }
            let item = if let Some(r) = object
                .get("videoRenderer")
                .or_else(|| object.get("gridVideoRenderer"))
                .or_else(|| object.get("playlistVideoRenderer"))
                .or_else(|| object.get("reelItemRenderer"))
            {
                if r["style"].as_str() == Some("PLAYLIST_VIDEO_RENDERER_STYLE_RECOMMENDED_VIDEO")
                    || r["isPlayable"].as_bool() == Some(false)
                {
                    return Ok(());
                }
                Some(legacy_video(r).ok_or_else(unfamiliar)?)
            } else if let Some(r) = object
                .get("playlistRenderer")
                .or_else(|| object.get("gridPlaylistRenderer"))
            {
                Some(legacy_playlist(r).ok_or_else(unfamiliar)?)
            } else if let Some(r) = object.get("lockupViewModel") {
                // Channel cards are intentionally outside the app's video/playlist search.
                if r["contentType"].as_str() == Some("LOCKUP_CONTENT_TYPE_CHANNEL") {
                    return Ok(());
                }
                Some(lockup_video(r).ok_or_else(unfamiliar)?)
            } else if let Some(r) = object.get("shortsLockupViewModel") {
                Some(shorts_video(r).ok_or_else(unfamiliar)?)
            } else {
                None
            };
            if let Some(mut video) = item {
                mark_history(&mut video, history_date);
                if video.title.is_empty()
                    || playlist_id(&video.id).is_err()
                    || (!video.playlist && video.id.len() != 11)
                {
                    return Err(unfamiliar());
                }
                push(video, page, seen, preserve_duplicates);
                return Ok(());
            }
            let continuation = if let Some(r) = object.get("continuationItemRenderer") {
                Some(r.pointer("/continuationEndpoint/continuationCommand/token"))
            } else if let Some(r) = object.get("continuationItemViewModel") {
                Some(
                    r.pointer("/continuationCommand/innertubeCommand/continuationCommand/token")
                        .or_else(|| r.pointer("/continuationCommand/continuationCommand/token"))
                        .or_else(|| r.pointer("/continuationCommand/token")),
                )
            } else {
                object
                    .get("nextContinuationData")
                    .map(|r| r.get("continuation"))
            };
            if let Some(token) = continuation {
                page.continuation = Some(
                    token
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .ok_or_else(unfamiliar)?
                        .to_owned(),
                );
                return Ok(());
            }
            if let Some(section) = object.get("itemSectionRenderer") {
                let date = text(&section["header"]["itemSectionHeaderRenderer"]["title"]);
                let date = history_date.map(|previous| {
                    if date.is_empty() {
                        previous
                    } else {
                        date.as_str()
                    }
                });
                return collect(
                    section
                        .get("contents")
                        .or_else(|| section.get("items"))
                        .ok_or_else(unfamiliar)?,
                    page,
                    seen,
                    preserve_duplicates,
                    date,
                );
            }
            if let Some(search) = object.get("twoColumnSearchResultsRenderer") {
                return collect(
                    search.get("primaryContents").ok_or_else(unfamiliar)?,
                    page,
                    seen,
                    preserve_duplicates,
                    history_date,
                );
            }
            if let Some(shelf) = object.get("chipsShelfWithVideoShelfRenderer") {
                // Home continuations can contain a category selector without
                // any video shelf yet. Its chips/header are controls, not cards.
                // Still parse content when supplied, and reject unknown fields
                // rather than silently dropping a future video-shelf variant.
                if shelf
                    .pointer("/chipsShelf/chipsShelfViewModel/contents")
                    .and_then(Value::as_array)
                    .is_none()
                {
                    return Err(unfamiliar());
                }
                for (key, child) in shelf.as_object().ok_or_else(unfamiliar)? {
                    match key.as_str() {
                        "chipsShelf" | "header" | "contentId" | "targetId" | "trackingParams" => {}
                        "contents" | "items" | "content" | "continuations" => {
                            collect(child, page, seen, preserve_duplicates, history_date)?;
                        }
                        _ => return Err(unfamiliar()),
                    }
                }
                return Ok(());
            }
            // Only descend through known content containers. Unknown wrappers
            // fail the page instead of silently claiming it is empty/complete.
            for key in [
                "richGridRenderer",
                "sectionListRenderer",
                "playlistVideoListRenderer",
                "richItemRenderer",
                "richSectionRenderer",
                "richShelfRenderer",
                "brandVideoShelfRenderer",
                "reelShelfRenderer",
                "shelfRenderer",
                "expandedShelfContentsRenderer",
                "horizontalListRenderer",
                "verticalListRenderer",
                "gridRenderer",
                "gridShelfViewModel",
                "singleColumnSearchResultsRenderer",
                "appendContinuationItemsAction",
                "reloadContinuationItemsCommand",
                "richGridContinuation",
                "sectionListContinuation",
                "itemSectionContinuation",
                "playlistVideoListContinuation",
            ] {
                if let Some(child) = object.get(key) {
                    return collect(child, page, seen, preserve_duplicates, history_date);
                }
            }
            let mut found = false;
            for key in [
                "contents",
                "items",
                "content",
                "continuationItems",
                "continuations",
            ] {
                if let Some(child) = object.get(key) {
                    found = true;
                    collect(child, page, seen, preserve_duplicates, history_date)?;
                }
            }
            if found { Ok(()) } else { Err(unfamiliar()) }
        }
        _ => Err(unfamiliar()),
    }
}

fn mark_history(video: &mut Video, date: Option<&str>) {
    if let Some(date) = date
        && !video.playlist
    {
        video.watched = WATCHED;
        video.watch_hint = WATCHED;
        if !date.is_empty() {
            video.group = date.to_owned();
            video.metadata = format!("Watched {date}");
        }
    }
}

fn push(video: Video, page: &mut Page, seen: &mut HashSet<String>, preserve_duplicates: bool) {
    let valid_id = if video.playlist {
        !video.id.is_empty()
    } else {
        video.id.len() == 11
    };
    let key = format!("{}:{}", video.playlist, video.id);
    if valid_id && !video.title.is_empty() && (preserve_duplicates || seen.insert(key)) {
        page.videos.push(video);
    }
}

pub(crate) fn text(value: &Value) -> String {
    if let Some(text) = value
        .as_str()
        .or_else(|| value.get("simpleText").and_then(Value::as_str))
        .or_else(|| value.get("content").and_then(Value::as_str))
    {
        return text.into();
    }
    value
        .get("runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|r| r.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

fn thumbnail(value: &Value) -> String {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .filter(|t| t["width"].as_u64().unwrap_or(0) <= 640)
                .max_by_key(|t| t["width"].as_u64().unwrap_or(0))
                .or_else(|| items.first())
        })
        .and_then(|t| t["url"].as_str())
        .map(absolute)
        .unwrap_or_default()
}

/// The smallest avatar that still looks sharp at 80px on a 2x display.
pub(crate) fn avatar(value: &Value) -> String {
    let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
    items
        .iter()
        .filter(|t| t["width"].as_u64().unwrap_or(0) >= 160)
        .min_by_key(|t| t["width"].as_u64().unwrap_or(0))
        .or_else(|| {
            items
                .iter()
                .max_by_key(|t| t["width"].as_u64().unwrap_or(0))
        })
        .and_then(|t| t["url"].as_str())
        .map(absolute)
        .unwrap_or_default()
}

fn absolute(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.into()
    }
}

fn legacy_video(r: &Value) -> Option<Video> {
    let (watched, watch_progress) = watch_marker(r);
    Some(Video {
        id: r["videoId"].as_str()?.into(),
        title: text(
            r.get("title")
                .or_else(|| r.get("headline"))
                .unwrap_or(&Value::Null),
        ),
        channel: text(
            r.get("ownerText")
                .or_else(|| r.get("shortBylineText"))
                .unwrap_or(&Value::Null),
        ),
        metadata: legacy_metadata(r),
        duration: text(&r["lengthText"]),
        thumbnail: thumbnail(&r["thumbnail"]["thumbnails"]),
        channel_id: renderer_channel(r),
        avatar: avatar(
            r.pointer("/channelThumbnailSupportedRenderers/channelThumbnailWithLinkRenderer/thumbnail/thumbnails")
                .or_else(|| r.pointer("/channelThumbnail/thumbnails"))
                .unwrap_or(&Value::Null),
        ),
        set_id: r["setVideoId"].as_str().unwrap_or_default().into(),
        watched,
        watch_hint: watched,
        watch_progress,
        ..Default::default()
    })
}

/// Views and age, or the `videoInfo` runs that playlist rows use instead.
fn legacy_metadata(r: &Value) -> String {
    let parts: Vec<String> = [text(&r["viewCountText"]), text(&r["publishedTimeText"])]
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect();
    if !parts.is_empty() {
        return parts.join(" · ");
    }
    r["videoInfo"]["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["text"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "•" && *t != "·")
        .collect::<Vec<_>>()
        .join(" · ")
}

fn renderer_channel(value: &Value) -> String {
    if let Some(id) = value
        .pointer("/browseEndpoint/browseId")
        .and_then(Value::as_str)
        && id.starts_with("UC")
        && id.len() == 24
    {
        return id.into();
    }
    match value {
        Value::Object(map) => map
            .values()
            .map(renderer_channel)
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
        Value::Array(items) => items
            .iter()
            .map(renderer_channel)
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn legacy_playlist(r: &Value) -> Option<Video> {
    let images = r
        .pointer("/thumbnails/0/thumbnails")
        .or_else(|| r.pointer("/thumbnail/thumbnails"))
        .or_else(|| {
            r.pointer("/thumbnailRenderer/playlistVideoThumbnailRenderer/thumbnail/thumbnails")
        })
        .unwrap_or(&Value::Null);
    Some(Video {
        id: r["playlistId"].as_str()?.into(),
        title: text(
            r.get("title")
                .or_else(|| r.get("headline"))
                .unwrap_or(&Value::Null),
        ),
        channel: text(&r["shortBylineText"]),
        metadata: text(&r["videoCountText"]),
        thumbnail: thumbnail(images),
        playlist: true,
        ..Default::default()
    })
}

fn lockup_video(r: &Value) -> Option<Video> {
    let playlist = r["contentType"].as_str() == Some("LOCKUP_CONTENT_TYPE_PLAYLIST");
    if r["contentType"]
        .as_str()
        .is_some_and(|kind| kind != "LOCKUP_CONTENT_TYPE_VIDEO" && !playlist)
    {
        return None;
    }
    let metadata = &r["metadata"]["lockupMetadataViewModel"];
    let rows = metadata
        .pointer("/metadata/contentMetadataViewModel/metadataRows")
        .and_then(Value::as_array);
    let row_texts: Vec<String> = rows
        .into_iter()
        .flatten()
        .map(|row| {
            row["metadataParts"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .map(|p| text(&p["text"]))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .unwrap_or_default()
        })
        .collect();
    let image = if playlist {
        r.pointer("/contentImage/collectionThumbnailViewModel/primaryThumbnail/thumbnailViewModel")
            .unwrap_or(&r["contentImage"]["thumbnailViewModel"])
    } else {
        &r["contentImage"]["thumbnailViewModel"]
    };
    let (watched, watch_progress) = if playlist {
        (UNKNOWN, None)
    } else {
        watch_marker(r)
    };
    let first_part = metadata
        .pointer("/metadata/contentMetadataViewModel/metadataRows/0/metadataParts/0/text")
        .map(text)
        .unwrap_or_default();
    let count_only = first_part.contains(" views")
        || first_part.contains(" watching")
        || first_part.ends_with(" videos");
    let channel_label = if count_only {
        String::new()
    } else {
        first_part
    };
    let combined_metadata = if count_only {
        row_texts.join(" · ")
    } else {
        let inline_metadata = rows
            .and_then(|rows| rows.first())
            .and_then(|row| row["metadataParts"].as_array())
            .into_iter()
            .flatten()
            .skip(1)
            .map(|p| text(&p["text"]))
            .filter(|s| !s.is_empty());
        inline_metadata
            .chain(row_texts.iter().skip(1).cloned())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    Some(Video {
        id: r["contentId"].as_str()?.into(),
        title: text(&metadata["title"]),
        channel: channel_label,
        metadata: combined_metadata,
        // Badges have moved between thumbnailOverlayBadgeViewModel and
        // thumbnailBottomOverlayViewModel, so search the overlays for any badge.
        duration: find_renderer(&image["overlays"], "thumbnailBadgeViewModel")
            .map(|badge| text(&badge["text"]))
            .unwrap_or_default(),
        thumbnail: thumbnail(&image["image"]["sources"]),
        playlist,
        watched,
        watch_hint: watched,
        watch_progress,
        observed_at: None,
        channel_id: renderer_channel(metadata),
        avatar: find_renderer(&metadata["image"], "avatarViewModel")
            .map(|view| avatar(&view["image"]["sources"]))
            .unwrap_or_default(),
        ..Default::default()
    })
}

fn shorts_video(r: &Value) -> Option<Video> {
    let endpoint = find_renderer(r.get("onTap")?, "reelWatchEndpoint")?;
    // Current Home/History Shorts wrap thumbnailViewModel in another object
    // of the same name. Keep older layouts and the watch endpoint as fallbacks.
    // Empty source lists must not mask a usable later candidate.
    let thumbnail = [
        r.pointer("/thumbnailViewModel/thumbnailViewModel/image/sources"),
        r.pointer("/thumbnailViewModel/image/sources"),
        r.pointer("/thumbnail/sources"),
        r.pointer("/thumbnail/thumbnails"),
        endpoint.pointer("/thumbnail/thumbnails"),
    ]
    .into_iter()
    .flatten()
    .map(thumbnail)
    .find(|url| !url.is_empty())
    .unwrap_or_default();
    let (watched, watch_progress) = watch_marker(r);
    Some(Video {
        id: endpoint["videoId"].as_str()?.into(),
        title: text(&r["overlayMetadata"]["primaryText"]),
        metadata: text(&r["overlayMetadata"]["secondaryText"]),
        thumbnail,
        duration: "SHORT".into(),
        watched,
        watch_hint: watched,
        watch_progress,
        ..Default::default()
    })
}

fn watch_marker(value: &Value) -> (i32, Option<u8>) {
    fn walk(value: &Value, explicit: &mut Option<bool>, progress: &mut Option<u8>) {
        match value {
            Value::Object(map) => {
                if let Some(watched) = map.get("isWatched").and_then(Value::as_bool) {
                    *explicit = Some(explicit.unwrap_or(false) || watched);
                }
                let percent = map
                    .get("percentDurationWatched")
                    .or_else(|| {
                        map.get("thumbnailOverlayProgressBarViewModel")
                            .and_then(|v| v.get("startPercent").or_else(|| v.get("endPercent")))
                    })
                    .and_then(Value::as_f64);
                if let Some(percent) = percent.filter(|n| (0.0..=100.0).contains(n)) {
                    *progress = Some(progress.unwrap_or(0).max(percent.ceil() as u8));
                }
                if map
                    .get("lastPlaybackPositionMilliseconds")
                    .and_then(|v| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .is_some_and(|n| n > 0.0)
                {
                    *explicit = Some(true);
                }
                for child in map.values() {
                    walk(child, explicit, progress);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, explicit, progress);
                }
            }
            _ => {}
        }
    }
    let (mut explicit, mut progress) = (None, None);
    walk(value, &mut explicit, &mut progress);
    let status = if explicit == Some(true) || progress.is_some_and(|p| p > 0) {
        WATCHED
    } else if explicit == Some(false) || progress == Some(0) {
        UNWATCHED
    } else {
        UNKNOWN
    };
    (status, progress)
}

pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

pub async fn download_thumbnail(http: &reqwest::Client, url: &str) -> Result<Pixels> {
    let url = reqwest::Url::parse(url)?;
    if url.scheme() != "https"
        || !url.host_str().is_some_and(|h| {
            h == "i.ytimg.com"
                || h.ends_with(".ytimg.com")
                || h.ends_with(".googleusercontent.com")
                || h.ends_with(".ggpht.com")
        })
    {
        bail!("Unsupported thumbnail host");
    }
    let cache = crate::cache::Cache::new("public-images");
    let cached = if let Some(cache) = &cache {
        cache.bytes("images-v1", url.as_str()).await
    } else {
        None
    };
    if let Some(bytes) = cached
        && let Ok(pixels) = decode_thumbnail(&bytes)
    {
        return Ok(pixels);
    }
    let bytes = thumbnail_bytes(http, &url).await?;
    let pixels = decode_thumbnail(&bytes)?;
    if let Some(cache) = cache {
        cache.put_bytes("images-v1", url.as_str(), bytes, 30 * crate::cache::DAY);
    }
    Ok(pixels)
}

fn decode_thumbnail(bytes: &[u8]) -> Result<Pixels> {
    let image = image::load_from_memory(bytes)?
        .thumbnail(640, 360)
        .into_rgba8();
    Ok(Pixels {
        width: image.width(),
        height: image.height(),
        bytes: image.into_raw(),
    })
}

async fn thumbnail_bytes(http: &reqwest::Client, url: &reqwest::Url) -> Result<Vec<u8>> {
    for attempt in 0..3 {
        match thumbnail_request(http, url).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                let temporary = error.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                    e.is_timeout()
                        || e.is_connect()
                        || e.is_body()
                        || e.status()
                            .is_some_and(|s| s.as_u16() == 429 || s.is_server_error())
                });
                if attempt == 2 || !temporary {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
            }
        }
    }
    unreachable!()
}

async fn thumbnail_request(http: &reqwest::Client, url: &reqwest::Url) -> Result<Vec<u8>> {
    let response = http
        .get(url.clone())
        .timeout(Duration::from_secs(12))
        .send()
        .await?
        .error_for_status()?;
    if response.content_length().is_some_and(|n| n > 5_000_000) {
        bail!("Thumbnail is too large");
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 5_000_000 {
            bail!("Thumbnail is too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field names, types and array lengths only. Never include response values,
    /// titles, account IDs, URLs, continuation tokens or authentication material.
    pub(super) fn schema(value: &Value, depth: usize) -> Value {
        match value {
            Value::Object(fields) if depth > 0 => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| key.chars().all(|c| c.is_ascii_alphabetic()))
                    .map(|(key, value)| (key.clone(), schema(value, depth - 1)))
                    .collect(),
            ),
            Value::Array(items) if depth > 0 => json!({
                "length": items.len(),
                "first": items.first().map(|v| schema(v, depth - 1)),
            }),
            Value::Object(_) => json!("object"),
            Value::Array(_) => json!("array"),
            Value::String(_) => json!("string"),
            Value::Number(_) => json!("number"),
            Value::Bool(_) => json!("bool"),
            Value::Null => Value::Null,
        }
    }

    #[tokio::test]
    #[ignore = "requires explicit SLINTYT_TEST_PROFILE authorization and network access"]
    async fn live_home_parser_schema() {
        let selection = Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("Set an authorized test profile")
                .into(),
            keyring: None,
        };
        // Authentication stays inside the normal importer and RustyPipe.
        let cookies = cookies::read(&selection).await;
        assert!(cookies.is_ok(), "Could not import the authorized profile");
        let client = RustyPipe::builder()
            .storage(Box::new(IdentityCapture::default()))
            .no_reporter()
            .build()
            .unwrap();
        assert!(
            client
                .user_auth_set_cookie_txt(&cookies.unwrap())
                .await
                .is_ok(),
            "Could not authenticate"
        );
        let mut token = None;
        let mut visitor = None;
        for index in 0..3 {
            let body = match &token {
                Some(token) => json!({"continuation": token}),
                None => json!({"browseId": "FEwhat_to_watch"}),
            };
            let response = client
                .query()
                .authenticated()
                .visitor_data_opt(visitor.as_deref())
                .raw(ClientType::Desktop, "browse", &body)
                .await;
            assert!(response.is_ok(), "Home request failed");
            let page = parse_home(&response.unwrap()).expect("Home parsing failed");
            eprintln!(
                "Home page {}: {} cards, continuation {}",
                index + 1,
                page.videos.len(),
                page.continuation.is_some()
            );
            token = page.continuation;
            visitor = page.visitor_data.or(visitor);
            if token.is_none() {
                break;
            }
        }
    }

    #[tokio::test]
    async fn thumbnail_retries_are_bounded_and_skip_permanent_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for (statuses, expected_status) in [
            (vec![503, 200], None),
            (vec![429, 200], None),
            (vec![503, 503, 503], Some(503)),
            (vec![404], Some(404)),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/thumbnail", listener.local_addr().unwrap())
                .parse()
                .unwrap();
            let server = tokio::spawn(async move {
                for status in statuses {
                    let (mut connection, _) =
                        tokio::time::timeout(Duration::from_secs(5), listener.accept())
                            .await
                            .unwrap()
                            .unwrap();
                    let mut request = [0; 2048];
                    let mut received = 0;
                    while !request[..received].ends_with(b"\r\n\r\n") {
                        let count = connection.read(&mut request[received..]).await.unwrap();
                        assert!(count > 0, "Incomplete request headers");
                        received += count;
                    }
                    connection.write_all(format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: 5\r\nConnection: close\r\n\r\nimage"
                    ).as_bytes()).await.unwrap();
                }
            });
            let http = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = thumbnail_bytes(&http, &url).await;
            if let Some(status) = expected_status {
                assert_eq!(
                    result
                        .unwrap_err()
                        .downcast_ref::<reqwest::Error>()
                        .unwrap()
                        .status()
                        .unwrap()
                        .as_u16(),
                    status
                );
            } else {
                assert_eq!(result.unwrap(), b"image");
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires explicit SLINTYT_TEST_PROFILE authorization and network access"]
    async fn live_home_and_history_thumbnails() {
        let selection = Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("Set an authorized test profile")
                .into(),
            keyring: None,
        };
        let cookies = cookies::read(&selection)
            .await
            .expect("Cookie import failed");
        let client = RustyPipe::builder()
            .storage(Box::new(IdentityCapture::default()))
            .no_reporter()
            .build()
            .unwrap();
        assert!(client.user_auth_set_cookie_txt(&cookies).await.is_ok());
        let (_, changes) = tokio::sync::watch::channel(0);
        let session = Session {
            progress: None,
            cache: None,
            playback_cookies: cookies.into(),
            client,
            watch_index: Arc::new(RwLock::new(WatchIndex::default())),
            watch_updates: changes,
            watch_task: tokio::spawn(std::future::pending()),
            watch_refresh: Arc::new(tokio::sync::Notify::new()),
            known_subscriptions: RwLock::default(),
        };
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut missing = 0;
        let mut failures = 0;
        let mut shorts = 0;
        let permits = Arc::new(tokio::sync::Semaphore::new(6));
        for view in [View::Home, View::History] {
            let page = session
                .page(&view, None, None)
                .await
                .expect("Page request failed");
            assert!(!page.videos.is_empty(), "Test requires a populated page");
            let page_shorts = page.videos.iter().filter(|v| v.duration == "SHORT").count();
            shorts += page_shorts;
            let mut jobs = tokio::task::JoinSet::new();
            for video in &page.videos {
                if video.thumbnail.is_empty() {
                    missing += 1;
                    continue;
                }
                let (http, url) = (http.clone(), video.thumbnail.clone());
                let permit = permits.clone().acquire_owned().await.unwrap();
                jobs.spawn(async move {
                    let _permit = permit;
                    match download_thumbnail(&http, &url).await {
                        Ok(_) => true,
                        Err(error) => {
                            if let Some(http) = error.downcast_ref::<reqwest::Error>() {
                                eprintln!("Thumbnail failure: HTTP status {:?}, timeout {}, connect {}, body {}",
                                    http.status(), http.is_timeout(), http.is_connect(), http.is_body());
                            } else if error.downcast_ref::<image::ImageError>().is_some() {
                                eprintln!("Thumbnail failure: image decoding");
                            } else {
                                eprintln!("Thumbnail failure: URL or size validation");
                            }
                            false
                        }
                    }
                });
            }
            let mut loaded = 0;
            while let Some(result) = jobs.join_next().await {
                if result.unwrap() {
                    loaded += 1;
                } else {
                    failures += 1;
                }
            }
            eprintln!(
                "{}: {} cards, {loaded} thumbnails loaded, {page_shorts} Shorts",
                if view == View::Home {
                    "Home"
                } else {
                    "History"
                },
                page.videos.len(),
            );
        }
        assert!(
            shorts > 0,
            "Test requires Shorts to exercise their thumbnails"
        );
        assert_eq!(missing, 0, "Cards missing thumbnail URLs");
        assert_eq!(failures, 0, "Thumbnails failed to download or decode");
    }

    #[tokio::test]
    #[ignore = "requires explicit SLINTYT_TEST_PROFILE authorization and network access"]
    async fn live_history_shorts_and_external_playlist_pagination() {
        let selection = Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("Set an authorized test profile")
                .into(),
            keyring: None,
        };
        let cookies = cookies::read(&selection)
            .await
            .expect("Cookie import failed");
        let client = RustyPipe::builder()
            .storage(Box::new(IdentityCapture::default()))
            .no_reporter()
            .build()
            .unwrap();
        assert!(
            client.user_auth_set_cookie_txt(&cookies).await.is_ok(),
            "Authentication failed"
        );
        let (_, changes) = tokio::sync::watch::channel(0);
        // Exercise production requests without changing the user's settings or
        // cache and without launching an unbounded background history crawl.
        let session = Session {
            progress: None,
            cache: None,
            playback_cookies: cookies.into(),
            client,
            watch_index: Arc::new(RwLock::new(WatchIndex::default())),
            watch_updates: changes,
            watch_task: tokio::spawn(std::future::pending()),
            watch_refresh: Arc::new(tokio::sync::Notify::new()),
            known_subscriptions: RwLock::default(),
        };
        let home = session
            .page(&View::Home, None, None)
            .await
            .expect("Home parsing failed");
        eprintln!("Home entries: {}", home.videos.len());
        let library = session
            .page(&View::Playlists, None, None)
            .await
            .expect("Library failed");
        if let Some(list) = library.videos.first() {
            session
                .page(&View::Playlist(list.id.clone()), None, None)
                .await
                .expect("Saved playlist parsing failed");
        }
        let (mut token, mut visitor) = (None::<String>, None::<String>);
        for n in 1..=3 {
            let page = history_page(&session.client, token.as_deref(), visitor.as_deref())
                .await
                .expect("History parsing failed");
            eprintln!(
                "History page {n}: {} videos, {} Shorts",
                page.videos.len(),
                page.videos.iter().filter(|v| v.duration == "SHORT").count()
            );
            assert!(
                !page.videos.is_empty(),
                "History test requires a populated profile"
            );
            token = page.continuation;
            visitor = page.visitor_data;
            if token.is_none() {
                break;
            }
        }
        let search = session
            .page(
                &View::Search("northernlion slay the spire 2".into()),
                None,
                None,
            )
            .await
            .expect("Search parsing failed");
        let list = search
            .videos
            .iter()
            .find(|v| v.playlist)
            .expect("No playlist in search results");
        let view = View::Playlist(list.id.clone());
        let mut feed = crate::feed::FeedPage::default();
        for n in 1..=5 {
            let token = feed.continuation.clone();
            let page = session
                .page(&view, token.as_deref(), feed.visitor.as_deref())
                .await;
            let page = page.expect("Playlist parsing failed");
            feed.accept(&view, token.as_deref(), page)
                .expect("Playlist pagination loop");
            eprintln!(
                "Playlist page {n}: {} accumulated videos",
                feed.videos.len()
            );
            if feed.continuation.is_none() {
                break;
            }
        }
        eprintln!(
            "External playlist: {} videos across {} pages",
            feed.videos.len(),
            feed.pages
        );
        assert!(
            feed.videos.len() > 100 && feed.pages > 1,
            "Test playlist did not extend beyond its first page"
        );
    }

    #[test]
    fn unknown_or_partial_page_formats_are_errors_not_empty_success() {
        for response in [
            json!({"responseContext": {}, "trackingParams": "metadata-only"}),
            json!({"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"selected": true}}]}}}),
            json!({"onResponseReceivedActions": [{"adsControlFlowOpportunityReceivedCommand": {}}]}),
            json!({"contents": {"unsupportedFeedRenderer": {"items": []}}}),
            json!({"contents": null}),
            json!({"contents": {"richGridRenderer": {"contents": [
                {"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Known"}}},
                {"newVideoRenderer": {"videoId": "lmnopqrstuv"}}
            ]}}}),
            json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [
                {"continuationItemViewModel": {"unknownCommand": "secret-token"}}
            ]}}]}),
        ] {
            assert!(parse_home(&response.to_string()).is_err());
        }
    }

    #[test]
    fn modern_playlist_tokens_and_short_shelves_are_not_dropped() {
        let response = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [
            {"richSectionRenderer": {"content": {"reelShelfRenderer": {"items": [
                {"shortsLockupViewModel": {"onTap": {"innertubeCommand": {"reelWatchEndpoint": {"videoId": "abcdefghijk"}}}, "overlayMetadata": {"primaryText": {"content": "Short"}}}},
                {"reelItemRenderer": {"videoId": "lmnopqrstuv", "headline": {"simpleText": "Another short"}}}
            ]}}}},
            {"continuationItemViewModel": {"continuationCommand": {"innertubeCommand": {"continuationCommand": {"token": "next"}}}}}
        ]}}]});
        let page = parse_page(&response.to_string(), ParseMode::History).unwrap();
        assert_eq!(page.videos.len(), 2);
        assert!(page.videos.iter().all(|v| v.watch_hint == WATCHED));
        assert_eq!(page.continuation.as_deref(), Some("next"));
    }

    #[test]
    fn playlist_video_continuation_is_not_overwritten_by_recommendations() {
        let token = |value: &str| {
            json!({"continuationItemViewModel": {
                "continuationCommand": {"innertubeCommand": {"continuationCommand": {"token": value}}}
            }})
        };
        let data = json!({"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {
            "selected": true, "content": {"sectionListRenderer": {"contents": [
                {"itemSectionRenderer": {"contents": [
                    {"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Playlist video"}}},
                    token("video-page-two")
                ]}},
                token("recommendations")
            ]}}
        }}]}}});
        let first = parse_response(&data.to_string(), true).unwrap();
        assert_eq!(first.continuation.as_deref(), Some("video-page-two"));
        let mut feed = crate::feed::FeedPage::default();
        let view = View::Playlist("PL_example".into());
        feed.accept(&view, None, first).unwrap();
        let next = json!({
            "contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"selected": true, "trackingParams": "placeholder"}}]}},
            "onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [
            {"videoRenderer": {"videoId": "lmnopqrstuv", "title": {"simpleText": "Last video"}}}
        ]}}]});
        feed.accept(
            &view,
            Some("video-page-two"),
            parse_response(&next.to_string(), true).unwrap(),
        )
        .unwrap();
        assert_eq!(feed.videos.len(), 2);
        assert_eq!(feed.pages, 2);
        assert!(feed.continuation.is_none());
    }

    #[test]
    fn grid_shelves_preserve_videos_while_non_content_commands_are_ignored() {
        let response = json!({
            "contents": {"gridShelfViewModel": {"contents": [
                {"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Video"}}},
                {"miniGameCardViewModel": {}}
            ]}},
            "onResponseReceivedActions": [{"adsControlFlowOpportunityReceivedCommand": {}}]
        });
        let page = parse_home(&response.to_string()).unwrap();
        assert_eq!(page.videos.len(), 1);
        assert_eq!(page.videos[0].id, "abcdefghijk");
    }

    #[test]
    fn home_parses_legacy_and_modern_cards_deduplicates_and_keeps_continuation() {
        let old = json!({"videoRenderer": {"videoId": "abcdefghijk", "title": {"runs": [{"text": "A "}, {"text": "title"}]}, "ownerText": {"runs": [{"text": "A channel"}]}, "lengthText": {"simpleText": "12:34"}, "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/old.jpg", "width": 480}]}}});
        let modern = json!({"lockupViewModel": {"contentId": "lmnopqrstuv", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO", "metadata": {"lockupMetadataViewModel": {"title": {"content": "A new title"}, "metadata": {"contentMetadataViewModel": {"metadataRows": [{"metadataParts": [{"text": {"content": "New channel"}}]}, {"metadataParts": [{"text": {"content": "1K views"}}, {"text": {"content": "Yesterday"}}]}]}}}}, "contentImage": {"thumbnailViewModel": {"image": {"sources": [{"url": "https://i.ytimg.com/new.jpg", "width": 480}]}, "overlays": [{"thumbnailOverlayBadgeViewModel": {"thumbnailBadges": [{"thumbnailBadgeViewModel": {"text": "10:22"}}]}}]}}}});
        let data = json!({"contents": {"richGridRenderer": {"contents": [old.clone(), old, modern, {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "next-page"}}}}]}}, "responseContext": {"visitorData": "visitor"}});
        let page = parse_home(&data.to_string()).unwrap();
        assert_eq!(page.videos.len(), 2);
        assert_eq!(page.videos[0].title, "A title");
        assert_eq!(page.videos[1].channel, "New channel");
        assert_eq!(page.videos[1].metadata, "1K views · Yesterday");
        assert_eq!(page.videos[1].duration, "10:22");
        assert_eq!(page.continuation.as_deref(), Some("next-page"));
        assert_eq!(page.visitor_data.as_deref(), Some("visitor"));
    }

    #[test]
    fn home_handles_continuations_signed_out_and_empty_feeds() {
        assert!(
            parse_home(r#"{"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}"#)
                .is_err()
        );
        assert!(parse_home(r#"{"error":{"code":401}}"#).is_err());
        assert!(parse_home(r#"{"unexpected":{}}"#).is_err());
        assert!(
            parse_home(r#"{"contents":{"richGridRenderer":{"contents":[]}}}"#)
                .unwrap()
                .videos
                .is_empty()
        );
        let page = parse_home(r#"{"onResponseReceivedActions":[{"appendContinuationItemsAction":{"continuationItems":[{"videoRenderer":{"videoId":"abcdefghijk","title":{"simpleText":"Next"}}}]}}]}"#).unwrap();
        assert_eq!(page.videos[0].title, "Next");
    }
    #[test]
    fn playlist_links_accept_youtube_urls_and_reject_other_hosts() {
        for input in [
            " PL_example-123 ",
            "https://www.youtube.com/playlist?list=PL_example-123",
            "https://music.youtube.com/watch?v=abcdefghijk&list=PL_example-123",
            "https://youtu.be/abcdefghijk?list=PL_example-123",
        ] {
            assert_eq!(playlist_id(input).unwrap(), "PL_example-123");
        }
        assert_eq!(playlist_id("WL").unwrap(), "WL");
        for input in [
            "",
            "https://youtube.com/watch?v=abcdefghijk",
            "https://youtube.com.evil.test/playlist?list=PL_example",
            "file:///playlist?list=PL_example",
            "https://example.com/?list=PL_example",
            "not a playlist",
        ] {
            assert!(playlist_id(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn search_parses_video_and_playlist_cards_and_skips_ads() {
        let response = json!({"contents": {"twoColumnSearchResultsRenderer": {"primaryContents": {"sectionListRenderer": {"contents": [
            {"itemSectionRenderer": {"contents": [
                {"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Result"}, "thumbnailOverlays": [{"thumbnailOverlayResumePlaybackRenderer": {"percentDurationWatched": 35}}]}},
                {"playlistRenderer": {"playlistId": "PL_old", "title": {"simpleText": "Classic playlist"}, "videoCountText": {"simpleText": "3 videos"}}},
                {"lockupViewModel": {"contentId": "PL_new", "contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST", "metadata": {"lockupMetadataViewModel": {"title": {"content": "Modern playlist"}}}, "contentImage": {"collectionThumbnailViewModel": {"primaryThumbnail": {"thumbnailViewModel": {"image": {"sources": [{"url": "https://i.ytimg.com/mock.jpg", "width": 480}]}}}}}}},
                {"adSlotRenderer": {"videoRenderer": {"videoId": "12345678901", "title": {"simpleText": "Advertisement"}}}}
            ]}},
            {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "search-next"}}}}
        ]}}}}});
        let page = parse_home(&response.to_string()).unwrap();
        assert_eq!(page.videos.len(), 3);
        assert_eq!(page.videos[0].watched, WATCHED);
        assert_eq!(page.videos[0].watch_progress, Some(35));
        assert!(page.videos[1].playlist);
        assert_eq!(page.videos[1].metadata, "3 videos");
        assert!(page.videos[2].playlist);
        assert_eq!(page.videos[2].thumbnail, "https://i.ytimg.com/mock.jpg");
        assert_eq!(page.continuation.as_deref(), Some("search-next"));
    }

    #[test]
    fn playlist_preserves_order_repeats_and_ignores_recommendations() {
        let first = json!({"playlistVideoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "First"}, "isWatched": false}});
        let second = json!({"playlistVideoRenderer": {"videoId": "lmnopqrstuv", "title": {"simpleText": "Second"}, "isWatched": true}});
        let response = json!({"metadata": {"playlistMetadataRenderer": {"title": "My playlist"}}, "contents": {
            "twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"playlistVideoListRenderer": {"contents": [first.clone(), second, first, {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "playlist-next"}}}}]}}}}],
            "secondaryContents": {"videoRenderer": {"videoId": "12345678901", "title": {"simpleText": "Recommendation"}}}}
        }});
        let page = parse_response(&response.to_string(), true).unwrap();
        assert_eq!(page.title, "My playlist");
        assert_eq!(
            page.videos
                .iter()
                .map(|v| v.title.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second", "First"]
        );
        assert_eq!(page.videos[0].watched, UNWATCHED);
        assert_eq!(page.videos[1].watched, WATCHED);
        assert_eq!(page.continuation.as_deref(), Some("playlist-next"));
        let next = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [
            {"playlistVideoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Repeated on next page"}}}
        ]}}]});
        assert_eq!(
            parse_response(&next.to_string(), true).unwrap().videos[0].id,
            "abcdefghijk"
        );
        assert!(
            parse_response(r#"{"alerts":[{"alertRenderer":{"type":"ERROR"}}]}"#, true)
                .err()
                .unwrap()
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn watch_markers_distinguish_missing_zero_and_partial_progress() {
        assert_eq!(watch_marker(&json!({})), (UNKNOWN, None));
        assert_eq!(
            watch_marker(
                &json!({"thumbnailOverlayResumePlaybackRenderer": {"percentDurationWatched": 0}})
            ),
            (UNWATCHED, Some(0))
        );
        assert_eq!(
            watch_marker(&json!({"thumbnailOverlayProgressBarViewModel": {"startPercent": 42.5}})),
            (WATCHED, Some(43))
        );
        assert_eq!(
            watch_marker(&json!({"lastPlaybackPositionMilliseconds": "12000"})),
            (WATCHED, None)
        );
        assert_eq!(
            watch_marker(&json!({"isWatched": true, "percentDurationWatched": 0})),
            (WATCHED, Some(0))
        );
        assert_eq!(
            watch_marker(&json!({"percentDurationWatched": 150})),
            (UNKNOWN, None)
        );
    }
    #[test]
    fn modern_thumbnail_progress_reaches_video_cards() {
        for percent in [0, 15, 65, 100] {
            let data = json!({"contents": [{"lockupViewModel": {
                "contentId": "abcdefghijk", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                "metadata": {"lockupMetadataViewModel": {"title": {"content": "A video"}}},
                "contentImage": {"thumbnailViewModel": {"overlays": [{"thumbnailBottomOverlayViewModel": {
                    "progressBar": {"thumbnailOverlayProgressBarViewModel": {"startPercent": percent}}
                }}]}}
            }}]});
            let page = parse_home(&data.to_string()).unwrap();
            assert_eq!(page.videos[0].watch_progress, Some(percent));
        }
        let page = parse_home(r#"{"contents":[{"videoRenderer":{"videoId":"abcdefghijk","title":{"simpleText":"History match only"},"isWatched":true}}]}"#).unwrap();
        assert_eq!(page.videos[0].watch_progress, None);
    }
    #[test]
    fn public_playlist_rich_grid_keeps_order_and_modern_pagination() {
        let card = |id: &str| {
            json!({"richItemRenderer": {"content": {"lockupViewModel": {
                "contentId": id, "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                "metadata": {"lockupMetadataViewModel": {"title": {"content": "Playlist video"}}}
            }}}})
        };
        let data = json!({"metadata": {"playlistMetadataRenderer": {"title": "Public playlist"}}, "contents": {"twoColumnBrowseResultsRenderer": {
            "tabs": [{"tabRenderer": {"selected": true, "content": {"richGridRenderer": {"contents": [
                card("abcdefghijk"), card("lmnopqrstuv"), card("abcdefghijk"),
                {"continuationItemViewModel": {"continuationCommand": {"continuationCommand": {"token": "public-next"}}}}
            ]}}}}],
            "secondaryContents": card("12345678901")
        }}});
        let page = parse_response(&data.to_string(), true).unwrap();
        assert_eq!(page.title, "Public playlist");
        assert_eq!(
            page.videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["abcdefghijk", "lmnopqrstuv", "abcdefghijk"]
        );
        assert_eq!(page.continuation.as_deref(), Some("public-next"));
        let next = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [card("12345678901")]}}]});
        assert_eq!(
            parse_response(&next.to_string(), true)
                .unwrap()
                .videos
                .len(),
            1
        );
    }

    #[test]
    fn playlist_excludes_recommended_entries_and_accepts_empty_end_pages() {
        let data = json!({"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"selected": true, "content": {"sectionListRenderer": {"contents": [
            {"playlistVideoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "In playlist"}}},
            {"playlistVideoRenderer": {"videoId": "lmnopqrstuv", "title": {"simpleText": "Suggestion"}, "style": "PLAYLIST_VIDEO_RENDERER_STYLE_RECOMMENDED_VIDEO"}}
        ]}}}}]}}});
        assert_eq!(
            parse_response(&data.to_string(), true)
                .unwrap()
                .videos
                .len(),
            1
        );
        assert!(
            parse_response(r#"{"onResponseReceivedActions":[]}"#, true)
                .unwrap()
                .videos
                .is_empty()
        );
    }
    #[test]
    fn displayed_history_preserves_progress_dates_and_repeated_watches() {
        let video = json!({"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "History video"}, "thumbnailOverlays": [{"thumbnailOverlayResumePlaybackRenderer": {"percentDurationWatched": 25}}]}});
        let data = json!({"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"selected": true, "content": {"sectionListRenderer": {"contents": [
            {"itemSectionRenderer": {"header": {"itemSectionHeaderRenderer": {"title": {"simpleText": "Today"}}}, "contents": [video.clone(), video]}}
        ]}}}}]}}});
        let page = parse_page(&data.to_string(), ParseMode::History).unwrap();
        assert_eq!(page.videos.len(), 2);
        assert_eq!(page.videos[0].watch_progress, Some(25));
        assert_eq!(page.videos[0].metadata, "Watched Today");
        assert_eq!(page.videos[0].watch_hint, WATCHED);
    }
    #[test]
    fn history_keeps_short_videos_when_reading_progress_metadata() {
        let data = json!({"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"selected": true, "content": {"sectionListRenderer": {"contents": [
            {"shortsLockupViewModel": {"onTap": {"innertubeCommand": {"reelWatchEndpoint": {"videoId": "abcdefghijk"}}}, "overlayMetadata": {"primaryText": {"content": "A short video"}}, "thumbnail": {"sources": [{"url": "https://i.ytimg.com/short.jpg", "width": 320}]}}},
            {"reelItemRenderer": {"videoId": "lmnopqrstuv", "headline": {"simpleText": "A classic short"}}}
        ]}}}}]}}});
        let page = parse_page(&data.to_string(), ParseMode::History).unwrap();
        assert_eq!(page.videos.len(), 2);
        assert_eq!(page.videos[0].title, "A short video");
        assert_eq!(page.videos[1].title, "A classic short");
        assert_eq!(page.videos[0].watch_hint, WATCHED);
    }

    #[test]
    fn shorts_thumbnails_support_current_and_legacy_layouts_on_home_and_history() {
        let sources = json!([
            {"url": "//i.ytimg.com/small.jpg", "width": 160},
            {"url": "//i.ytimg.com/medium.jpg", "width": 480},
            {"url": "//i.ytimg.com/large.jpg", "width": 1080}
        ]);
        let variants = [
            json!({"thumbnailViewModel": {"thumbnailViewModel": {"image": {"sources": sources}}}}),
            json!({"thumbnailViewModel": {"image": {"sources": sources}}}),
            json!({"thumbnail": {"sources": sources}}),
            json!({"thumbnail": {"thumbnails": sources}}),
            json!({"thumbnailViewModel": {"thumbnailViewModel": {"image": {"sources": []}}}}),
        ];
        for mut card in variants {
            card["onTap"] = json!({"innertubeCommand": {"reelWatchEndpoint": {
                "videoId": "abcdefghijk",
                "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/endpoint.jpg", "width": 480}]}
            }}});
            card["overlayMetadata"] = json!({"primaryText": {"content": "Example Short"}});
            let expected = if card.pointer("/thumbnailViewModel/thumbnailViewModel/image/sources")
                == Some(&json!([]))
            {
                "https://i.ytimg.com/endpoint.jpg"
            } else {
                "https://i.ytimg.com/medium.jpg"
            };
            let data = json!({"contents": [{"shortsLockupViewModel": card}]}).to_string();
            for mode in [ParseMode::Feed, ParseMode::History] {
                let page = parse_page(&data, mode).unwrap();
                assert_eq!(page.videos.len(), 1);
                assert_eq!(page.videos[0].thumbnail, expected);
            }
        }
    }

    #[test]
    fn branded_video_shelves_do_not_block_the_home_feed() {
        let card = |id: &str| {
            json!({"lockupViewModel": {
                "contentId": id,
                "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                "metadata": {"lockupMetadataViewModel": {"title": {"content": "Example video"}}},
                "contentImage": {"thumbnailViewModel": {"image": {"sources": [
                    {"url": "https://i.ytimg.com/example.jpg", "width": 480}
                ]}}}
            }})
        };
        let data = json!({"contents": {"richGridRenderer": {"contents": [
            {"richSectionRenderer": {"content": {"brandVideoShelfRenderer": {
                "content": [card("abcdefghijk")],
                "header": {"sectionHeaderViewModel": {}},
                "showMoreButton": {"buttonRenderer": {}}
            }}}},
            {"richItemRenderer": {"content": card("lmnopqrstuv")}}
        ]}}});
        let page = parse_home(&data.to_string()).unwrap();
        assert_eq!(page.videos.len(), 2);
        assert_eq!(page.videos[0].id, "abcdefghijk");
        assert_eq!(page.videos[1].id, "lmnopqrstuv");
        assert!(page.videos.iter().all(|v| !v.thumbnail.is_empty()));
    }

    #[test]
    fn home_category_selectors_preserve_surrounding_videos_and_continuation() {
        let mut shelf = json!({
            "chipsShelf": {"chipsShelfViewModel": {
                "contents": [{"chipViewModel": {"text": "Example category"}}],
                "autoselectChipOnVisible": true, "autoselectedChipIndex": 0
            }},
            "header": {"sectionHeaderViewModel": {"headline": {"content": "Example"}}},
            "contentId": "category-selector", "targetId": "category-shelf"
        });
        let response = |shelf: Value| {
            json!({
                "onResponseReceivedActions": [{"appendContinuationItemsAction": {
                    "continuationItems": [
                        {"videoRenderer": {"videoId": "abcdefghijk", "title": {"simpleText": "Before"}}},
                        {"richSectionRenderer": {"content": {"chipsShelfWithVideoShelfRenderer": shelf}}},
                        {"videoRenderer": {"videoId": "lmnopqrstuv", "title": {"simpleText": "After"}}},
                        {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "next"}}}}
                    ]
                }}]
            })
        };
        let page = parse_home(&response(shelf.clone()).to_string()).unwrap();
        assert_eq!(
            page.videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["abcdefghijk", "lmnopqrstuv"]
        );
        assert_eq!(page.continuation.as_deref(), Some("next"));

        shelf["content"] = json!({"videoRenderer": {
            "videoId": "12345678901", "title": {"simpleText": "Inside"}
        }});
        let page = parse_home(&response(shelf.clone()).to_string()).unwrap();
        assert_eq!(
            page.videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["abcdefghijk", "12345678901", "lmnopqrstuv"]
        );

        shelf["content"] = json!({"unrecognizedVideoShelfRenderer": {"items": []}});
        assert!(parse_home(&response(shelf.clone()).to_string()).is_err());
        shelf.as_object_mut().unwrap().remove("content");
        shelf["unrecognizedShelf"] = json!({});
        assert!(parse_home(&response(shelf).to_string()).is_err());
        assert!(parse_home(&response(json!({})).to_string()).is_err());
    }

    #[test]
    fn parser_diagnostics_do_not_print_response_values() {
        let diagnostic = schema(
            &json!({
                "title": "private-title", "token": "private-token",
                "url": "https://example.invalid/private",
                "nested": [{"cookie": "private-cookie", "accountId": "private-account"}]
            }),
            6,
        )
        .to_string();
        assert!(!diagnostic.contains("private"));
        assert!(!diagnostic.contains("https://"));
        assert!(diagnostic.contains("string"));
    }
}
