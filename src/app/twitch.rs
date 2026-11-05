use super::*;
use crate::{
    TwitchResult,
    twitch::{self as api, Client, SearchMode, SearchResult, VideoKind},
};
use std::cell::RefCell;

/// How many pages one filter may walk through before it stops and says so.
const FILTER_PAGES: u32 = 20;

/// Everything loaded so far. The UI models hold the sorted or filtered view of it.
#[derive(Default)]
struct Lists {
    search: Vec<SearchResult>,
    user: Vec<SearchResult>,
    user_cursor: Option<String>,
    filter_pages: u32,
}

thread_local! {
    static LISTS: RefCell<Lists> = RefCell::default();
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
        LISTS.with_borrow_mut(|lists| lists.search.clear());
        ui.set_twitch_results(ModelRc::default());
        ui.set_twitch_cursor("".into());
        ui.set_twitch_results_query(query.as_str().into());
        ui.set_twitch_results_mode(ui.get_twitch_search_mode());
        run_search(&ui, &task, query, None);
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
        run_search(&ui, &task, query, Some(cursor));
    });
    let weak = app.as_weak();
    ui.on_twitch_sort_changed(move || {
        if let Some(app) = weak.upgrade() {
            show_search(&app.global::<AppState>());
        }
    });
    let weak = app.as_weak();
    ui.on_twitch_play(move |id, title, channel| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let ui = app.global::<AppState>();
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
        reload_user(&ui, &task);
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

fn run_search(ui: &AppState, task: &Task, query: String, cursor: Option<String>) {
    let Some(client) = task.client.clone() else {
        ui.set_twitch_error("Could not open the SlintYT config folder.".into());
        return;
    };
    let mode = if ui.get_twitch_results_mode() == 1 {
        SearchMode::User
    } else {
        SearchMode::Video
    };
    let epoch = task.generation.fetch_add(1, Ordering::Relaxed) + 1;
    ui.set_twitch_busy(true);
    ui.set_twitch_error("".into());
    let (weak, generation) = (task.weak.clone(), task.generation.clone());
    task.runtime.spawn(async move {
        let result = client.search(mode, &query, cursor.as_deref()).await;
        update(&weak, move |app| {
            if generation.load(Ordering::Relaxed) != epoch {
                return;
            }
            let ui = app.global::<AppState>();
            ui.set_twitch_busy(false);
            match result {
                Ok(page) => {
                    LISTS.with_borrow_mut(|lists| lists.search.extend(page.results));
                    ui.set_twitch_cursor(page.cursor.unwrap_or_default().into());
                    show_search(&ui);
                }
                Err(error) => ui.set_twitch_error(format!("{error:#}").into()),
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
        let result = client
            .user_videos(&login, kind, popular, cursor.as_deref())
            .await;
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
    let users = ui.get_twitch_results_mode() == 1;
    let rows: Vec<_> = LISTS.with_borrow(|lists| {
        api::sorted(&lists.search, users, ui.get_twitch_sort())
            .into_iter()
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
    TwitchResult {
        initials: initials(&result.title).into(),
        tone: tone(&result.title),
        id: result.id.into(),
        title: result.title.into(),
        subtitle: result.subtitle.into(),
        metadata: result.metadata.into(),
        duration: result.duration.into(),
        picture: result.picture.into(),
    }
}
