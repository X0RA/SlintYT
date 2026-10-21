//! Account actions and video details run independently of feed navigation.
use super::*;
use rustypipe::model::richtext::ToMarkdown;
use rustypipe::{
    client::ClientType,
    model::{Comment, paginator::Paginator},
};
use serde_json::{Value, json};
use std::sync::{Mutex, RwLock};

pub(super) type SessionSlot = Arc<RwLock<Option<Arc<Session>>>>;

/// Presentation only: never persist continuation tokens or account action payloads.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedComment {
    id: String,
    author: String,
    author_id: String,
    avatar: String,
    text: String,
    metadata: String,
    likes: String,
    replies: i32,
}

impl CachedComment {
    fn from_row(row: &crate::CommentData) -> Self {
        Self {
            id: row.id.to_string(),
            author: row.author.to_string(),
            author_id: row.author_id.to_string(),
            avatar: row.avatar.to_string(),
            text: row.text.to_string(),
            metadata: row.metadata.to_string(),
            likes: row.likes.to_string(),
            replies: row.replies,
        }
    }

    fn into_row(self) -> crate::CommentData {
        crate::CommentData {
            initials: initials(&self.author).into(),
            id: self.id.into(),
            author: self.author.into(),
            author_id: self.author_id.into(),
            avatar: self.avatar.into(),
            text: self.text.into(),
            metadata: self.metadata.into(),
            likes: self.likes.into(),
            replies: self.replies,
            ..Default::default()
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedDetails {
    description: String,
    markdown: String,
    channel: String,
    channel_id: String,
    avatar: String,
    stats: String,
    recommendations: Vec<Video>,
}

async fn restore_comments(
    session: &Session,
    id: &str,
    newest: bool,
    weak: &slint::Weak<AppWindow>,
    data: &Arc<Mutex<Discussion>>,
    generation: u64,
) {
    if let Some(cache) = &session.cache
        && let Some(rows) = cache
            .get::<Vec<CachedComment>>("comments-v1", &format!("{id}/{newest}"))
            .await
    {
        update_discussion(weak, data, generation, move |app| {
            app.global::<AppState>()
                .set_comments(ModelRc::new(VecModel::from(
                    rows.into_iter()
                        .map(CachedComment::into_row)
                        .collect::<Vec<_>>(),
                )));
        });
    }
}

#[derive(Default)]
struct Discussion {
    video: String,
    generation: u64,
    ready: bool,
    top: Paginator<Comment>,
    newest: Paginator<Comment>,
    initial_top: Paginator<Comment>,
    initial_newest: Paginator<Comment>,
    post_params: Option<String>,
    actions: std::collections::HashMap<String, youtube::renderers::CommentActions>,
    replies: std::collections::HashMap<String, Paginator<Comment>>,
    reply_rows: std::collections::HashMap<String, Vec<crate::CommentData>>,
}

impl Discussion {
    fn reset(&mut self, video: String) -> u64 {
        *self = Self {
            video,
            generation: self.generation + 1,
            ..Default::default()
        };
        self.generation
    }

    fn accepts(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn append_replies(
        &mut self,
        generation: u64,
        id: &str,
        mut page: Paginator<Comment>,
        raw: Option<&Value>,
    ) -> Option<(Vec<crate::CommentData>, Option<String>)> {
        if !self.accepts(generation) {
            return None;
        }
        let incoming = std::mem::take(&mut page.items)
            .into_iter()
            .map(|c| enrich_comment(c, 1, raw, &mut self.actions))
            .collect::<Vec<_>>();
        let next = page.ctoken.clone();
        self.replies.insert(id.to_owned(), page);
        let cached = self.reply_rows.entry(id.to_owned()).or_default();
        for row in incoming {
            if !cached.iter().any(|c| c.id == row.id) {
                cached.push(row);
            }
        }
        Some((cached.clone(), next))
    }
}

fn update_discussion(
    weak: &slint::Weak<AppWindow>,
    data: &Arc<Mutex<Discussion>>,
    generation: u64,
    apply: impl FnOnce(AppWindow) + Send + 'static,
) {
    let data = data.clone();
    update(weak, move |app| {
        let d = data.lock().unwrap();
        let ui = app.global::<AppState>();
        if !d.accepts(generation) || !ui.get_player_open() || ui.get_player_video_id() != d.video {
            return;
        }
        drop(d);
        apply(app);
    });
}

fn replace_replies(
    rows: &mut Vec<crate::CommentData>,
    parent: &str,
    replies: &[crate::CommentData],
    expanded: bool,
    token: Option<&str>,
) {
    let Some(index) = rows.iter().position(|c| c.id == parent) else {
        return;
    };
    let end = rows
        .iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, c)| c.indent <= rows[index].indent)
        .map_or(rows.len(), |(i, _)| i);
    rows.splice(index + 1..end, replies.iter().cloned());
    rows[index].expanded = expanded;
    if expanded {
        rows[index].token = token.unwrap_or_default().into();
    }
}

async fn request(session: &Session, endpoint: &str, body: Value) -> Result<Value, String> {
    let raw = session
        .client()
        .query()
        .authenticated()
        .raw(ClientType::Desktop, endpoint, &body)
        .await
        .map_err(|_| "YouTube could not complete this action. Please retry.".to_string())?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| "YouTube returned an unreadable response.".to_string())?;
    if value.get("error").is_some() || raw.contains("STATUS_FAILED") {
        return Err(
            "YouTube did not accept this action. Please check your account and retry.".into(),
        );
    }
    if let Some(cache) = &session.cache {
        if endpoint.starts_with("comment/") {
            cache.invalidate("comments-v1").await;
        } else if endpoint.starts_with("subscription/")
            || endpoint.starts_with("like/")
            || endpoint == "browse/edit_playlist"
        {
            cache.invalidate("pages-v1").await;
            cache.invalidate("details-v1").await;
        }
    }
    Ok(value)
}
fn account_profile_from_response(raw: &Value) -> Option<crate::config::AccountProfile> {
    let header = youtube::find_renderer(raw, "activeAccountHeaderRenderer")?;
    let name = youtube::text(&header["accountName"]);
    if name.trim().is_empty() {
        return None;
    }
    Some(crate::config::AccountProfile {
        name,
        avatar: youtube::avatar(&header["accountPhoto"]["thumbnails"]),
    })
}

async fn fetch_account_profile(
    client: &rustypipe::client::RustyPipe,
) -> Option<crate::config::AccountProfile> {
    let raw = client
        .query()
        .authenticated()
        .raw(ClientType::Desktop, "account/account_menu", &json!({}))
        .await
        .ok()?;
    let profile = account_profile_from_response(&serde_json::from_str::<Value>(&raw).ok()?)?;
    // Populate the existing shared disk image cache even before the menu opens.
    if !profile.avatar.is_empty() {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        let _ = youtube::download_thumbnail(&http, &profile.avatar).await;
    }
    Some(profile)
}

pub(super) async fn account_profile(session: &Session) -> Option<crate::config::AccountProfile> {
    if let Some(profile) = fetch_account_profile(session.client()).await {
        if let Some(cache) = &session.cache {
            cache.put("account-v2", "profile", &profile, crate::cache::DAY);
        }
        return Some(profile);
    }
    session.cache.as_ref()?.get("account-v2", "profile").await
}

/// Fill in older saved selections without starting playback or history indexing.
pub(super) async fn saved_profile_account(
    selection: &Selection,
) -> Option<crate::config::AccountProfile> {
    let (client, _, _) = youtube::authenticated_client(selection).await.ok()?;
    fetch_account_profile(&client).await
}

fn toast(weak: &slint::Weak<AppWindow>, result: Result<(), String>, success: String) {
    update(weak, move |app| {
        let state = app.global::<AppState>();
        state.set_action_busy(false);
        state.set_toast(result.err().unwrap_or(success).into());
    });
}
fn comment(c: Comment, indent: i32) -> crate::CommentData {
    let (author, id, avatar) = c
        .author
        .map(|a| (a.name, a.id, youtube::avatar_url(&a.avatar)))
        .unwrap_or_else(|| ("YouTube user".into(), String::new(), String::new()));
    crate::CommentData {
        id: c.id.into(),
        author: author.clone().into(),
        author_id: id.into(),
        initials: initials(&author).into(),
        avatar: avatar.into(),
        text: c
            .text
            .0
            .iter()
            .map(|t| t.get_text())
            .collect::<String>()
            .into(),
        metadata: c.publish_date_txt.into(),
        likes: c
            .like_count
            .map(|n| youtube::number(n.into()))
            .unwrap_or_default()
            .into(),
        replies: c.reply_count as i32,
        token: c.replies.ctoken.unwrap_or_default().into(),
        indent,
        ..Default::default()
    }
}

fn enrich_comment(
    c: Comment,
    indent: i32,
    raw: Option<&Value>,
    actions: &mut std::collections::HashMap<String, youtube::renderers::CommentActions>,
) -> crate::CommentData {
    let mut row = comment(c, indent);
    if let Some(raw) = raw {
        let action = youtube::renderers::comment_actions(raw, row.id.as_str());
        row.liked = action.liked;
        row.can_like = action.like.is_some() && action.unlike.is_some();
        let count = if action.liked {
            &action.liked_count
        } else {
            &action.unliked_count
        };
        if !count.is_empty() {
            row.likes = count.into();
        }
        if row.avatar.is_empty() {
            row.avatar = action.avatar.as_str().into();
        }
        actions.insert(row.id.to_string(), action);
    }
    row
}

pub(super) fn wire(
    app: &AppWindow,
    runtime: tokio::runtime::Handle,
    slot: SessionSlot,
    sender: mpsc::UnboundedSender<Command>,
) {
    let state = app.global::<AppState>();
    let discussion = Arc::new(Mutex::new(Discussion::default()));
    let (weak, rt, sessions, data) = (
        app.as_weak(),
        runtime.clone(),
        slot.clone(),
        discussion.clone(),
    );
    state.on_load_video_details(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        let generation = data.lock().unwrap().reset(id.to_string());
        ui.set_details_loading(true);
        ui.set_details_error("".into());
        ui.set_comments(ModelRc::default());
        ui.set_recommendations(ModelRc::default());
        ui.set_player_description("".into());
        ui.set_player_rich_description(slint::StyledText::default());
        ui.set_comments_loading(false);
        ui.set_comments_more(false);
        ui.set_comments_total("".into());
        ui.set_comments_error("".into());
        ui.set_comment_draft("".into());
        ui.set_player_channel("".into());
        ui.set_player_channel_avatar("".into());
        ui.set_player_channel_id("".into());
        ui.set_player_liked(false);
        ui.set_player_subscribed(false);
        let session = sessions.read().unwrap().clone();
        let (weak, data, id) = (weak.clone(), data.clone(), id.to_string());
        rt.spawn(async move {
            let Some(session) = session else { return };
            restore_comments(&session, &id, false, &weak, &data, generation).await;
            if let Some(cache) = &session.cache
                && let Some(mut details) = cache.get::<CachedDetails>("details-v1", &id).await
            {
                for video in &mut details.recommendations {
                    video.watch_hint = 0;
                    video.watched = 0;
                    video.watch_progress = None;
                }
                session.annotate(&mut details.recommendations);
                update_discussion(&weak, &data, generation, move |app| {
                    let ui = app.global::<AppState>();
                    ui.set_player_channel_initials(initials(&details.channel).into());
                    ui.set_player_channel_tone(tone(&details.channel));
                    ui.set_player_channel(details.channel.into());
                    crate::player::media_keys::refresh(&app);
                    ui.set_player_channel_id(details.channel_id.into());
                    ui.set_player_channel_avatar(details.avatar.into());
                    ui.set_player_stats(details.stats.into());
                    ui.set_player_rich_description(
                        slint::StyledText::from_markdown(&details.markdown).unwrap_or_else(|_| {
                            slint::StyledText::from_plain_text(&details.description)
                        }),
                    );
                    ui.set_player_description(details.description.into());
                    ui.set_recommendations(ModelRc::new(VecModel::from(
                        details
                            .recommendations
                            .into_iter()
                            .map(video_data)
                            .collect::<Vec<_>>(),
                    )));
                });
            }
            let query = session.client().query().authenticated();
            let (result, playlists) = tokio::join!(
                query.video_details(&id),
                session.page(&View::Playlists, None, None)
            );
            match result {
                Ok(details) => {
                    let description = details
                        .description
                        .0
                        .iter()
                        .map(|t| t.get_text())
                        .collect::<String>();
                    let markdown = details.description.to_markdown();
                    let mut recommendations = details
                        .recommended
                        .items
                        .into_iter()
                        .map(youtube::video_item)
                        .collect::<Vec<_>>();
                    session.annotate(&mut recommendations);
                    let comments_total = details.top_comments.count.map(youtube::number);
                    {
                        let mut d = data.lock().unwrap();
                        if !d.accepts(generation) {
                            return;
                        }
                        *d = Discussion {
                            video: id.clone(),
                            generation,
                            initial_top: details.top_comments.clone(),
                            initial_newest: details.latest_comments.clone(),
                            top: details.top_comments,
                            newest: details.latest_comments,
                            ..Default::default()
                        };
                    }
                    let stats = format!(
                        "{} views · {}",
                        youtube::number(details.view_count),
                        details.publish_date_txt.unwrap_or_default()
                    );
                    let channel = details.channel;
                    let raw = request(&session, "next", json!({"videoId":id})).await.ok();
                    if let Some(raw) = &raw {
                        let cards = youtube::renderers::recommendations(raw);
                        if !cards.is_empty() {
                            recommendations = cards;
                            session.annotate(&mut recommendations);
                        }
                    }
                    let subscribed = raw
                        .as_ref()
                        .and_then(|v| youtube::find_renderer(v, "subscribeButtonRenderer"))
                        .and_then(|v| v["subscribed"].as_bool())
                        .unwrap_or(false);
                    let liked = raw
                        .as_ref()
                        .is_some_and(|v| v.to_string().contains("LIKE_BUTTON_STATE_LIKED"));
                    if let Some(cache) = &session.cache {
                        cache.put(
                            "details-v1",
                            &id,
                            &CachedDetails {
                                description: description.clone(),
                                markdown: markdown.clone(),
                                channel: channel.name.clone(),
                                channel_id: channel.id.clone(),
                                avatar: youtube::avatar_url(&channel.avatar),
                                stats: stats.clone(),
                                recommendations: recommendations.clone(),
                            },
                            crate::cache::DAY,
                        );
                    }
                    {
                        let mut d = data.lock().unwrap();
                        if !d.accepts(generation) {
                            return;
                        }
                        d.ready = true;
                    }
                    let (next_weak, next_id) = (weak.clone(), id.clone());
                    let thumbs = recommendations.clone();
                    update_discussion(&weak, &data, generation, move |app| {
                        let ui = app.global::<AppState>();
                        if ui.get_player_video_id() != id {
                            return;
                        }
                        if let Ok(page) = playlists {
                            ui.set_save_playlists(ModelRc::new(VecModel::from(
                                page.videos
                                    .into_iter()
                                    .filter(|v| v.id != "WL" && v.id != "LL")
                                    .map(video_data)
                                    .collect::<Vec<_>>(),
                            )));
                        }
                        ui.set_details_loading(false);
                        if let Some(total) = comments_total {
                            ui.set_comments_total(total.into());
                        }
                        ui.set_player_channel(channel.name.clone().into());
                        crate::player::media_keys::refresh(&app);
                        ui.set_player_channel_id(channel.id.as_str().into());
                        ui.set_player_channel_initials(initials(&channel.name).into());
                        ui.set_player_channel_tone(tone(&channel.name));
                        ui.set_player_channel_avatar(
                            avatars::channel_avatar(
                                &channel.id,
                                youtube::avatar_url(&channel.avatar),
                            )
                            .into(),
                        );
                        ui.set_player_stats(stats.into());
                        ui.set_player_rich_description(
                            slint::StyledText::from_markdown(&markdown).unwrap_or_else(|_| {
                                slint::StyledText::from_plain_text(&description)
                            }),
                        );
                        ui.set_player_description(description.into());
                        ui.set_player_subscribed(subscribed);
                        ui.set_player_liked(liked);
                        ui.set_recommendations(ModelRc::new(VecModel::from(
                            recommendations
                                .into_iter()
                                .map(video_data)
                                .collect::<Vec<_>>(),
                        )));
                        ui.invoke_load_comments(false, false);
                    });
                    let http = reqwest::Client::new();
                    for (index, v) in thumbs.into_iter().take(18).enumerate() {
                        if let Ok(pixels) = youtube::download_thumbnail(&http, &v.thumbnail).await {
                            let id = next_id.clone();
                            update_discussion(&next_weak, &data, generation, move |app| {
                                let ui = app.global::<AppState>();
                                if ui.get_player_video_id() != id {
                                    return;
                                }
                                let model = ui.get_recommendations();
                                if let Some(mut row) = model.row_data(index) {
                                    row.thumbnail = thumbnail_image(&pixels);
                                    model.set_row_data(index, row);
                                }
                            });
                        }
                    }
                }
                Err(_) => update_discussion(&weak, &data, generation, move |app| {
                    let ui = app.global::<AppState>();
                    if ui.get_player_video_id() == id {
                        ui.set_details_loading(false);
                        ui.set_details_error(
                            "Could not load video details. Retry to try again.".into(),
                        );
                    }
                }),
            }
        });
    });
    let (weak, rt, sessions, data) = (
        app.as_weak(),
        runtime.clone(),
        slot.clone(),
        discussion.clone(),
    );
    state.on_load_comments(move |newest, more| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_comments_loading() || ui.get_details_loading() || !data.lock().unwrap().ready {
            return;
        }
        ui.set_comments_loading(true);
        ui.set_comments_error("".into());
        ui.set_comments_newest(newest);
        let (id, generation, token, visitor) = {
            let d = data.lock().unwrap();
            let p = if more {
                if newest { &d.newest } else { &d.top }
            } else if newest {
                &d.initial_newest
            } else {
                &d.initial_top
            };
            (
                d.video.clone(),
                d.generation,
                p.ctoken.clone(),
                p.visitor_data.clone(),
            )
        };
        let (weak, data, session) = (weak.clone(), data.clone(), sessions.read().unwrap().clone());
        rt.spawn(async move {
            let Some(session) = session else { return };
            if !more {
                restore_comments(&session, &id, newest, &weak, &data, generation).await;
            }
            let raw = if let Some(token) = &token {
                request(&session, "next", json!({"continuation":token}))
                    .await
                    .ok()
            } else {
                None
            };
            let result = if let Some(token) = token {
                session
                    .client()
                    .query()
                    .authenticated()
                    .video_comments(&token, visitor.as_deref())
                    .await
                    .map_err(|_| "Could not load comments. Please retry.")
            } else {
                Ok(Paginator::default())
            };
            match result {
                Ok(p) => {
                    let has_more = p.ctoken.is_some();
                    let total = p.count.map(youtube::number);
                    let items = p.items.clone();
                    {
                        let mut d = data.lock().unwrap();
                        if !d.accepts(generation) {
                            return;
                        }
                        if let Some(raw) = &raw {
                            d.post_params = youtube::find_renderer(raw, "createCommentEndpoint")
                                .and_then(|v| v["createCommentParams"].as_str())
                                .map(str::to_owned)
                                .or_else(|| d.post_params.clone());
                        }
                        if !more {
                            d.replies.clear();
                            d.reply_rows.clear();
                            d.actions.clear();
                        }
                        for c in &items {
                            d.replies.insert(c.id.clone(), c.replies.clone());
                        }
                        if newest {
                            d.newest = p;
                        } else {
                            d.top = p;
                        }
                    }
                    let display = {
                        let mut d = data.lock().unwrap();
                        if !d.accepts(generation) {
                            return;
                        }
                        items
                            .into_iter()
                            .map(|c| enrich_comment(c, 0, raw.as_ref(), &mut d.actions))
                            .collect::<Vec<_>>()
                    };
                    // Cache the first page; pagination continues with fresh server tokens.
                    if !more && let Some(cache) = &session.cache {
                        let rows = display
                            .iter()
                            .map(CachedComment::from_row)
                            .collect::<Vec<_>>();
                        cache.put(
                            "comments-v1",
                            &format!("{id}/{newest}"),
                            &rows,
                            crate::cache::DAY,
                        );
                    }
                    update_discussion(&weak, &data, generation, move |app| {
                        let ui = app.global::<AppState>();
                        if ui.get_player_video_id() != id {
                            return;
                        }
                        let mut rows = if more {
                            ui.get_comments().iter().collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                        rows.extend(display);
                        ui.set_comments(ModelRc::new(VecModel::from(rows)));
                        ui.set_comments_more(has_more);
                        ui.set_comments_loading(false);
                        if let Some(total) = total {
                            ui.set_comments_total(total.into());
                        }
                    });
                }
                Err(e) => update_discussion(&weak, &data, generation, move |app| {
                    let ui = app.global::<AppState>();
                    if ui.get_player_video_id() == id {
                        ui.set_comments_loading(false);
                        ui.set_comments_error(e.into());
                    }
                }),
            }
        });
    });
    let (weak, rt, sessions, data) = (
        app.as_weak(),
        runtime.clone(),
        slot.clone(),
        discussion.clone(),
    );
    state.on_load_replies(move |id, more| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_comments_loading() || ui.get_details_loading() {
            return;
        }
        let mut rows = ui.get_comments().iter().collect::<Vec<_>>();
        if !more
            && let Some(index) = rows.iter().position(|c| c.id == id)
            && rows[index].expanded
        {
            replace_replies(&mut rows, id.as_str(), &[], false, None);
            ui.set_comments(ModelRc::new(VecModel::from(rows)));
            return;
        }
        let (generation, p) = {
            let d = data.lock().unwrap();
            if !d.ready || ui.get_player_video_id() != d.video {
                return;
            }
            let Some(p) = d.replies.get(id.as_str()).cloned() else {
                return;
            };
            if !more && let Some(cached) = d.reply_rows.get(id.as_str()) {
                replace_replies(&mut rows, id.as_str(), cached, true, p.ctoken.as_deref());
                ui.set_comments(ModelRc::new(VecModel::from(rows)));
                return;
            }
            (d.generation, p)
        };
        let Some(token) = p.ctoken else { return };
        ui.set_comments_loading(true);
        ui.set_comments_error("".into());
        let (weak, data, session, id) = (
            weak.clone(),
            data.clone(),
            sessions.read().unwrap().clone(),
            id.to_string(),
        );
        rt.spawn(async move {
            let Some(session) = session else { return };
            let raw = request(&session, "next", json!({"continuation":token}))
                .await
                .ok();
            let result = session
                .client()
                .query()
                .authenticated()
                .video_comments(&token, p.visitor_data.as_deref())
                .await;
            match result {
                Ok(p) => {
                    let Some((display, next)) =
                        data.lock()
                            .unwrap()
                            .append_replies(generation, &id, p, raw.as_ref())
                    else {
                        return;
                    };
                    update_discussion(&weak, &data, generation, move |app| {
                        let ui = app.global::<AppState>();
                        let mut rows = ui.get_comments().iter().collect::<Vec<_>>();
                        replace_replies(&mut rows, &id, &display, true, next.as_deref());
                        ui.set_comments(ModelRc::new(VecModel::from(rows)));
                        ui.set_comments_loading(false);
                    });
                }
                Err(_) => update_discussion(&weak, &data, generation, move |app| {
                    let ui = app.global::<AppState>();
                    ui.set_comments_loading(false);
                    ui.set_comments_error("Could not load replies. Please retry.".into());
                }),
            }
        });
    });
    let (weak, rt, sessions, data) = (
        app.as_weak(),
        runtime.clone(),
        slot.clone(),
        discussion.clone(),
    );
    state.on_toggle_comment_like(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_action_busy() {
            return;
        }
        let Some(row) = ui.get_comments().iter().find(|c| c.id == id) else {
            return;
        };
        let action = data.lock().unwrap().actions.get(id.as_str()).cloned();
        let Some(action) = action else { return };
        let liked = !row.liked;
        let payload = if liked {
            action.like.clone()
        } else {
            action.unlike.clone()
        };
        let Some(payload) = payload else { return };
        let count = if liked {
            action.liked_count
        } else {
            action.unliked_count
        };
        let generation = data.lock().unwrap().generation;
        ui.set_action_busy(true);
        let (weak, session, id, data) = (
            weak.clone(),
            sessions.read().unwrap().clone(),
            id.to_string(),
            data.clone(),
        );
        rt.spawn(async move {
            let result = if let Some(session) = session {
                request(&session, "comment/perform_comment_action", payload)
                    .await
                    .map(|_| ())
            } else {
                Err("Connect a browser first.".into())
            };
            if result.is_ok() {
                let cache = data.clone();
                update_discussion(&weak, &data, generation, move |app| {
                    let ui = app.global::<AppState>();
                    for rows in cache.lock().unwrap().reply_rows.values_mut() {
                        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
                            row.liked = liked;
                            if !count.is_empty() {
                                row.likes = count.clone().into();
                            }
                        }
                    }
                    let model = ui.get_comments();
                    for i in 0..model.row_count() {
                        if let Some(mut row) = model.row_data(i)
                            && row.id == id
                        {
                            row.liked = liked;
                            if !count.is_empty() {
                                row.likes = count.clone().into();
                            }
                            model.set_row_data(i, row);
                            break;
                        }
                    }
                });
            }
            toast(
                &weak,
                result,
                if liked {
                    "Comment liked"
                } else {
                    "Comment like removed"
                }
                .into(),
            );
        });
    });
    let (weak, rt, sessions) = (app.as_weak(), runtime.clone(), slot.clone());
    state.on_toggle_subscription(move |id, subscribe| {
        let Some(app) = weak.upgrade() else { return };
        if app.global::<AppState>().get_action_busy() {
            return;
        }
        app.global::<AppState>().set_action_busy(true);
        let (weak, session, id) = (
            weak.clone(),
            sessions.read().unwrap().clone(),
            id.to_string(),
        );
        rt.spawn(async move {
            let result = if let Some(session) = session {
                request(
                    &session,
                    if subscribe {
                        "subscription/subscribe"
                    } else {
                        "subscription/unsubscribe"
                    },
                    json!({"channelIds":[id]}),
                )
                .await
                .map(|_| {
                    session.remember_subscription(&id, subscribe);
                })
            } else {
                Err("Connect a browser first.".into())
            };
            if result.is_ok() {
                let id = id.clone();
                update(&weak, move |app| {
                    let ui = app.global::<AppState>();
                    if ui.get_channel_id() == id {
                        ui.set_channel_subscribed(subscribe);
                    }
                    if ui.get_player_channel_id() == id {
                        ui.set_player_subscribed(subscribe);
                    }
                    let model = ui.get_videos();
                    for i in 0..model.row_count() {
                        if let Some(mut row) = model.row_data(i)
                            && row.id == id
                        {
                            row.subscribed = subscribe;
                            model.set_row_data(i, row);
                        }
                    }
                });
            }
            toast(
                &weak,
                result,
                if subscribe {
                    "Subscribed"
                } else {
                    "Unsubscribed"
                }
                .into(),
            );
        });
    });
    let (weak, rt, sessions) = (app.as_weak(), runtime.clone(), slot.clone());
    state.on_toggle_like(move || {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_action_busy() {
            return;
        }
        ui.set_action_busy(true);
        let liked = !ui.get_player_liked();
        let id = ui.get_player_video_id().to_string();
        let (weak, session) = (weak.clone(), sessions.read().unwrap().clone());
        rt.spawn(async move {
            let result = if let Some(session) = session {
                request(
                    &session,
                    if liked {
                        "like/like"
                    } else {
                        "like/removelike"
                    },
                    json!({"target":{"videoId":id}}),
                )
                .await
                .map(|_| ())
            } else {
                Err("Connect a browser first.".into())
            };
            if result.is_ok() {
                let id = id.clone();
                update(&weak, move |app| {
                    let ui = app.global::<AppState>();
                    if ui.get_player_video_id() == id {
                        ui.set_player_liked(liked);
                    }
                });
            }
            toast(
                &weak,
                result,
                if liked {
                    "Added to liked videos"
                } else {
                    "Like removed"
                }
                .into(),
            );
        });
    });
    let (weak, rt, sessions, save_sender) =
        (app.as_weak(), runtime.clone(), slot.clone(), sender.clone());
    state.on_save_video(move |playlist| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_action_busy() { return; }
        ui.set_action_busy(true);
        let id = ui.get_player_video_id().to_string();
        let (weak, session, sender) = (weak.clone(), sessions.read().unwrap().clone(), save_sender.clone());
        rt.spawn(async move {
            let result = if let Some(session) = &session {
                request(session, "browse/edit_playlist", json!({"playlistId":playlist.as_str(),"actions":[{"action":"ACTION_ADD_VIDEO","addedVideoId":id}]})).await.map(|_| ())
            } else { Err("Connect a browser first.".into()) };
            if result.is_ok() && let Some(session) = session {
                let _ = sender.send(Command::PlaylistChanged { bookmarks_current: false, playlist: playlist.to_string(), session });
            }
            toast(&weak, result, "Video saved to playlist".into());
        });
    });
    let (weak, rt, sessions) = (app.as_weak(), runtime.clone(), slot.clone());
    state.on_edit_playlist(move |index, destination| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if index < 0
            || ui.get_action_busy()
            || ui.get_busy()
            || ui.get_refreshing()
            || ui.get_view_kind() != 4
        {
            return;
        }
        let rows = ui.get_videos().iter().collect::<Vec<_>>();
        let Some(row) = rows.get(index as usize) else {
            return;
        };
        if row.set_id.is_empty() {
            return;
        }
        let destination = if destination < 0 {
            -1
        } else {
            destination.min(rows.len() as i32 - 1)
        };
        let action = playlist_action(&rows, index as usize, destination);
        let playlist = ui.get_current_playlist().to_string();
        ui.set_action_busy(true);
        let (weak, session, sender) = (
            weak.clone(),
            sessions.read().unwrap().clone(),
            sender.clone(),
        );
        rt.spawn(async move {
            let result = if let Some(session) = &session {
                request(
                    session,
                    "browse/edit_playlist",
                    json!({"playlistId":playlist,"actions":[action]}),
                )
                .await
                .map(|_| ())
            } else {
                Err("Connect a browser first.".into())
            };
            if result.is_ok()
                && let Some(session) = session
            {
                let _ = sender.send(Command::PlaylistChanged {
                    bookmarks_current: false,
                    playlist,
                    session,
                });
            }
            toast(
                &weak,
                result,
                if destination < 0 {
                    "Video removed"
                } else {
                    "Playlist reordered"
                }
                .into(),
            );
        });
    });
    let (weak, rt, sessions) = (app.as_weak(), runtime.clone(), slot);
    let data = discussion;
    state.on_submit_comment(move |text, parent| {
        if text.trim().is_empty() {
            return;
        }
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_action_busy() || ui.get_details_loading() {
            return;
        }
        let id = ui.get_player_video_id().to_string();
        let text = text.to_string();
        let (generation, post_params, reply_token, reply_params) = {
            let d = data.lock().unwrap();
            if !d.ready || d.video != id {
                return;
            }
            (
                d.generation,
                d.post_params.clone(),
                d.replies
                    .get(parent.as_str())
                    .and_then(|p| p.ctoken.clone()),
                d.actions.get(parent.as_str()).and_then(|a| a.reply.clone()),
            )
        };
        ui.set_action_busy(true);
        let (weak, session, data) = (weak.clone(), sessions.read().unwrap().clone(), data.clone());
        rt.spawn(async move {
            let result = async {
                let session = session.ok_or("Connect a browser first.")?;
                let raw = if parent.is_empty() && post_params.is_some() {
                    json!({"createCommentEndpoint":{"createCommentParams":post_params}})
                } else if !parent.is_empty() && reply_params.is_some() {
                    json!({"createCommentReplyEndpoint":{"createReplyParams":reply_params}})
                } else if let Some(token) = reply_token {
                    request(&session, "next", json!({"continuation":token})).await?
                } else {
                    request(&session, "next", json!({"videoId":id})).await?
                };
                let key = if parent.is_empty() {
                    "createCommentEndpoint"
                } else {
                    "createCommentReplyEndpoint"
                };
                let endpoint = youtube::find_renderer(&raw, key)
                    .ok_or("YouTube has not enabled posting for this thread.")?;
                let params = endpoint[if parent.is_empty() {
                    "createCommentParams"
                } else {
                    "createReplyParams"
                }]
                .as_str()
                .ok_or("YouTube has not enabled posting for this thread.")?;
                request(
                    &session,
                    if parent.is_empty() {
                        "comment/create_comment"
                    } else {
                        "comment/create_comment_reply"
                    },
                    if parent.is_empty() {
                        json!({"createCommentParams":params,"commentText":text})
                    } else {
                        json!({"createReplyParams":params,"commentText":text})
                    },
                )
                .await?;
                Ok(())
            }
            .await;
            if result.is_ok() {
                update_discussion(&weak, &data, generation, |app| {
                    app.global::<AppState>().set_comment_draft("".into());
                });
            }
            toast(&weak, result, "Comment posted".into());
        });
    });
    let weak = app.as_weak();
    state.on_follow_video_link(move |link| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if let Ok(url) = reqwest::Url::parse(link.as_str()) {
            let youtube = url.host_str().is_some_and(|h| {
                h == "youtube.com" || h.ends_with(".youtube.com") || h == "youtu.be"
            });
            if youtube {
                let video = url
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned());
                if video.as_deref() == Some(ui.get_player_video_id().as_str())
                    && let Some((_, time)) =
                        url.query_pairs().find(|(k, _)| k == "t" || k == "start")
                    && let Some(seconds) = timestamp(&time)
                {
                    ui.invoke_player_seek(seconds as f32);
                    return;
                }
                if let Some(tag) = url.path().strip_prefix("/hashtag/") {
                    ui.invoke_search(format!("#{tag}").into());
                    return;
                }
            }
        }
        ui.invoke_open_url(link);
    });
    let weak = app.as_weak();
    state.on_copy_link(move |url| {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            ui.set_clipboard_text(url);
            ui.set_clipboard_serial(ui.get_clipboard_serial() + 1);
            app.global::<AppState>().set_toast("Link copied".into());
        }
    });
    let (weak, rt) = (app.as_weak(), runtime);
    state.on_open_url(move |url| {
        let Ok(url) = reqwest::Url::parse(url.as_str()) else {
            return;
        };
        if !matches!(url.scheme(), "https" | "http") {
            return;
        }
        let weak = weak.clone();
        rt.spawn(async move {
            #[cfg(target_os = "linux")]
            let result = tokio::process::Command::new("xdg-open")
                .arg(url.as_str())
                .status()
                .await;
            #[cfg(target_os = "macos")]
            let result = tokio::process::Command::new("open")
                .arg(url.as_str())
                .status()
                .await;
            #[cfg(target_os = "windows")]
            let result = tokio::process::Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", url.as_str()])
                .status()
                .await;
            if !result.is_ok_and(|s| s.success()) {
                toast(
                    &weak,
                    Err("Could not open your browser.".into()),
                    String::new(),
                );
            }
        });
    });
}

