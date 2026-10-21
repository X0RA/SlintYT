//! Account-scoped bookmark destinations and a complete playlist membership index.
//! Authentication stays inside RustyPipe; responses and credentials are never logged.
use crate::{
    cache::Cache,
    config::Store,
    youtube::{self, Page, Video},
};
use anyhow::{Context, Result, bail};
use rustypipe::{
    client::{ClientType, RustyPipe},
    model::{PlaylistItem, paginator::ContinuationEndpoint},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::RwLock,
};

const DEFAULT_NAME: &str = "slintyt-bookmarks";

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DestinationMode {
    #[default]
    Automatic,
    Selected,
}
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    Added,
    Played,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Preferences {
    pub mode: DestinationMode,
    pub automatic_id: String,
    pub selected_id: String,
    pub sort: Sort,
}
impl Preferences {
    pub fn id(&self) -> &str {
        match self.mode {
            DestinationMode::Automatic => &self.automatic_id,
            DestinationMode::Selected => &self.selected_id,
        }
    }
}
#[derive(Clone, Default)]
pub struct Choice {
    pub id: String,
    pub title: String,
    pub visibility: String,
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub preferences: Preferences,
    pub enabled: bool,
    pub busy: bool,
    pub choices_busy: bool,
    pub ready: bool,
    pub error: String,
    pub note: String,
    pub title: String,
    pub choices: Vec<Choice>,
    pub revision: u64,
    pub content_revision: u64,
}
#[derive(Default)]
struct State {
    ui: Snapshot,
    generation: u64,
    videos: Vec<Video>,
    pending: HashMap<String, bool>,
    added_sorted: bool,
    rendered_revision: Option<u64>,
    rendered_history: Option<u64>,
    // Newly created playlists can read as unavailable for a while; never replace them then.
    created: Option<(String, std::time::Instant)>,
    fetched: Option<(String, std::time::Instant)>,
    choices_error: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct SavedPlaylist {
    title: String,
    videos: Vec<Video>,
    added_sorted: bool,
}

pub struct Bookmarks {
    client: RustyPipe,
    cache: Option<Cache>,
    path: PathBuf,
    state: RwLock<State>,
    operations: tokio::sync::Mutex<()>,
}
impl Bookmarks {
    pub fn new(account: String, client: RustyPipe, cache: Option<Cache>) -> Result<Self> {
        let store = Store::new()?;
        let enabled = store.load_settings()?.bookmarks_enabled;
        let path = store.directory().join("bookmarks").join(account);
        let preferences = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Bookmark settings could not be read.")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Preferences::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            client,
            cache,
            path,
            state: RwLock::new(State {
                ui: Snapshot {
                    preferences,
                    enabled,
                    ..Default::default()
                },
                ..Default::default()
            }),
            operations: tokio::sync::Mutex::new(()),
        })
    }
    pub fn needs_render(&self, history_revision: u64) -> bool {
        let state = self.state.read().unwrap();
        state.rendered_revision != Some(state.ui.content_revision)
            || (state.ui.preferences.sort == Sort::Played
                && state.rendered_history != Some(history_revision))
    }
    fn recently_created(&self, id: &str) -> bool {
        self.state
            .read()
            .unwrap()
            .created
            .as_ref()
            .is_some_and(|(created, at)| {
                created == id && at.elapsed() < std::time::Duration::from_secs(10 * 60)
            })
    }
    fn recently_fetched(&self, id: &str) -> bool {
        self.state
            .read()
            .unwrap()
            .fetched
            .as_ref()
            .is_some_and(|(fetched, at)| {
                fetched == id && at.elapsed() < std::time::Duration::from_secs(30)
            })
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.read().unwrap().ui.clone()
    }
    // Unknown, absent, present, adding, removing. Unknown never means absent.
    pub fn membership(&self, id: &str) -> i32 {
        let state = self.state.read().unwrap();
        if let Some(adding) = state.pending.get(id) {
            return if *adding { 3 } else { 4 };
        }
        if state.videos.iter().any(|v| v.id == id) {
            return 2;
        }
        if state.ui.ready { 1 } else { 0 }
    }
    pub fn set_error(&self, error: String) {
        let mut state = self.state.write().unwrap();
        state.ui.error = error;
        state.ui.revision += 1;
    }
    fn save(&self, preferences: &Preferences) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("Invalid bookmark settings path")?;
        std::fs::create_dir_all(parent)?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(preferences)?)?;
        std::fs::rename(temp, &self.path)?;
        Ok(())
    }
    pub fn configure(
        &self,
        enabled: bool,
        mode: DestinationMode,
        selected: Option<String>,
    ) -> Result<()> {
        let mut state = self.state.write().unwrap();
        let mut preferences = state.ui.preferences.clone();
        preferences.mode = mode;
        if let Some(id) = selected {
            preferences.selected_id = id;
        }
        self.save(&preferences)?;
        let different = preferences.id() != state.ui.preferences.id();
        state.generation += 1;
        state.ui.enabled = enabled;
        state.ui.preferences = preferences;
        state.ui.busy = false;
        state.ui.ready = false;
        state.ui.error.clear();
        state.pending.clear();
        if different {
            state.videos.clear();
            state.ui.title.clear();
        }
        state.ui.revision += 1;
        state.ui.content_revision += 1;
        Ok(())
    }
    pub fn set_sort(&self, sort: Sort) -> Result<()> {
        let mut state = self.state.write().unwrap();
        let mut preferences = state.ui.preferences.clone();
        preferences.sort = sort;
        self.save(&preferences)?;
        state.ui.preferences = preferences;
        state.ui.revision += 1;
        state.ui.content_revision += 1;
        Ok(())
    }
    pub fn suspend(&self) {
        let mut state = self.state.write().unwrap();
        state.generation += 1;
        state.ui.enabled = false;
        state.pending.clear();
        state.ui.revision += 1;
    }
    pub fn generation(&self) -> u64 {
        self.state.read().unwrap().generation
    }
    fn valid(&self, generation: u64) -> bool {
        let state = self.state.read().unwrap();
        state.generation == generation && state.ui.enabled
    }
    fn finish(&self, generation: u64, result: &Result<()>) {
        let mut state = self.state.write().unwrap();
        if state.generation != generation {
            return;
        }
        state.ui.busy = false;
        state.ui.error = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        state.ui.revision += 1;
    }
    async fn request(&self, endpoint: &str, body: &Value) -> Result<Value> {
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.client
                .query()
                .lang(rustypipe::param::Language::En)
                .authenticated()
                .raw(ClientType::Desktop, endpoint, body),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("YouTube took too long. Refresh before retrying this action.")
        })?
        .map_err(|_| {
            anyhow::anyhow!("YouTube could not complete the bookmark request. Please retry.")
        })?;
        let value: Value = serde_json::from_str(&raw)
            .context("YouTube returned an unreadable bookmark response.")?;
        if value.get("error").is_some() || raw.contains("STATUS_FAILED") {
            bail!("YouTube did not accept this bookmark action. Reconnect your browser or retry.");
        }
        Ok(value)
    }
    async fn library(&self) -> Result<Vec<PlaylistItem>> {
        let query = self.client.query().authenticated();
        let mut page = query
            .saved_playlists()
            .await
            .map_err(|_| anyhow::anyhow!("Could not load your playlists. Please retry."))?;
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        loop {
            result.extend(page.items);
            let Some(token) = page.ctoken else { break };
            if !seen.insert(token.clone()) {
                bail!("YouTube repeated a playlist page. Please retry.");
            }
            page = query
                .continuation::<PlaylistItem, _>(
                    &token,
                    ContinuationEndpoint::Browse,
                    page.visitor_data.as_deref(),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Could not load all your playlists. Please retry."))?;
        }
        Ok(result)
    }
    async fn metadata(&self, id: &str) -> Result<(Value, String, String)> {
        // Newly created playlists can briefly return an unavailable response.
        // Retry only the read, never playlist creation or edits.
        let mut raw = self
            .request("browse", &json!({"browseId":format!("VL{id}")}))
            .await?;
        let mut parsed = youtube::parse_response(&raw.to_string(), true);
        for _ in 0..3 {
            if parsed.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            raw = self
                .request("browse", &json!({"browseId":format!("VL{id}")}))
                .await?;
            parsed = youtube::parse_response(&raw.to_string(), true);
        }
        if parsed.is_err()
            && raw["alerts"]
                .as_array()
                .is_some_and(|alerts| alerts.iter().any(|a| a["alertRenderer"]["type"] == "ERROR"))
        {
            return Err(UnavailablePlaylist.into());
        }
        let page = parsed?;
        if !editable(&raw) {
            bail!(
                "This playlist is unavailable or cannot be edited by this account. Choose another playlist in Settings."
            );
        }
        let visibility = playlist_visibility(&raw);
        Ok((raw, page.title, visibility))
    }
    pub async fn load_choices(&self) -> Result<()> {
        let generation = {
            let mut s = self.state.write().unwrap();
            if s.ui.choices_busy {
                return Ok(());
            }
            s.ui.choices_busy = true;
            s.ui.revision += 1;
            s.generation
        };
        let result = async {
            let items = tokio::time::timeout(std::time::Duration::from_secs(60), self.library()).await
                .map_err(|_| anyhow::anyhow!("Loading playlists timed out. Please retry."))??;
            let mut choices = Vec::new();
            let mut failed = 0;
            // Four independent reads at a time, without holding the mutation lock.
            for batch in items.chunks(4) {
                if self.generation() != generation { return Ok(()); }
                let inspect = |item: Option<PlaylistItem>| async move {
                    let Some(item) = item else { return Ok(None); };
                    let raw = self.request("browse", &json!({"browseId":format!("VL{}",item.id)})).await?;
                    let page = youtube::parse_response(&raw.to_string(), true)?;
                    if !editable(&raw) { return Ok(None); }
                    Ok::<_, anyhow::Error>(Some(Choice {
                        id: item.id.clone(),
                        title: if page.title.is_empty() { item.name.clone() } else { page.title },
                        visibility: playlist_visibility(&raw),
                    }))
                };
                let (a,b,c,d) = tokio::join!(inspect(batch.first().cloned()), inspect(batch.get(1).cloned()), inspect(batch.get(2).cloned()), inspect(batch.get(3).cloned()));
                for result in [a,b,c,d] {
                    match result { Ok(Some(choice)) => choices.push(choice), Ok(None) => {}, Err(_) => failed += 1 }
                }
                choices.sort_by_key(|c| c.title.to_lowercase());
                let mut state = self.state.write().unwrap();
                if state.generation == generation {
                    state.ui.choices = choices.clone();
                    state.ui.revision += 1;
                }
            }
            {
                let mut state = self.state.write().unwrap();
                if state.generation == generation { state.ui.choices = choices; }
            }
            if failed > 0 { bail!("{failed} playlists could not be checked. Available playlists are listed; reload to retry the others."); }
            Ok(())
        }.await;
        self.finish_choices(generation, &result);
        result
    }
    fn finish_choices(&self, generation: u64, result: &Result<()>) {
        let mut state = self.state.write().unwrap();
        state.ui.choices_busy = false;
        if state.generation == generation {
            // Only clear an error this loader set, never one from a refresh or toggle.
            match result {
                Err(e) => {
                    state.ui.error = e.to_string();
                    state.choices_error = state.ui.error.clone();
                }
                Ok(()) => {
                    if !state.choices_error.is_empty() && state.ui.error == state.choices_error {
                        state.ui.error.clear();
                    }
                    state.choices_error.clear();
                }
            }
        }
        state.ui.revision += 1;
    }
    // Called only after an explicit unavailable response and a successful library
    // scan that excludes the saved ID. Preserve manually selected destinations.
    fn forget_automatic(&self, id: &str, generation: u64) -> Result<bool> {
        let mut state = self.state.write().unwrap();
        if state.generation != generation
            || !state.ui.enabled
            || state.ui.preferences.mode != DestinationMode::Automatic
            || state.ui.preferences.automatic_id != id
        {
            return Ok(false);
        }
        let mut preferences = state.ui.preferences.clone();
        preferences.automatic_id.clear();
        self.save(&preferences)?;
        state.ui.preferences = preferences;
        state.videos.clear();
        state.ui.ready = false;
        state.ui.title.clear();
        state.ui.revision += 1;
        state.ui.content_revision += 1;
        Ok(true)
    }
    async fn resolve(&self, generation: u64) -> Result<String> {
        let preferences = self.snapshot().preferences;
        if !preferences.id().is_empty() {
            return Ok(preferences.id().into());
        }
        if preferences.mode == DestinationMode::Selected {
            bail!("Choose a bookmark playlist in Settings.");
        }
        let library = self.library().await?;
        let mut matches = Vec::new();
        for item in library.into_iter().filter(|p| p.name == DEFAULT_NAME) {
            // A failed exact-name lookup is not evidence that creation is needed.
            let (_, title, visibility) = self.metadata(&item.id).await?;
            matches.push(Choice {
                id: item.id,
                title,
                visibility,
            });
        }
        if !self.valid(generation) {
            bail!("Bookmark settings changed.");
        }
        if matches.len() > 1 {
            self.state.write().unwrap().ui.choices = matches;
            bail!(
                "More than one slintyt-bookmarks playlist exists. Use Choose playlist to select one."
            );
        }
        let id = if let Some(found) = matches.first() {
            found.id.clone()
        } else {
            // No blind retry: the next attempt rechecks the complete library first.
            let response = self
                .request(
                    "playlist/create",
                    &json!({"title":DEFAULT_NAME,"privacyStatus":"PRIVATE","videoIds":[]}),
                )
                .await?;
            response["playlistId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("YouTube did not confirm the new playlist. Refresh before retrying.")?
                .to_owned()
        };
        let mut state = self.state.write().unwrap();
        if matches.is_empty() {
            state.created = Some((id.clone(), std::time::Instant::now()));
        }
        // Remember a successfully created ID even when the user disabled the feature mid-request.
        state.ui.preferences.automatic_id = id.clone();
        self.save(&state.ui.preferences)?;
        state.ui.revision += 1;
        Ok(id)
    }
    pub async fn ensure_loaded(&self) -> Result<()> {
        let _guard = self.operations.lock().await;
        let snapshot = self.snapshot();
        if !snapshot.enabled {
            bail!("Enable bookmarks in Settings.");
        }
        if !snapshot.ready {
            self.refresh_locked().await?;
        }
        Ok(())
    }
    pub async fn refresh(&self) -> Result<()> {
        let _guard = self.operations.lock().await;
        self.refresh_locked().await
    }
    async fn refresh_locked(&self) -> Result<()> {
        let generation = self.state.read().unwrap().generation;
        if !self.valid(generation) {
            return Ok(());
        }
        {
            let mut s = self.state.write().unwrap();
            s.ui.busy = true;
            s.ui.revision += 1;
        }
        let result = async {
            let id = self.resolve(generation).await?;
            if !self.valid(generation) {
                return Ok(());
            }
            if let Some(cache) = &self.cache
                && let Some(saved) = cache.get::<SavedPlaylist>("bookmarks-v1", &id).await
            {
                let mut s = self.state.write().unwrap();
                if s.generation == generation && s.videos.is_empty() && !saved.videos.is_empty() {
                    s.videos = saved.videos;
                    s.ui.title = saved.title;
                    s.added_sorted = saved.added_sorted;
                    s.ui.note = "Showing saved bookmarks while refreshing…".into();
                    s.ui.content_revision += 1;
                    s.ui.revision += 1;
                }
            }
            let fetched = self.fetch(&id, generation).await;
            if fetched
                .as_ref()
                .err()
                .is_some_and(|e| e.is::<UnavailablePlaylist>())
                && self.snapshot().preferences.mode == DestinationMode::Automatic
                && self.valid(generation)
                && !self.recently_created(&id)
            {
                // Confirm absence in a complete successful library response. Network
                // or parser failures must never cause another playlist to be created.
                let library = self.library().await?;
                if !library.iter().any(|p| p.id == id) && self.valid(generation) {
                    if !self.forget_automatic(&id, generation)? {
                        return Ok(());
                    }
                    let replacement = self.resolve(generation).await?;
                    return self.fetch(&replacement, generation).await;
                }
            }
            fetched
        }
        .await;
        self.finish(generation, &result);
        result
    }
    async fn fetch(&self, id: &str, generation: u64) -> Result<()> {
        let (mut raw, title, visibility) = self.metadata(id).await?;
        let mut added_sorted = false;
        if let Some(action) = youtube::renderers::sort_action(&raw, "Date added (newest)") {
            raw = self.request("browse", &action).await?;
            added_sorted = true;
        } else if selected_added_sort(&raw) {
            added_sorted = true;
        } else if let Some(action) = added_sort_edit(&raw, id) {
            if !self.valid(generation) {
                return Ok(());
            }
            self.request("browse/edit_playlist", &action).await?;
            raw = self.metadata(id).await?.0;
            added_sorted = selected_added_sort(&raw);
        }
        let mut page = youtube::parse_response(&raw.to_string(), true)?;
        let mut videos = Vec::new();
        let mut tokens = HashSet::new();
        loop {
            if !self.valid(generation) {
                return Ok(());
            }
            videos.extend(page.videos);
            let Some(token) = page.continuation else {
                break;
            };
            if !tokens.insert(token.clone()) {
                bail!("YouTube repeated a bookmark page. Please retry.");
            }
            raw = self
                .request("browse", &json!({"continuation":token}))
                .await?;
            page = youtube::parse_response(&raw.to_string(), true)?;
        }
        let snapshot = SavedPlaylist {
            title: format!("{title} · {visibility}"),
            videos,
            added_sorted,
        };
        if !self.valid(generation) {
            return Ok(());
        }
        if let Some(cache) = &self.cache {
            cache.put("bookmarks-v1", id, &snapshot, 30 * crate::cache::DAY);
        }
        let mut s = self.state.write().unwrap();
        if s.generation != generation {
            return Ok(());
        }
        let changed = !s.ui.ready
            || s.videos
                .iter()
                .map(|v| (&v.id, &v.set_id))
                .ne(snapshot.videos.iter().map(|v| (&v.id, &v.set_id)))
            || s.added_sorted != added_sorted;
        s.videos = snapshot.videos;
        s.ui.title = snapshot.title;
        s.added_sorted = added_sorted;
        s.fetched = Some((id.to_owned(), std::time::Instant::now()));
        s.ui.ready = true;
        s.ui.note.clear();
        if changed {
            s.ui.content_revision += 1;
        }
        s.ui.revision += 1;
        Ok(())
    }
    pub fn page(
        &self,
        history: &[String],
        history_complete: bool,
        history_revision: u64,
    ) -> Result<Page> {
        let s = self.state.read().unwrap();
        if !s.ui.enabled {
            bail!("Enable bookmarks in Settings.");
        }
        let rendered_revision = s.ui.content_revision;
        let mut seen = HashSet::new();
        let mut videos: Vec<_> = s
            .videos
            .iter()
            .filter(|v| seen.insert(v.id.clone()))
            .cloned()
            .collect();
        if s.ui.preferences.sort == Sort::Played {
            let ranks: HashMap<_, _> = history
                .iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), i))
                .collect();
            videos.sort_by_key(|v| ranks.get(v.id.as_str()).copied().unwrap_or(usize::MAX));
        }
        // Keep videos playable when YouTube removes its sort control; the UI explicitly
        // identifies the fallback as playlist order instead of claiming date-added order.
        drop(s);
        let mut s = self.state.write().unwrap();
        s.rendered_revision = Some(rendered_revision);
        s.rendered_history = Some(history_revision);
        let note = if s.ui.preferences.sort == Sort::Played && !history_complete {
            "Watch history is still syncing. Recently played order is incomplete."
        } else if s.ui.preferences.sort == Sort::Added && !s.added_sorted {
            "YouTube did not provide date-added sorting. Unranked videos are shown in playlist order. Refresh to retry."
        } else {
            ""
        };
        if s.ui.note != note {
            s.ui.note = note.into();
            s.ui.revision += 1;
        }
        Ok(Page {
            videos,
            title: "Bookmarks".into(),
            ..Default::default()
        })
    }
    pub fn mark_pending(&self, video: &str) {
        let mut s = self.state.write().unwrap();
        let adding = !s.videos.iter().any(|v| v.id == video);
        s.pending.insert(video.into(), adding);
        s.ui.revision += 1;
    }
    pub fn clear_pending(&self, video: &str) {
        let mut s = self.state.write().unwrap();
        s.pending.remove(video);
        s.ui.revision += 1;
    }
    pub async fn toggle(&self, video: &str, generation: u64) -> Result<()> {
        let result = self.toggle_inner(video, generation).await;
        if result.is_err() {
            // Any failure may mean the local copy is stale, so the next toggle rereads it.
            self.state.write().unwrap().fetched = None;
        }
        if self.valid(generation) {
            self.clear_pending(video);
            self.finish(generation, &result);
        }
        result
    }
    async fn toggle_inner(&self, video: &str, generation: u64) -> Result<()> {
        if !self.valid(generation) {
            return Ok(());
        }
        if !crate::history::video_id(video) {
            bail!("Invalid video.");
        }
        let adding = {
            let state = self.state.read().unwrap();
            state
                .pending
                .get(video)
                .copied()
                .unwrap_or_else(|| !state.videos.iter().any(|v| v.id == video))
        };
        self.ensure_loaded().await?;
        let _guard = self.operations.lock().await;
        if !self.valid(generation) {
            return Ok(());
        }
        let id = self.snapshot().preferences.id().to_owned();
        // Reconcile external changes before choosing add vs remove, unless the
        // playlist was just read (the previous toggle or refresh already did).
        if !self.recently_fetched(&id) {
            self.fetch(&id, generation).await?;
            if !self.valid(generation) {
                return Ok(());
            }
        }
        let actions = {
            let s = self.state.read().unwrap();
            edit_actions(&s.videos, video, adding)?
        };
        if actions.is_empty() {
            return Ok(());
        }
        let result = async {
            self.request(
                "browse/edit_playlist",
                &json!({"playlistId":id,"actions":actions}),
            )
            .await?;
            if let Some(cache) = &self.cache {
                cache.invalidate("pages-v1").await;
            }
            self.fetch(&id, generation).await?;
            if self.valid(generation)
                && self
                    .state
                    .read()
                    .unwrap()
                    .videos
                    .iter()
                    .any(|v| v.id == video)
                    != adding
            {
                bail!("YouTube has not confirmed the bookmark change. Refresh before retrying.");
            }
            Ok(())
        }
        .await;
        {
            let mut s = self.state.write().unwrap();
            if s.generation != generation {
                return result;
            }
            s.pending.remove(video);
            s.ui.revision += 1;
            if result.is_err() {
                s.ui.ready = false;
            }
        }
        self.finish(generation, &result);
        result
    }
}

