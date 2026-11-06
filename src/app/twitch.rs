use super::*;
use crate::{
    TwitchFollow, TwitchRecent, TwitchResult,
    twitch::{self as api, Client, SearchMode, SearchResult, VideoKind},
    twitch_library::{self as library, Follow},
};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// How many pages one filter may walk through before it stops and says so.
const FILTER_PAGES: u32 = 20;
/// How often a playing Twitch video's position is written down.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
/// Followed channels shown before "See all".
const FOLLOWING_PREVIEW: usize = 8;

/// Everything loaded so far. The UI models hold the sorted or filtered view of it.
#[derive(Default)]
struct Lists {
    search: Vec<SearchResult>,
    channels: Vec<SearchResult>,
    user: Vec<SearchResult>,
    user_cursor: Option<String>,
    filter_pages: u32,
}

thread_local! {
    static LISTS: RefCell<Lists> = RefCell::default();
    /// Each followed channel's newest video IDs, once they have been checked.
    static LATEST: RefCell<HashMap<String, Vec<u64>>> = RefCell::default();
}

/// What a request needs to run and to report back to the UI thread.
#[derive(Clone)]
struct Task {
    weak: slint::Weak<AppWindow>,
    runtime: tokio::runtime::Handle,
    client: Option<Arc<Client>>,
    generation: Arc<AtomicU64>,
}