fn playlist_action(rows: &[VideoData], index: usize, destination: i32) -> Value {
    let row = &rows[index];
    if destination < 0 {
        return json!({"action":"ACTION_REMOVE_VIDEO","setVideoId":row.set_id.as_str(),"removedVideoId":row.id.as_str()});
    }
    let successor = destination as usize + usize::from(destination as usize > index);
    let mut action = json!({"action":"ACTION_MOVE_VIDEO_BEFORE","setVideoId":row.set_id.as_str()});
    if let Some(next) = rows.get(successor) {
        action["movedSetVideoIdSuccessor"] = json!(next.set_id.as_str());
    }
    action
}

fn timestamp(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let (mut seconds, mut number, mut digits) = (0u64, 0u64, false);
    for c in value.chars() {
        if let Some(digit) = c.to_digit(10) {
            number = number.checked_mul(10)?.checked_add(digit.into())?;
            digits = true;
        } else {
            if !digits {
                return None;
            }
            let unit = match c {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            seconds = seconds.checked_add(number.checked_mul(unit)?)?;
            number = 0;
            digits = false;
        }
    }
    seconds.checked_add(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_menu_metadata_reads_names_and_avatars_without_other_account_data() {
        let raw = json!({"actions": [{"activeAccountHeaderRenderer": {
            "accountName": {"simpleText": "Example creator"},
            "accountPhoto": {"thumbnails": [{"url":"https://yt3.ggpht.com/avatar","width":160}]},
            "email": {"simpleText": "unused@example.invalid"}
        }}]});
        let profile = account_profile_from_response(&raw).unwrap();
        assert_eq!(profile.name, "Example creator");
        assert_eq!(profile.avatar, "https://yt3.ggpht.com/avatar");
        let mut rich = raw.clone();
        rich["actions"][0]["activeAccountHeaderRenderer"]["accountName"] =
            json!({"runs":[{"text":"Example "},{"text":"creator"}]});
        assert_eq!(account_profile_from_response(&rich).unwrap(), profile);
        assert!(
            account_profile_from_response(&json!({"activeAccountHeaderRenderer":{}})).is_none()
        );
        assert!(!serde_json::to_string(&profile).unwrap().contains("unused"));
    }

    #[tokio::test]
    #[ignore = "requires explicit SLINTYT_TEST_PROFILE authorization and network access; warms the image cache"]
    async fn live_account_profile_metadata() {
        let selection = Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("Set an authorized profile")
                .into(),
            keyring: None,
        };
        let profile = saved_profile_account(&selection).await;
        assert!(
            profile.is_some(),
            "Account display metadata was unavailable"
        );
        let profile = profile.unwrap();
        assert!(!profile.name.is_empty());
        assert!(!profile.avatar.is_empty());
        let bytes = crate::cache::Cache::new("public-images")
            .unwrap()
            .bytes("images-v1", &profile.avatar)
            .await;
        assert!(bytes.is_some(), "Profile image was not cached");
        assert!(image::load_from_memory(&bytes.unwrap()).is_ok());
        eprintln!(
            "Account name found and profile image verified in the disk cache; no cookie or response values printed"
        );
    }

    #[test]
    fn comment_snapshot_keeps_display_content_without_reusing_account_actions() {
        let row = crate::CommentData {
            id: "comment".into(),
            author: "Example".into(),
            text: "Saved comment text".into(),
            token: "old-reply-token".into(),
            can_like: true,
            liked: true,
            expanded: true,
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&CachedComment::from_row(&row)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("old-reply-token"));
        let restored = serde_json::from_slice::<CachedComment>(&bytes)
            .unwrap()
            .into_row();
        assert_eq!(restored.text, row.text);
        assert!(!restored.can_like);
        assert!(!restored.expanded);
        assert!(restored.token.is_empty());
    }

    fn reply_page(ids: &[&str], token: Option<&str>) -> Paginator<Comment> {
        let mut page = Paginator::default();
        page.ctoken = token.map(str::to_owned);
        page.visitor_data = Some("visitor".into());
        page.items = ids
            .iter()
            .map(|id| {
                serde_json::from_value(json!({
                    "id": id, "text": [], "author": null, "publish_date": null,
                    "publish_date_txt": "today", "like_count": 0, "reply_count": 0,
                    "replies": Paginator::<Comment>::default(),
                    "by_owner": false, "pinned": false, "hearted": false
                }))
                .unwrap()
            })
            .collect();
        page
    }

    #[test]
    fn switching_videos_clears_posting_tokens_and_rejects_old_results_even_after_returning() {
        let mut d = Discussion::default();
        let old = d.reset("a".into());
        d.ready = true;
        d.post_params = Some("post-to-a".into());
        d.initial_top = reply_page(&[], Some("comments-a"));
        d.actions.insert("parent-a".into(), Default::default());
        d.append_replies(
            old,
            "parent-a",
            reply_page(&["reply-a"], Some("next-a")),
            None,
        )
        .unwrap();
        d.reset("b".into());
        assert!(!d.ready);
        assert!(d.post_params.is_none());
        assert!(d.initial_top.ctoken.is_none());
        assert!(d.actions.is_empty());
        assert!(d.replies.is_empty());
        assert!(d.reply_rows.is_empty());
        assert!(
            d.append_replies(old, "parent-a", reply_page(&["late"], None), None)
                .is_none()
        );
        let current = d.reset("a".into());
        assert!(!d.accepts(old));
        assert!(d.accepts(current));
    }

    #[test]
    fn reply_pages_keep_their_cursor_and_survive_collapse_without_duplicates() {
        let mut d = Discussion::default();
        let generation = d.reset("video".into());
        let (first, token) = d
            .append_replies(
                generation,
                "parent",
                reply_page(&["r1"], Some("page-2")),
                None,
            )
            .unwrap();
        assert_eq!(d.replies["parent"].ctoken.as_deref(), Some("page-2"));
        assert_eq!(d.replies["parent"].visitor_data.as_deref(), Some("visitor"));
        let mut rows = vec![
            crate::CommentData {
                id: "parent".into(),
                ..Default::default()
            },
            crate::CommentData {
                id: "other".into(),
                ..Default::default()
            },
        ];
        replace_replies(&mut rows, "parent", &first, true, token.as_deref());
        assert_eq!(
            rows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["parent", "r1", "other"]
        );
        replace_replies(&mut rows, "parent", &[], false, None);
        assert_eq!(rows.len(), 2);
        replace_replies(
            &mut rows,
            "parent",
            &d.reply_rows["parent"],
            true,
            token.as_deref(),
        );
        assert_eq!(rows.len(), 3);
        let (all, token) = d
            .append_replies(generation, "parent", reply_page(&["r1", "r2"], None), None)
            .unwrap();
        replace_replies(&mut rows, "parent", &all, true, token.as_deref());
        assert_eq!(
            rows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["parent", "r1", "r2", "other"]
        );
        assert!(rows[0].token.is_empty());
        assert!(d.replies["parent"].ctoken.is_none());
    }
    #[test]
    fn playlist_drag_uses_the_successor_after_removing_the_moved_row() {
        let rows = ["a", "b", "c", "d"].map(|id| VideoData {
            id: id.into(),
            set_id: format!("set-{id}").into(),
            ..Default::default()
        });
        assert_eq!(
            playlist_action(&rows, 0, 2)["movedSetVideoIdSuccessor"],
            "set-d"
        );
        assert_eq!(
            playlist_action(&rows, 3, 0)["movedSetVideoIdSuccessor"],
            "set-a"
        );
        assert!(
            playlist_action(&rows, 0, 3)
                .get("movedSetVideoIdSuccessor")
                .is_none()
        );
        assert_eq!(playlist_action(&rows, 0, -1)["removedVideoId"], "a");
    }
    #[test]
    fn description_timestamps_accept_seconds_and_youtube_units() {
        assert_eq!(timestamp("125"), Some(125));
        assert_eq!(timestamp("1h2m3s"), Some(3723));
        assert_eq!(timestamp("2m5"), Some(125));
        assert_eq!(timestamp("bad"), None);
        assert_eq!(timestamp("999999999999999999999999999h"), None);
    }
}