#[derive(Debug)]
struct UnavailablePlaylist;
impl std::fmt::Display for UnavailablePlaylist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This playlist is unavailable. Refresh or choose another playlist in Settings.")
    }
}
impl std::error::Error for UnavailablePlaylist {}

fn edit_actions(videos: &[Video], video: &str, adding: bool) -> Result<Vec<Value>> {
    let rows: Vec<_> = videos.iter().filter(|v| v.id == video).collect();
    if adding {
        return Ok(if rows.is_empty() {
            vec![json!({"action":"ACTION_ADD_VIDEO","addedVideoId":video})]
        } else {
            Vec::new()
        });
    }
    if rows.iter().any(|v| v.set_id.is_empty()) {
        bail!("YouTube did not supply the playlist entry needed for removal. Refresh and retry.");
    }
    Ok(rows.iter().map(|v| json!({"action":"ACTION_REMOVE_VIDEO","setVideoId":v.set_id,"removedVideoId":video})).collect())
}

fn playlist_visibility(raw: &Value) -> String {
    if let Some(value) =
        youtube::find_renderer(raw, "playlistHeaderRenderer").and_then(|h| h["privacy"].as_str())
    {
        return value.to_owned();
    }
    fn selected(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => {
                if value["isSelected"] == true || value["selected"] == true {
                    let label = youtube::text(&value["label"]);
                    if ["Private", "Public", "Unlisted"].contains(&label.as_str()) {
                        return Some(label.to_uppercase());
                    }
                }
                map.values().find_map(selected)
            }
            Value::Array(items) => items.iter().find_map(selected),
            _ => None,
        }
    }
    youtube::find_renderer(&raw["sidebar"], "playlistSidebarPrimaryInfoRenderer")
        .and_then(|v| selected(&v["privacyForm"]))
        .unwrap_or_else(|| "Unknown visibility".into())
}
fn editable(raw: &Value) -> bool {
    if youtube::find_renderer(raw, "playlistVideoListRenderer")
        .is_some_and(|v| v["isEditable"].as_bool() == Some(true))
    {
        return true;
    }
    // Empty playlists use the new page header and omit playlistVideoListRenderer.
    // An owner-only title edit form is positive evidence, unlike Save/Share controls.
    if youtube::find_renderer(&raw["sidebar"], "playlistSidebarPrimaryInfoRenderer")
        .is_some_and(|v| youtube::find_renderer(&v["titleForm"], "playlistEditEndpoint").is_some())
    {
        return true;
    }
    youtube::find_renderer(raw, "playlistHeaderRenderer").is_some_and(|v| {
        v["isEditable"].as_bool() == Some(true) || v["canDelete"].as_bool() == Some(true)
    })
}
fn added_sort_edit(value: &Value, playlist: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if youtube::text(&value["title"]).eq_ignore_ascii_case("Date added (newest)")
                && let Some(endpoint) = youtube::find_renderer(value, "playlistEditEndpoint")
            {
                let actions = endpoint["actions"].as_array()?;
                if endpoint["playlistId"].as_str() == Some(playlist)
                    && actions.len() == 1
                    && actions[0]["action"] == "ACTION_SET_PLAYLIST_VIDEO_ORDER"
                    && actions[0]["playlistVideoOrder"].is_number()
                {
                    return Some(endpoint.clone());
                }
            }
            map.values().find_map(|v| added_sort_edit(v, playlist))
        }
        Value::Array(items) => items.iter().find_map(|v| added_sort_edit(v, playlist)),
        _ => None,
    }
}
fn selected_added_sort(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            (value["selected"].as_bool() == Some(true)
                && youtube::text(&value["title"]).eq_ignore_ascii_case("Date added (newest)"))
                || map.values().any(selected_added_sort)
        }
        Value::Array(items) => items.iter().any(selected_added_sort),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn authorized_client() -> RustyPipe {
        let name = std::env::var("SLINTYT_TEST_ACCOUNT")
            .expect("Set SLINTYT_TEST_ACCOUNT to the explicitly authorized account name");
        let settings = Store::new().unwrap().load_settings().unwrap();
        let selection = settings
            .profiles
            .iter()
            .find(|p| p.youtube.name == name)
            .expect("Authorized saved account not found")
            .selection
            .clone();
        let (client, _, _) = crate::youtube::authenticated_client(&selection)
            .await
            .expect("Normal account connection failed");
        let account = client
            .query()
            .authenticated()
            .raw(ClientType::Desktop, "account/account_menu", &json!({}))
            .await
            .unwrap();
        let account: Value = serde_json::from_str(&account).unwrap();
        let header = youtube::find_renderer(&account, "activeAccountHeaderRenderer").unwrap();
        assert_eq!(
            youtube::text(&header["accountName"]),
            name,
            "Wrong active account"
        );
        client
    }
    #[tokio::test]
    #[ignore = "Creates and deletes a disposable playlist on an explicitly authorized account"]
    async fn live_bookmark_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = service(dir.path().join("test.json"));
        b.client = authorized_client().await;
        let response = b.request("playlist/create", &json!({"title":format!("slintyt-bookmarks-test-{}",crate::history::now()),"privacyStatus":"PRIVATE","videoIds":[]})).await.expect("Private test playlist creation failed");
        let id = response["playlistId"]
            .as_str()
            .expect("Missing new playlist ID")
            .to_owned();
        // Action failures are returned so cleanup runs before the test assertion.
        let work = async {
            b.configure(true, DestinationMode::Selected, Some(id.clone()))?;
            b.refresh().await?;
            let (_, _, privacy) = b.metadata(&id).await?;
            anyhow::ensure!(privacy == "PRIVATE", "New playlist was not private");
            anyhow::ensure!(
                b.membership("jNQXAC9IVRw") == 1,
                "New playlist was not empty"
            );
            b.mark_pending("jNQXAC9IVRw");
            b.toggle("jNQXAC9IVRw", b.generation()).await?;
            b.clear_pending("jNQXAC9IVRw");
            anyhow::ensure!(
                b.membership("jNQXAC9IVRw") == 2,
                "Added video was not confirmed"
            );
            anyhow::ensure!(
                b.state.read().unwrap().added_sorted,
                "Newest-added ordering was not confirmed"
            );
            b.mark_pending("jNQXAC9IVRw");
            b.toggle("jNQXAC9IVRw", b.generation()).await?;
            b.clear_pending("jNQXAC9IVRw");
            anyhow::ensure!(
                b.membership("jNQXAC9IVRw") == 1,
                "Removed video remained in playlist"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let cleanup = b
            .request("playlist/delete", &json!({"playlistId":id}))
            .await;
        assert!(
            cleanup.is_ok(),
            "Could not delete the disposable test playlist"
        );
        work.expect("Bookmark round trip failed; disposable playlist was deleted");
        println!(
            "Verified private creation, complete membership, add, remove, and disposable playlist deletion."
        );
    }

    fn service(path: PathBuf) -> Bookmarks {
        Bookmarks {
            client: RustyPipe::new(),
            cache: None,
            path,
            state: RwLock::new(State {
                ui: Snapshot {
                    enabled: true,
                    ready: true,
                    ..Default::default()
                },
                added_sorted: true,
                ..Default::default()
            }),
            operations: tokio::sync::Mutex::new(()),
        }
    }
    #[test]
    fn sorting_uses_history_rank_deduplicates_and_keeps_unknowns_last() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        b.state.write().unwrap().videos = ["a", "b", "c", "a", "d"]
            .into_iter()
            .map(|id| Video {
                id: id.into(),
                ..Default::default()
            })
            .collect();
        assert_eq!(
            b.page(&[], true, 0)
                .unwrap()
                .videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
        b.set_sort(Sort::Played).unwrap();
        assert_eq!(
            b.page(&["c".into(), "b".into()], true, 0)
                .unwrap()
                .videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "b", "a", "d"]
        );
        b.page(&[], false, 0).unwrap();
        assert!(b.snapshot().note.contains("incomplete"));
    }
    #[test]
    fn switching_destinations_invalidates_inflight_results_and_preserves_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        b.configure(true, DestinationMode::Selected, Some("first".into()))
            .unwrap();
        let old = b.generation();
        b.configure(true, DestinationMode::Selected, Some("second".into()))
            .unwrap();
        b.finish(old, &Err(anyhow::anyhow!("stale failure")));
        assert!(b.snapshot().error.is_empty());
        assert!(!b.valid(old));
        b.configure(false, DestinationMode::Selected, None).unwrap();
        let saved: Preferences = serde_json::from_slice(&std::fs::read(&b.path).unwrap()).unwrap();
        assert_eq!(saved.id(), "second");
        assert!(!b.snapshot().enabled);
        assert_eq!(b.membership("missing"), 0);
    }

    #[test]
    fn desired_membership_is_idempotent_and_removes_every_matching_entry() {
        let rows = vec![
            Video {
                id: "a".into(),
                set_id: "entry1".into(),
                ..Default::default()
            },
            Video {
                id: "a".into(),
                set_id: "entry2".into(),
                ..Default::default()
            },
        ];
        assert!(edit_actions(&rows, "a", true).unwrap().is_empty());
        let remove = edit_actions(&rows, "a", false).unwrap();
        assert_eq!(remove.len(), 2);
        assert_eq!(remove[1]["setVideoId"], "entry2");
        assert!(edit_actions(&[], "a", false).unwrap().is_empty());
        assert_eq!(edit_actions(&[], "a", true).unwrap().len(), 1);
        assert!(
            edit_actions(
                &[Video {
                    id: "a".into(),
                    ..Default::default()
                }],
                "a",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn deleted_automatic_destination_can_be_resolved_again_without_changing_manual_choice() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        {
            let mut s = b.state.write().unwrap();
            s.ui.preferences.automatic_id = "deleted".into();
            s.ui.preferences.selected_id = "manual".into();
        }
        assert!(!b.forget_automatic("another", b.generation()).unwrap());
        assert!(b.forget_automatic("deleted", b.generation()).unwrap());
        let saved: Preferences = serde_json::from_slice(&std::fs::read(&b.path).unwrap()).unwrap();
        assert!(saved.automatic_id.is_empty());
        assert_eq!(saved.selected_id, "manual");
        assert!(!b.snapshot().ready);
        b.configure(true, DestinationMode::Selected, None).unwrap();
        assert!(!b.forget_automatic("", b.generation()).unwrap());
        assert_eq!(b.snapshot().preferences.id(), "manual");
    }
    #[tokio::test]
    async fn concurrent_initial_load_rechecks_ready_under_the_operation_lock() {
        let dir = tempfile::tempdir().unwrap();
        let b = std::sync::Arc::new(service(dir.path().join("account.json")));
        b.state.write().unwrap().ui.ready = false;
        let guard = b.operations.lock().await;
        let other = b.clone();
        let pending = tokio::spawn(async move { other.ensure_loaded().await });
        tokio::task::yield_now().await;
        b.state.write().unwrap().ui.ready = true;
        drop(guard);
        // No authenticated client is configured: a duplicate fetch would fail.
        pending.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn early_toggle_errors_are_retained_for_the_ui_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        b.mark_pending("invalid");
        assert!(b.toggle("invalid", b.generation()).await.is_err());
        assert_eq!(b.snapshot().error, "Invalid video.");
        assert_eq!(b.membership("invalid"), 1);
    }
    #[test]
    fn rendered_page_does_not_request_a_second_load_and_notes_match_sort() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        assert!(b.needs_render(0));
        b.page(&[], true, 0).unwrap();
        assert!(!b.needs_render(0));
        b.set_sort(Sort::Played).unwrap();
        assert!(b.needs_render(0));
        b.state.write().unwrap().added_sorted = false;
        b.page(&[], true, 0).unwrap();
        assert!(b.snapshot().note.is_empty());
        assert!(!b.needs_render(0));
    }
    #[test]
    fn recent_playlists_are_protected_and_played_order_tracks_history() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        b.state.write().unwrap().created = Some(("new".into(), std::time::Instant::now()));
        assert!(b.recently_created("new"));
        assert!(!b.recently_created("old"));
        b.state.write().unwrap().fetched = Some(("new".into(), std::time::Instant::now()));
        assert!(b.recently_fetched("new"));
        b.set_sort(Sort::Played).unwrap();
        b.page(&[], true, 4).unwrap();
        assert!(!b.needs_render(4));
        assert!(b.needs_render(5));
        b.set_sort(Sort::Added).unwrap();
        b.page(&[], true, 4).unwrap();
        assert!(!b.needs_render(5));
    }
    #[test]
    fn choice_loading_keeps_errors_it_did_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let b = service(dir.path().join("account.json"));
        b.finish_choices(b.generation(), &Err(anyhow::anyhow!("Playlists failed")));
        assert_eq!(b.snapshot().error, "Playlists failed");
        b.finish_choices(b.generation(), &Ok(()));
        assert!(b.snapshot().error.is_empty());
        b.set_error("Refresh failed".into());
        b.finish_choices(b.generation(), &Ok(()));
        assert_eq!(b.snapshot().error, "Refresh failed");
    }
    #[test]
    fn destination_modes_keep_independent_ids() {
        let mut p = Preferences {
            automatic_id: "default".into(),
            selected_id: "chosen".into(),
            ..Default::default()
        };
        assert_eq!(p.id(), "default");
        p.mode = DestinationMode::Selected;
        assert_eq!(p.id(), "chosen");
    }
    #[test]
    fn sort_edit_only_accepts_matching_playlist_and_order_action() {
        let mut item = json!({"title":"Date added (newest)","serviceEndpoint":{"playlistEditEndpoint":{"playlistId":"mine","params":"opaque","actions":[{"action":"ACTION_SET_PLAYLIST_VIDEO_ORDER","playlistVideoOrder":1}]}}});
        assert!(added_sort_edit(&item, "mine").is_some());
        assert!(added_sort_edit(&item, "other").is_none());
        item["serviceEndpoint"]["playlistEditEndpoint"]["actions"][0]["action"] =
            json!("ACTION_REMOVE_VIDEO");
        assert!(added_sort_edit(&item, "mine").is_none());
        assert!(editable(
            &json!({"sidebar":{"playlistSidebarPrimaryInfoRenderer":{"titleForm":{"inlineFormRenderer":{"onChange":{"playlistEditEndpoint":{}}}}}}})
        ));
    }
    #[test]
    fn editability_requires_positive_evidence() {
        assert!(!editable(
            &json!({"playlistHeaderRenderer":{"title":"Mine"}})
        ));
        assert!(editable(
            &json!({"playlistVideoListRenderer":{"isEditable":true}})
        ));
        assert!(selected_added_sort(
            &json!({"items":[{"title":"Date added (newest)","selected":true}]})
        ));
        assert!(!selected_added_sort(
            &json!({"title":"Date published (newest)","selected":true})
        ));
    }
}