pub(super) fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let client = Client::new().ok().map(Arc::new);
    let ui = app.global::<AppState>();
    let enabled = Store::new()
        .and_then(|s| s.load_settings())
        .map(|s| s.twitch_enabled)
        .unwrap_or(false);
    ui.set_twitch_enabled(enabled);
    if enabled {
        prepare_operations(&runtime, &client);
    }
    let (weak, rt, api) = (app.as_weak(), runtime.clone(), client.clone());
    ui.on_set_twitch_enabled(move |enabled| {
        let result = Store::new().and_then(|s| s.save_twitch_enabled(enabled));
        if let (Err(e), Some(app)) = (result, weak.upgrade()) {
            let ui = app.global::<AppState>();
            ui.set_twitch_enabled(!enabled);
            ui.set_toast(format!("Could not save Twitch setting: {e}").into());
        } else if enabled {
            prepare_operations(&rt, &api);
        }
    });
    let search = Task {
        weak: app.as_weak(),
        runtime: runtime.clone(),
        client: client.clone(),
        generation: Arc::new(AtomicU64::new(0)),
    };
    let user = Task {
        generation: Arc::new(AtomicU64::new(0)),
        ..search.clone()
    };
    let channels = Task {
        generation: Arc::new(AtomicU64::new(0)),
        ..search.clone()
    };
    let landing = search.clone();
    let channel_task = channels.clone();
    let task = search.clone();
    ui.on_twitch_search(move |query| {
        let Some(app) = task.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let query = query.trim().to_owned();
        if query.is_empty() || !ui.get_twitch_enabled() {
            return;
        }
        {
            let mut library = library::shared();
            library.add_search(&query, 0);
            // Only the list of recent searches is lost if this fails.
            let _ = library.save();
        }
        show_landing(&ui);
        LISTS.with_borrow_mut(|lists| {
            lists.search.clear();
            lists.channels.clear();
        });
        ui.set_twitch_results(ModelRc::default());
        ui.set_twitch_cursor("".into());
        ui.set_twitch_results_query(query.as_str().into());
        ui.set_twitch_channels(ModelRc::default());
        ui.set_twitch_channel_cursor("".into());
        ui.set_twitch_video_total(0);
        ui.set_twitch_channel_total(0);
        ui.set_twitch_results_tab(0);
        ui.set_twitch_sort(0);
        run_search(&ui, &task, query.clone(), None, false);
        run_search(&ui, &channel_task, query, None, true);
    });
    let task = search;
    ui.on_twitch_load_more(move || {
        let Some(app) = task.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let cursor = ui.get_twitch_cursor().to_string();
        if ui.get_twitch_busy() || cursor.is_empty() {
            return;
        }
        let query = ui.get_twitch_results_query().to_string();
        run_search(&ui, &task, query, Some(cursor), false);
    });
    let task = channels.clone();
    ui.on_twitch_load_more_channels(move || {
        if let Some(app) = task.weak.upgrade() {
            let ui = app.global::<AppState>();
            let cursor = ui.get_twitch_channel_cursor().to_string();
            if !ui.get_twitch_channels_busy() && !cursor.is_empty() {
                run_search(
                    &ui,
                    &task,
                    ui.get_twitch_results_query().to_string(),
                    Some(cursor),
                    true,
                );
            }
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_sort_changed(move || {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            show_search(&ui);
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_play(move |id, title, channel| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        // Kept so its progress has somewhere to go, and resume knows it.
        remember(&id, &title, &channel);
        ui.invoke_play_video(format!("twitch:{id}").into(), title);
        ui.set_player_channel(channel);
        crate::player::media_keys::refresh(&app);
    });
    let task = user.clone();
    ui.on_twitch_open_user(move |item| {
        let Some(app) = task.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let login = item.id.to_string();
        ui.set_twitch_user_open(true);
        ui.set_twitch_user_login(item.id);
        ui.set_twitch_user_name(item.title);
        ui.set_twitch_user_meta(item.subtitle);
        ui.set_twitch_user_description("".into());
        ui.set_twitch_user_avatar(item.picture);
        ui.set_twitch_user_initials(item.initials);
        ui.set_twitch_user_tone(item.tone);
        ui.set_twitch_user_tab(0);
        ui.set_twitch_user_sort(0);
        ui.set_twitch_user_filter("".into());
        let following = library::shared().following(&login);
        ui.set_twitch_user_following(following);
        reload_user(&ui, &task);
        if following {
            // Opening the channel counts its videos as seen.
            refresh_follow(&task, login.clone(), true);
        }
        load_user_info(&task, login);
    });
    let task = user.clone();
    ui.on_twitch_user_reload(move || {
        if let Some(app) = task.weak.upgrade() {
            reload_user(&app.global::<AppState>(), &task);
        }
    });
    let task = user;
    ui.on_twitch_user_load_more(move || {
        let Some(app) = task.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let cursor = LISTS.with_borrow(|lists| lists.user_cursor.clone());
        if !ui.get_twitch_user_busy() && cursor.is_some() {
            run_user(&ui, &task, cursor);
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_user_filter_changed(move || {
        if let Some(app) = weak.upgrade() {
            LISTS.with_borrow_mut(|lists| lists.filter_pages = 0);
            show_user(&app.global::<AppState>());
        }
    });
    wire_library(app);
    wire_landing(app, landing, channels);
}

/// Recent searches for the dropdown, and followed channels and bookmarks for the landing page.
fn wire_landing(app: &AppWindow, task: Task, channels: Task) {
    let ui = app.global::<AppState>();
    show_landing(&ui);
    let weak = app.as_weak();
    ui.on_twitch_recent_refresh(move || {
        if let Some(app) = weak.upgrade() {
            show_landing(&app.global::<AppState>());
        }
    });
    let refresh = task.clone();
    ui.on_twitch_landing_refresh(move || {
        let Some(app) = refresh.weak.upgrade() else {
            return;
        };
        show_landing(&app.global::<AppState>());
        refresh_bookmark_avatars(&refresh);
        for follow in library::shared().follows() {
            refresh_follow(&refresh, follow.login, false);
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_recent_remove(move |query| {
        if let Some(app) = weak.upgrade() {
            {
                let mut library = library::shared();
                let indices: Vec<_> = library
                    .searches()
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.query.eq_ignore_ascii_case(&query))
                    .map(|(i, _)| i)
                    .collect();
                for i in indices.into_iter().rev() {
                    library.remove_search(i);
                }
                let _ = library.save();
            }
            show_landing(&app.global::<AppState>());
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_recent_clear(move || {
        if let Some(app) = weak.upgrade() {
            {
                let mut library = library::shared();
                library.clear_searches();
                let _ = library.save();
            }
            show_landing(&app.global::<AppState>());
        }
    });
    let clear = task.clone();
    ui.on_twitch_clear_search(move || {
        let Some(app) = clear.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        // A search still running would otherwise fill the page back in.
        clear.generation.fetch_add(1, Ordering::Relaxed);
        channels.generation.fetch_add(1, Ordering::Relaxed);
        LISTS.with_borrow_mut(|lists| lists.channels.clear());
        ui.set_twitch_channels(ModelRc::default());
        ui.set_twitch_channel_cursor("".into());
        ui.set_twitch_channels_busy(false);
        LISTS.with_borrow_mut(|lists| lists.search.clear());
        ui.set_twitch_results(ModelRc::default());
        ui.set_twitch_cursor("".into());
        ui.set_twitch_results_query("".into());
        ui.set_twitch_busy(false);
        ui.set_twitch_error("".into());
        ui.set_twitch_channels_error("".into());
        ui.set_twitch_sort(0);
        show_landing(&ui);
    });
    ui.on_twitch_toggle_follow(move |item| {
        let Some(app) = task.weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let follow = Follow {
            login: item.id.into(),
            name: item.title.into(),
            picture: item.picture.into(),
            ..Follow::default()
        };
        let login = follow.login.clone();
        let now = crate::history::now();
        let result = {
            let mut library = library::shared();
            let following = library.toggle_follow(follow.clone(), now);
            let saved = library.save();
            if saved.is_err() {
                library.toggle_follow(follow, now);
            }
            saved.map(|()| following)
        };
        match result {
            Ok(following) => {
                if ui.get_twitch_user_login() == login {
                    ui.set_twitch_user_following(following);
                }
                ui.set_twitch_library_epoch(ui.get_twitch_library_epoch() + 1);
                if following {
                    refresh_follow(&task, login, true);
                }
            }
            Err(error) => ui.set_toast(format!("{error:#}").into()),
        }
        show_landing(&ui);
    });
}

/// Fetches a followed channel's newest videos to count the new ones. With
/// `seen`, or the first time, they all count as seen.
fn refresh_follow(task: &Task, login: String, seen: bool) {
    let Some(client) = task.client.clone() else {
        return;
    };
    let weak = task.weak.clone();
    task.runtime.spawn(async move {
        let Ok(page) = client.latest_videos(&login).await else {
            return;
        };
        let ids: Vec<u64> = page
            .results
            .iter()
            .filter_map(|video| video.id.parse().ok())
            .collect();
        update(&weak, move |app| {
            let ui = app.global::<AppState>();
            // The channel may have been opened while this was loading.
            let open = ui.get_twitch_user_open() && ui.get_twitch_user_login() == login;
            if let Some(newest) = ids.iter().max().copied() {
                let mut library = library::shared();
                let first = library.follow(&login).is_some_and(|f| f.seen.is_none());
                if (seen || open || first) && library.mark_seen(&login, newest) {
                    let _ = library.save();
                }
            }
            LATEST.with_borrow_mut(|latest| latest.insert(login, ids));
            show_landing(&ui);
        });
    });
}

fn show_landing(ui: &AppState) {
    let (recent, follows) = {
        let library = library::shared();
        let filter = ui.get_twitch_search_text().to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let recent: Vec<_> = library
            .searches()
            .iter()
            .filter(|s| {
                s.query.to_lowercase().contains(&filter) && seen.insert(s.query.to_lowercase())
            })
            .map(|s| TwitchRecent {
                query: s.query.as_str().into(),
            })
            .collect();
        (recent, library.follows())
    };
    let mut follows: Vec<_> = LATEST.with_borrow(|latest| {
        follows
            .into_iter()
            .map(|follow| {
                let new = latest
                    .get(&follow.login)
                    .map(|ids| library::new_videos(ids, follow.seen));
                (new, follow)
            })
            .collect()
    });
    // Channels with new videos first, then by name.
    follows.sort_by_cached_key(|(new, follow)| {
        (
            std::cmp::Reverse(new.unwrap_or(0)),
            follow.name.to_lowercase(),
        )
    });
    ui.set_twitch_following_total(follows.len() as i32);
    if !ui.get_twitch_following_all() {
        follows.truncate(FOLLOWING_PREVIEW);
    }
    let cards: Vec<_> = follows
        .into_iter()
        .map(|(new, follow)| {
            let name = if follow.name.is_empty() {
                follow.login.clone()
            } else {
                follow.name
            };
            TwitchFollow {
                initials: initials(&name).into(),
                tone: tone(&name),
                status: match new {
                    None => "".into(),
                    Some(0) => "Up to date".into(),
                    Some(1) => "1 new video".into(),
                    Some(n) if n >= api::LATEST_PAGE as usize => format!("{n}+ new videos").into(),
                    Some(n) => format!("{n} new videos").into(),
                },
                fresh: new.is_some_and(|n| n > 0),
                login: follow.login.into(),
                name: name.into(),
                picture: follow.picture.into(),
            }
        })
        .collect();
    ui.set_twitch_recent(ModelRc::new(VecModel::from(recent)));
    ui.set_twitch_following_list(ModelRc::new(VecModel::from(cards)));
    show_bookmarks(ui);
}

/// Local bookmarks and watch progress, kept in `twitch_library`.
fn wire_library(app: &AppWindow) {
    let ui = app.global::<AppState>();
    ui.on_twitch_bookmarked(|id, _| library::shared().bookmarked(&id));
    ui.on_twitch_progress(|id, _| library::shared().progress(&id));
    ui.on_twitch_following(|login, _| library::shared().following(&login));
    let weak = app.as_weak();
    ui.on_twitch_toggle_bookmark(move |id| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
        let error = {
            let mut library = library::shared();
            if !library.known(&id)
                && let Some(video) = loaded(&id)
            {
                library.remember(video);
            }
            let now = crate::history::now();
            if library.toggle_bookmark(&id, now).is_none() {
                return;
            }
            let result = library.save();
            if result.is_err() {
                library.toggle_bookmark(&id, now);
            }
            result.err()
        };
        if let Some(error) = error {
            ui.set_toast(format!("{error:#}").into());
        }
        ui.set_twitch_library_epoch(ui.get_twitch_library_epoch() + 1);
        show_bookmarks(&ui);
    });
    // The player has no hook for Twitch, so its position is read on a timer.
    let timer = Rc::new(slint::Timer::default());
    let keep_alive = timer.clone();
    let weak = app.as_weak();
    timer.start(slint::TimerMode::Repeated, PROGRESS_INTERVAL, move || {
        let Some(app) = weak.upgrade() else {
            keep_alive.stop();
            return;
        };
        let ui = app.global::<AppState>();
        // Before a video loads its position is still 0, which would undo progress.
        if !ui.get_player_open()
            || !ui.get_player_twitch()
            || ui.get_player_loading()
            || !ui.get_player_error().is_empty()
        {
            return;
        }
        let length = f64::from(ui.get_player_duration());
        let position = if ui.get_player_ended() {
            length
        } else {
            f64::from(ui.get_player_position())
        };
        let id = ui.get_player_video_id();
        let mut library = library::shared();
        if library.record(&id, position, length, crate::history::now()) {
            // A failed write is retried on the next tick.
            let _ = library.save();
            drop(library);
            ui.set_twitch_library_epoch(ui.get_twitch_library_epoch() + 1);
        }
    });
}

/// A loaded search or user page row, which has the details a bookmark shows.
fn loaded(id: &str) -> Option<SearchResult> {
    LISTS.with_borrow(|lists| {
        lists
            .search
            .iter()
            .chain(&lists.user)
            .find(|result| result.id == id)
            .cloned()
    })
}

fn remember(id: &str, title: &str, channel: &str) {
    let mut library = library::shared();
    let video = loaded(id).or_else(|| {
        (!library.known(id)).then(|| SearchResult {
            id: id.into(),
            title: title.into(),
            ..SearchResult::default()
        })
    });
    if let Some(mut video) = video {
        if video.subtitle.is_empty() {
            video.subtitle = channel.into();
        }
        library.remember(video);
    }
}

/// All local bookmarks, newest first.
fn show_bookmarks(ui: &AppState) {
    let mut entries: Vec<_> = library::shared().bookmarks().into_iter().collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.bookmarked_at));
    let rows: Vec<_> = entries.into_iter().map(|e| row(e.video)).collect();
    ui.set_twitch_bookmarks(ModelRc::new(VecModel::from(rows)));
}

fn prepare_operations(runtime: &tokio::runtime::Handle, client: &Option<Arc<Client>>) {
    let Some(client) = client.clone() else {
        return;
    };
    // A failure here isn't shown: the next search retries and reports it.
    runtime.spawn(async move {
        let _ = client.prepare().await;
    });
}

fn run_search(ui: &AppState, task: &Task, query: String, cursor: Option<String>, channels: bool) {
    let Some(client) = task.client.clone() else {
        ui.set_twitch_error("Could not open the SlintYT config folder.".into());
        return;
    };
    let mode = if channels {
        SearchMode::User
    } else {
        SearchMode::Video
    };
    let epoch = task.generation.fetch_add(1, Ordering::Relaxed) + 1;
    if channels {
        ui.set_twitch_channels_busy(true);
        ui.set_twitch_channels_error("".into());
    } else {
        ui.set_twitch_busy(true);
        ui.set_twitch_error("".into());
    }
    let (weak, generation) = (task.weak.clone(), task.generation.clone());
    task.runtime.spawn(async move {
        let mut result = client.search(mode, &query, cursor.as_deref()).await;
        if !channels && let Ok(page) = &mut result {
            video_avatars(&client, page).await;
        }
        update(&weak, move |app| {
            if generation.load(Ordering::Relaxed) != epoch {
                return;
            }
            let ui = app.global::<AppState>();
            if channels {
                ui.set_twitch_channels_busy(false);
            } else {
                ui.set_twitch_busy(false);
            }
            match result {
                Ok(page) => {
                    let total = page
                        .total
                        .unwrap_or(page.results.len() as u64)
                        .min(i32::MAX as u64) as i32;
                    if channels {
                        LISTS.with_borrow_mut(|lists| lists.channels.extend(page.results));
                        ui.set_twitch_channel_cursor(page.cursor.unwrap_or_default().into());
                        ui.set_twitch_channel_total(total.max(ui.get_twitch_channel_total()));
                    } else {
                        LISTS.with_borrow_mut(|lists| lists.search.extend(page.results));
                        ui.set_twitch_cursor(page.cursor.unwrap_or_default().into());
                        ui.set_twitch_video_total(total.max(ui.get_twitch_video_total()));
                    }
                    show_search(&ui);
                }
                Err(error) => {
                    if channels {
                        ui.set_twitch_channels_error(format!("{error:#}").into());
                    } else {
                        ui.set_twitch_error(format!("{error:#}").into());
                    }
                }
            }
        });
    });
}

fn reload_user(ui: &AppState, task: &Task) {
    LISTS.with_borrow_mut(|lists| {
        lists.user.clear();
        lists.user_cursor = None;
        lists.filter_pages = 0;
    });
    ui.set_twitch_user_videos(ModelRc::default());
    ui.set_twitch_user_more(false);
    ui.set_twitch_user_note("".into());
    run_user(ui, task, None);
}

fn load_user_info(task: &Task, login: String) {
    let Some(client) = task.client.clone() else {
        return;
    };
    let weak = task.weak.clone();
    task.runtime.spawn(async move {
        // The header already shows the name and picture from the search result.
        let Ok(info) = client.user_info(&login).await else {
            return;
        };
        update(&weak, move |app| {
            let ui = app.global::<AppState>();
            if ui.get_twitch_user_login() == login {
                ui.set_twitch_user_description(info.description.into());
                ui.set_twitch_user_meta(format!("@{login} · {}", info.followers).into());
            }
        });
    });
}

fn run_user(ui: &AppState, task: &Task, cursor: Option<String>) {
    let Some(client) = task.client.clone() else {
        ui.set_twitch_user_error("Could not open the SlintYT config folder.".into());
        return;
    };
    let login = ui.get_twitch_user_login().to_string();
    let kind = VideoKind::from_index(ui.get_twitch_user_tab());
    let popular = ui.get_twitch_user_sort() == 1;
    let epoch = task.generation.fetch_add(1, Ordering::Relaxed) + 1;
    ui.set_twitch_user_busy(true);
    ui.set_twitch_user_error("".into());
    let (weak, generation) = (task.weak.clone(), task.generation.clone());
    task.runtime.spawn(async move {
        let mut result = client
            .user_videos(&login, kind, popular, cursor.as_deref())
            .await;
        if let Ok(page) = &mut result {
            video_avatars(&client, page).await;
        }
        update(&weak, move |app| {
            if generation.load(Ordering::Relaxed) != epoch {
                return;
            }
            let ui = app.global::<AppState>();
            ui.set_twitch_user_busy(false);
            match result {
                Ok(page) => {
                    LISTS.with_borrow_mut(|lists| {
                        lists.user.extend(page.results);
                        lists.user_cursor = page.cursor;
                        lists.filter_pages += 1;
                    });
                    show_user(&ui);
                }
                Err(error) => ui.set_twitch_user_error(format!("{error:#}").into()),
            }
        });
    });
}

fn show_search(ui: &AppState) {
    let channel_sort = if ui.get_twitch_results_tab() == 1 {
        ui.get_twitch_sort()
    } else {
        0
    };
    let channels = LISTS.with_borrow(|lists| {
        api::sorted(&lists.channels, true, channel_sort)
            .into_iter()
            .map(row)
            .collect()
    });
    if let Some(rows) = replacement(&ui.get_twitch_channels(), channels) {
        ui.set_twitch_channels(rows);
    }
    let rows: Vec<_> = LISTS.with_borrow(|lists| {
        api::sorted(
            &lists.search,
            false,
            if ui.get_twitch_results_tab() == 1 {
                0
            } else {
                ui.get_twitch_sort()
            },
        )
        .into_iter()
        .filter(|r| api::length_matches(r.seconds, ui.get_twitch_length()))
        .map(row)
        .collect()
    });
    if let Some(rows) = replacement(&ui.get_twitch_results(), rows) {
        ui.set_twitch_results(rows);
    }
}

fn show_user(ui: &AppState) {
    let filter = ui.get_twitch_user_filter().to_string();
    let (rows, loaded, more, capped) = LISTS.with_borrow(|lists| {
        let rows: Vec<_> = lists
            .user
            .iter()
            .filter(|result| api::matches_filter(result, &filter))
            .cloned()
            .map(row)
            .collect();
        let capped = !filter.trim().is_empty() && lists.filter_pages >= FILTER_PAGES;
        let more = lists.user_cursor.is_some() && !capped;
        (
            rows,
            lists.user.len(),
            more,
            capped && lists.user_cursor.is_some(),
        )
    });
    if let Some(rows) = replacement(&ui.get_twitch_user_videos(), rows) {
        ui.set_twitch_user_videos(rows);
    }
    ui.set_twitch_user_more(more);
    ui.set_twitch_user_note(if capped {
        format!("Searched the {loaded} most recently loaded videos.").into()
    } else {
        "".into()
    });
}

/// A new page only adds rows, so rows already on screen aren't rebuilt.
/// Returns a new model when the list changed in any other way.
fn replacement(
    current: &ModelRc<TwitchResult>,
    rows: Vec<TwitchResult>,
) -> Option<ModelRc<TwitchResult>> {
    let shown = current.row_count();
    let appendable = rows.len() >= shown && current.iter().eq(rows[..shown].iter().cloned());
    match current.as_any().downcast_ref::<VecModel<TwitchResult>>() {
        Some(model) if appendable => {
            for row in rows.into_iter().skip(shown) {
                model.push(row);
            }
            None
        }
        _ => Some(ModelRc::new(VecModel::from(rows))),
    }
}

fn row(result: SearchResult) -> TwitchResult {
    let video = !result.duration.is_empty();
    let label = if video {
        &result.subtitle
    } else {
        &result.title
    };
    let stats = if video {
        let views = api::views(result.count);
        if result.published_at > 0 {
            format!(
                "{views} · {}",
                api::ago(result.published_at, crate::history::now())
            )
        } else {
            views
        }
    } else {
        api::followers(result.count)
    };
    TwitchResult {
        initials: initials(label).into(),
        tone: tone(label),
        channel: result.subtitle.clone().into(),
        stats: stats.into(),
        game: result.game.into(),
        kind: result.kind.into(),
        avatar: result.avatar.into(),
        id: result.id.into(),
        title: result.title.into(),
        subtitle: result.subtitle.into(),
        metadata: result.metadata.into(),
        duration: result.duration.into(),
        picture: result.picture.into(),
    }
}

/// Video queries sometimes omit profile images. Channel search supplies them,
/// using the same disk cache as ordinary searches and matching the exact login.
async fn video_avatars(client: &Arc<Client>, page: &mut api::SearchPage) {
    let logins: std::collections::HashSet<_> = page
        .results
        .iter()
        .filter(|video| video.avatar.is_empty() && !video.channel.is_empty())
        .map(|video| video.channel.to_lowercase())
        .collect();
    let mut requests = tokio::task::JoinSet::new();
    for login in logins {
        let client = client.clone();
        requests.spawn(async move {
            let page = client.search(SearchMode::User, &login, None).await.ok()?;
            let picture = page
                .results
                .into_iter()
                .find(|channel| channel.id.eq_ignore_ascii_case(&login))?
                .picture;
            Some((login, picture))
        });
    }
    let mut avatars = HashMap::new();
    while let Some(result) = requests.join_next().await {
        if let Ok(Some((login, picture))) = result {
            avatars.insert(login, picture);
        }
    }
    for video in &mut page.results {
        if video.avatar.is_empty()
            && let Some(picture) = avatars.get(&video.channel.to_lowercase())
        {
            video.avatar = picture.clone();
        }
    }
}

fn refresh_bookmark_avatars(task: &Task) {
    let Some(client) = task.client.clone() else {
        return;
    };
    let results: Vec<_> = library::shared()
        .bookmarks()
        .into_iter()
        .map(|entry| entry.video)
        .filter(|video| video.avatar.is_empty() && !video.channel.is_empty())
        .collect();
    if results.is_empty() {
        return;
    }
    let weak = task.weak.clone();
    task.runtime.spawn(async move {
        let mut page = api::SearchPage {
            results,
            ..Default::default()
        };
        video_avatars(&client, &mut page).await;
        update(&weak, move |app| {
            let mut library = library::shared();
            for video in page
                .results
                .into_iter()
                .filter(|video| !video.avatar.is_empty())
            {
                library.remember(video);
            }
            let _ = library.save();
            drop(library);
            show_bookmarks(&app.global::<AppState>());
        });
    });
}
