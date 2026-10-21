use crate::{
    AppState, AppWindow, BrowserProfile, VideoData,
    browser::{self, Selection},
    config::Store,
    feed::{FeedPage, PageCache, ThumbnailCache},
    youtube::{self, Session, Video, View},
};
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

mod actions;
mod avatars;
mod folders;
mod shorts;

enum Command {
    Start,
    Scan,
    ConnectIndex(usize, i32),
    ConnectSaved(usize),
    Connect(Selection),
    /// Try the last connection attempt again from the start-up splash.
    Retry,
    /// Leave the splash (cancelling any connection) and open the profile picker.
    ChangeBrowser,
    ProfileDetails {
        selection: Selection,
        account: crate::config::AccountProfile,
        origin: Option<Arc<Session>>,
    },
    PlaylistChanged {
        playlist: String,
        session: Arc<Session>,
    },
    Load {
        view: Option<View>,
        more: bool,
        epoch: u64,
    },
}

pub fn wire(app: &AppWindow) -> (tokio::runtime::Runtime, Arc<crate::player::Playback>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Could not start the background worker");
    let (sender, receiver) = mpsc::unbounded_channel();
    #[cfg(target_os = "macos")]
    crate::macos_tools::wire(app, runtime.handle().clone());
    let player = crate::player::install(app, runtime.handle().clone());
    let generation = Arc::new(AtomicU64::new(0));
    let session_slot = Arc::new(std::sync::RwLock::new(None));
    avatars::wire(app, runtime.handle().clone());
    folders::wire(app, runtime.handle().clone());
    actions::wire(
        app,
        runtime.handle().clone(),
        session_slot.clone(),
        sender.clone(),
    );
    let ui = app.global::<AppState>();
    let (send, weak) = (sender.clone(), app.as_weak());
    ui.on_switch_profile(move |index| {
        if index >= 0 && begin(&weak, "Connecting saved profile…") {
            if let Some(app) = weak.upgrade() {
                let state = app.global::<AppState>();
                state.set_account_menu_open(false);
                state.set_sync_menu_open(false);
                state.invoke_player_close();
            }
            let _ = send.send(Command::ConnectSaved(index as usize));
        }
    });
    ui.on_matches(|name, query| name.to_lowercase().contains(&query.to_lowercase()));
    ui.on_channel_count(|rows, query, subscribed_only| {
        let query = query.to_lowercase();
        rows.iter()
            .filter(|row| {
                (!subscribed_only || row.subscribed) && row.title.to_lowercase().contains(&query)
            })
            .count() as i32
    });
    let weak = app.as_weak();
    ui.on_reflow_feed(move |columns| {
        if let Some(app) = weak.upgrade() {
            reflow(&app, columns);
        }
    });
    let (send, weak, epoch) = (sender.clone(), app.as_weak(), generation.clone());
    ui.on_open_subscriptions(move |channels| {
        navigate(
            &weak,
            &send,
            &epoch,
            if channels {
                View::Channels
            } else {
                View::Subscriptions
            },
        )
    });
    let (send, weak, epoch) = (sender.clone(), app.as_weak(), generation.clone());
    ui.on_open_channel(move |id, tab, sort, query| {
        navigate(
            &weak,
            &send,
            &epoch,
            View::Channel {
                id: id.to_string(),
                tab: tab.clamp(0, 2),
                sort: sort.clamp(0, 2),
                query: query.trim().chars().take(500).collect(),
            },
        );
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    ui.on_retry_startup(move || {
        if !begin(&weak, "Retrying connection…") {
            return;
        }
        if let Some(app) = weak.upgrade() {
            app.global::<AppState>().set_startup_failed(false);
        }
        let _ = send.send(Command::Retry);
    });
    // Connecting runs on the worker, so cancel it directly before queuing the picker.
    let (cancel_connect, cancel) = tokio::sync::watch::channel(0u64);
    let (send, weak, epoch) = (sender.clone(), app.as_weak(), generation.clone());
    ui.on_change_browser(move || {
        cancel_connect.send_modify(|n| *n += 1);
        epoch.fetch_add(1, Ordering::Relaxed);
        if let Some(app) = weak.upgrade() {
            let state = app.global::<AppState>();
            state.invoke_player_close();
            state.set_initializing(false);
            state.set_startup_failed(false);
            state.set_account_menu_open(false);
            state.set_sync_menu_open(false);
            state.set_settings_open(false);
            state.set_setup_open(true);
            state.set_busy(true);
            state.set_error("".into());
            state.set_manual_profile_path("".into());
        }
        let _ = send.send(Command::ChangeBrowser);
    });
    let (weak, handle) = (app.as_weak(), runtime.handle().clone());
    ui.on_browse_profile_folder(move || {
        let Some(app) = weak.upgrade() else { return };
        let state = app.global::<AppState>();
        if state.get_busy() || state.get_profile_dialog_open() {
            return;
        }
        state.set_profile_dialog_open(true);
        // Create on the UI thread for macOS, await off-thread on every platform.
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose your browser profile folder")
            .pick_folder();
        let weak = weak.clone();
        handle.spawn(async move {
            let folder = dialog.await;
            update(&weak, move |app| {
                let state = app.global::<AppState>();
                state.set_profile_dialog_open(false);
                if state.get_setup_open()
                    && !state.get_initializing()
                    && let Some(folder) = folder
                {
                    state.set_manual_profile_path(folder.path().display().to_string().into());
                    state.set_selected_profile(-1);
                    state.set_error("".into());
                }
            });
        });
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    ui.on_scan_profiles(move || {
        if begin(&weak, "Looking for browser profiles…") {
            let _ = send.send(Command::Scan);
        }
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    ui.on_connect_profile(move |index, keyring| {
        if index >= 0 && begin(&weak, "Reading cookies and checking your YouTube sign-in…") {
            let _ = send.send(Command::ConnectIndex(index as usize, keyring));
        }
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    ui.on_connect_manual(move |browser, profile, keyring| {
        let profile = match browser::profile_path(&profile) {
            Ok(path) => path,
            Err(error) => {
                if let Some(app) = weak.upgrade() {
                    app.global::<AppState>().set_error(error.to_string().into());
                }
                return;
            }
        };
        if begin(&weak, "Reading cookies and checking your YouTube sign-in…") {
            // The picker only asks for a folder; guess its browser when none is given.
            let browser = match browser.as_str() {
                "" => browser::infer(&profile).to_owned(),
                browser => browser.to_owned(),
            };
            let _ = send.send(Command::Connect(Selection {
                browser,
                profile,
                keyring: keyring_value(keyring),
            }));
        }
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    let epoch = generation.clone();
    ui.on_load_feed(move |history| {
        navigate(
            &weak,
            &send,
            &epoch,
            if history { View::History } else { View::Home },
        )
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    let epoch = generation.clone();
    ui.on_search(move |query| {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        if query.chars().count() > 500 {
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>()
                    .set_error("Keep your search under 500 characters.".into());
            }
            return;
        }
        navigate(&weak, &send, &epoch, View::Search(query.to_owned()));
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    let epoch = generation.clone();
    ui.on_open_playlists(move || navigate(&weak, &send, &epoch, View::Playlists));
    let (send, weak) = (sender.clone(), app.as_weak());
    let epoch = generation.clone();
    ui.on_open_playlist(move |input| match youtube::playlist_id(input.as_str()) {
        Ok(id) => navigate(&weak, &send, &epoch, View::Playlist(id)),
        Err(error) => {
            if let Some(app) = weak.upgrade() {
                app.global::<AppState>().set_error(error.to_string().into());
            }
        }
    });
    let (send, weak) = (sender.clone(), app.as_weak());
    let epoch = generation.clone();
    ui.on_reload(move |more| {
        if begin(&weak, "Loading from YouTube…") {
            let _ = send.send(Command::Load {
                view: None,
                more,
                epoch: epoch.fetch_add(1, Ordering::Relaxed) + 1,
            });
        }
    });
    ui.set_busy(true);
    ui.set_status("Looking for your saved browser…".into());
    runtime.spawn(worker(
        receiver,
        sender.clone(),
        app.as_weak(),
        generation,
        player.clone(),
        session_slot,
        cancel,
    ));
    let _ = sender.send(Command::Start);
    (runtime, player)
}

fn prepare(app: &AppWindow, view: &View) {
    let state = app.global::<AppState>();
    if state.get_player_open() {
        state.invoke_player_close();
    }
    state.set_settings_open(false);
    state.set_scroll_reset(state.get_scroll_reset() + 1);
    state.set_sections(ModelRc::default());
    state.set_channel_filter("".into());
    if let View::Playlist(id) = view {
        state.set_current_playlist(id.into());
    } else {
        state.set_current_playlist("".into());
    }
    if let View::Channel {
        id,
        tab,
        sort,
        query,
    } = view
    {
        if state.get_channel_id() != id {
            state.set_channel_name("".into());
            state.set_channel_banner(slint::Image::default());
            state.set_channel_avatar("".into());
        }
        state.set_channel_id(id.into());
        state.set_channel_tab(*tab);
        state.set_channel_sort(*sort);
        state.set_channel_query(query.into());
    }
    state.set_view_kind(view.kind());
    state.set_page_title(view.title().into());
    state.set_page_subtitle(view.subtitle().into());
    state.set_videos(ModelRc::default());
    state.set_has_more(false);
    state.set_refreshing(false);
}

fn navigate(
    weak: &slint::Weak<AppWindow>,
    sender: &mpsc::UnboundedSender<Command>,
    generation: &Arc<AtomicU64>,
    view: View,
) {
    if let Some(app) = weak.upgrade()
        && app.global::<AppState>().get_connected()
    {
        app.global::<AppState>().set_busy(false);
    }
    if begin(weak, "Loading from YouTube…") {
        if let Some(app) = weak.upgrade() {
            prepare(&app, &view);
        }
        let _ = sender.send(Command::Load {
            view: Some(view),
            more: false,
            epoch: generation.fetch_add(1, Ordering::Relaxed) + 1,
        });
    }
}

fn begin(weak: &slint::Weak<AppWindow>, status: &str) -> bool {
    let Some(app) = weak.upgrade() else {
        return false;
    };
    let state = app.global::<AppState>();
    if state.get_busy() {
        return false;
    }
    state.set_busy(true);
    state.set_error("".into());
    state.set_status(status.into());
    true
}

fn keyring_value(index: i32) -> Option<String> {
    match index {
        1 => Some("GNOMEKEYRING"),
        2 => Some("KWALLET6"),
        3 => Some("KWALLET5"),
        4 => Some("KWALLET"),
        5 => Some("BASICTEXT"),
        _ => None,
    }
    .map(str::to_owned)
}

fn update(weak: &slint::Weak<AppWindow>, f: impl FnOnce(AppWindow) + Send + 'static) {
    let _ = weak.upgrade_in_event_loop(f);
}

// A cancelled connection can already have UI updates queued on the event loop.
// Keep its receiver version so those updates cannot reopen the splash or feed.
fn update_startup(
    weak: &slint::Weak<AppWindow>,
    cancel: &watch::Receiver<u64>,
    f: impl FnOnce(AppWindow) + Send + 'static,
) {
    let cancel = cancel.clone();
    update(weak, move |app| {
        if !cancel.has_changed().unwrap_or(true) {
            f(app);
        }
    });
}

fn publish_profiles(weak: &slint::Weak<AppWindow>, discovery: browser::Discovery) {
    let current = Store::new().and_then(|s| s.load()).ok().flatten();
    let (rows, selected) = picker_rows(&discovery.profiles, current.as_ref());
    let temporary = rows.iter().filter(|r| r.temporary).count() as i32;
    update(weak, move |app| {
        let state = app.global::<AppState>();
        state.set_profiles(ModelRc::new(VecModel::from(rows)));
        state.set_selected_profile(selected);
        state.set_temporary_profiles(temporary);
        state.set_scan_warning(discovery.warning.into());
        state.set_status("".into());
        state.set_busy(false);
        state.set_initializing(false);
    });
}

/// Display group for a discovered profile: browser name and its data folder.
/// Temporary automation profiles share one group.
fn profile_group(profile: &Selection) -> (String, std::path::PathBuf) {
    if profile.temporary() {
        return ("Temporary profiles".into(), std::path::PathBuf::new());
    }
    let label = profile.label();
    let name = label.split_once(" · ").map_or(label.as_str(), |(n, _)| n);
    (name.to_owned(), profile.root())
}

/// Orders profiles group by group, keeping discovery order inside each group and
/// moving temporary profiles last. Connecting uses indices into this order.
fn group_profiles(profiles: &mut [Selection]) {
    let mut seen: Vec<(String, std::path::PathBuf)> = Vec::new();
    for profile in profiles.iter() {
        let group = profile_group(profile);
        if !seen.contains(&group) {
            seen.push(group);
        }
    }
    profiles.sort_by_key(|p| {
        let group = profile_group(p);
        (p.temporary(), seen.iter().position(|g| *g == group))
    });
}

/// Picker rows for already grouped profiles, plus the row in use (or -1).
fn picker_rows(profiles: &[Selection], current: Option<&Selection>) -> (Vec<BrowserProfile>, i32) {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    let tidy = |path: &std::path::Path| match home.as_ref().and_then(|h| path.strip_prefix(h).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let mut previous = None;
    let rows = profiles
        .iter()
        .map(|profile| {
            let group = profile_group(profile);
            let first = previous.as_ref() != Some(&group);
            let temporary = profile.temporary();
            let file = profile
                .profile
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            // Automation tools nest a Default profile inside a uniquely named folder.
            let name = match profile.profile.parent().and_then(|p| p.file_name()) {
                Some(parent) if temporary && file == "Default" => {
                    parent.to_string_lossy().into_owned()
                }
                _ => file.into_owned(),
            };
            let row = BrowserProfile {
                label: profile.label().into(),
                path: profile.profile.display().to_string().into(),
                name: name.into(),
                root: if temporary {
                    "made by automation and test tools".into()
                } else {
                    tidy(&group.1).into()
                },
                initials: initials(&group.0).into(),
                tone: tone(&group.0),
                group: group.0.as_str().into(),
                first,
                temporary,
                ..Default::default()
            };
            previous = Some(group);
            row
        })
        .collect();
    let selected = current
        .and_then(|c| {
            profiles
                .iter()
                .position(|p| p.browser == c.browser && p.profile == c.profile)
        })
        .map_or(-1, |i| i as i32);
    (rows, selected)
}

fn apply_sync_status(app: &AppWindow, status: crate::watch::SyncStatus) {
    let state = app.global::<AppState>();
    state.set_sync_running(status.running);
    state.set_sync_complete(status.complete);
    state.set_sync_failed(status.failed);
    state.set_sync_count(status.count.to_string().into());
    state.set_sync_title(status.title.into());
    state.set_sync_detail(status.detail.into());
    state.set_sync_warning(status.warning.into());
}

fn publish_watch(weak: &slint::Weak<AppWindow>, session: &Session, videos: &mut [Video]) {
    session.annotate(videos);
    let flags: Vec<_> = videos
        .iter()
        .map(|v| {
            (
                v.id.clone(),
                v.playlist,
                v.watched,
                v.watch_progress.map(i32::from).unwrap_or(-1),
            )
        })
        .collect();
    let note = session.watch_note();
    let sync = session.sync_status();
    update(weak, move |app| {
        apply_sync_status(&app, sync);
        let state = app.global::<AppState>();
        state.set_watch_note(note.into());
        let model = state.get_videos();
        for (index, (id, playlist, watched, progress)) in flags.into_iter().enumerate() {
            if let Some(mut row) = model.row_data(index)
                && row.id == id
                && row.playlist == playlist
                && (row.watched != watched || row.watch_progress != progress)
            {
                row.watched = watched;
                row.watch_progress = progress;
                model.set_row_data(index, row);
            }
        }
    });
}

fn publish_saved_profiles(
    weak: &slint::Weak<AppWindow>,
    settings: &crate::config::Settings,
) -> Vec<Selection> {
    let profiles = settings
        .profiles
        .iter()
        .map(|p| p.selection.clone())
        .collect::<Vec<_>>();
    let active = profiles
        .iter()
        .position(|p| Some(p) == settings.current_profile.as_ref())
        .map_or(-1, |i| i as i32);
    let rows = settings
        .profiles
        .iter()
        .map(|p| BrowserProfile {
            label: p.selection.label().into(),
            path: p.selection.profile.to_string_lossy().into_owned().into(),
            name: if p.youtube.name.is_empty() {
                "YouTube profile".into()
            } else {
                p.youtube.name.as_str().into()
            },
            initials: initials(&p.youtube.name).into(),
            avatar: p.youtube.avatar.as_str().into(),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    update(weak, move |app| {
        let ui = app.global::<AppState>();
        if let Some(current) = rows.get(active as usize) {
            ui.set_account_name(current.name.clone());
            ui.set_account_initial(current.initials.clone());
            ui.set_account_avatar(current.avatar.clone());
            ui.set_account_label(current.label.clone());
        }
        ui.set_saved_profiles(ModelRc::new(VecModel::from(rows)));
        ui.set_active_profile(active);
    });
    profiles
}

async fn worker(
    mut receiver: mpsc::UnboundedReceiver<Command>,
    sender: mpsc::UnboundedSender<Command>,
    weak: slint::Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    player: Arc<crate::player::Playback>,
    session_slot: actions::SessionSlot,
    mut cancel: tokio::sync::watch::Receiver<u64>,
) {
    let mut profiles = Vec::<Selection>::new();
    let mut saved_profiles = Vec::<Selection>::new();
    let mut last_attempt: Option<Selection> = None;
    let mut session: Option<Arc<Session>> = None;
    let mut current_view = View::Home;
    let mut feed = FeedPage::default();
    let mut pages = PageCache::default();
    let images = ThumbnailCache::default();
    type LoadResult = (u64, View, Result<FeedPage, String>);
    let mut active: Option<tokio::task::JoinHandle<LoadResult>> = None;
    let mut initial_feed = true;
    let mut active_epoch = 0;
    let mut view_epoch = 0;
    let (idle_updates, mut watch_changes) = watch::channel(0_u64);
    loop {
        let command = tokio::select! {
            next = receiver.recv() => { let Some(command) = next else { break; }; command },
            result = async {
                match active.as_mut() { Some(task) => task.await, None => std::future::pending().await }
            } => {
                active = None;
                if result.is_err() { fail_page(&weak, "YouTube request could not finish. Please retry.".into(), generation.clone(), active_epoch); continue; }
                if let Ok((epoch, view, result)) = result {
                    if generation.load(Ordering::Relaxed) != epoch { continue; }
                    match result {
                        Ok(page) => {
                            initial_feed = false;
                            let shown = feed.videos.len();
                            feed = page;
                            if let Some(session) = session.as_ref() { session.annotate(&mut feed.videos); }
                            pages.put(&view, &feed);
                            let note = session.as_ref().map(|s| s.watch_note()).unwrap_or_default();
                            publish_feed(&weak, &feed, &images, &generation, epoch, (false, false), note);
                            // Rows added by scrolling are on screen, so fetch them before the rest.
                            let mut order = feed.videos.clone().into_iter().enumerate().collect::<Vec<_>>();
                            if shown < order.len() { order.rotate_left(shown); }
                            thumbnails(&weak, order, generation.clone(), epoch, images.clone());
                            if let Some(session) = session.as_ref() { shorts::lookup(&weak, session.clone(), &feed.videos); }
                            if let Some(session) = session.as_ref() { session.cache_feed(&view, &feed); }
                        }
                        Err(message) => fail_page(&weak, message, generation.clone(), epoch),
                    }
                }
                continue;
            },
            result = watch_changes.changed() => {
                if let Some(session) = session.as_ref() { publish_watch(&weak, session, &mut feed.videos); }
                if result.is_err() { watch_changes = idle_updates.subscribe(); }
                continue;
            }
        };
        let mut connect = None;
        let mut load = None;
        let mut scan = false;
        let mut refresh_history = false;
        let mut reload_pages = None;
        match command {
            Command::Start => match Store::new().and_then(|store| {
                let settings = store.load_settings()?;
                saved_profiles = publish_saved_profiles(&weak, &settings);
                let missing = settings
                    .profiles
                    .iter()
                    .filter(|p| {
                        p.youtube.name.is_empty()
                            && Some(&p.selection) != settings.current_profile.as_ref()
                    })
                    .map(|p| p.selection.clone())
                    .collect::<Vec<_>>();
                let sender = sender.clone();
                tokio::spawn(async move {
                    for selection in missing {
                        if let Ok(Some(account)) = tokio::time::timeout(
                            Duration::from_secs(30),
                            actions::saved_profile_account(&selection),
                        )
                        .await
                        {
                            let _ = sender.send(Command::ProfileDetails {
                                selection,
                                account,
                                origin: None,
                            });
                        }
                    }
                });
                store.load()
            }) {
                Ok(Some(selection)) => connect = Some(selection),
                Ok(None) => scan = true,
                Err(error) => {
                    let message = error.to_string();
                    update(&weak, move |app| {
                        app.global::<AppState>().set_error(message.into())
                    });
                    scan = true;
                }
            },
            Command::ProfileDetails {
                selection,
                account,
                origin,
            } => {
                if let Ok(store) = Store::new()
                    && let Ok(settings) = store.load_settings()
                {
                    // A delayed request must not replace a newly connected account's identity.
                    let stale = match origin {
                        Some(origin) => !session.as_ref().is_some_and(|s| Arc::ptr_eq(s, &origin)),
                        None => settings.current_profile.as_ref() == Some(&selection),
                    };
                    if !stale
                        && store.save_account(&selection, account).is_ok()
                        && let Ok(settings) = store.load_settings()
                    {
                        saved_profiles = publish_saved_profiles(&weak, &settings);
                    }
                }
                continue;
            }
            Command::Scan => scan = true,
            Command::Retry => match last_attempt.clone() {
                Some(selection) => connect = Some(selection),
                None => scan = true,
            },
            Command::ChangeBrowser => {
                cancel.borrow_and_update();
                generation.fetch_add(1, Ordering::Relaxed);
                if let Some(task) = active.take() {
                    task.abort();
                }
                update(&weak, |app| {
                    let state = app.global::<AppState>();
                    state.set_initializing(false);
                    state.set_startup_failed(false);
                    state.set_error("".into());
                    state.set_settings_open(false);
                    state.set_setup_open(true);
                    state.set_busy(true);
                    state.set_status("Looking for browser profiles…".into());
                });
                scan = true;
            }
            Command::ConnectSaved(index) => connect = saved_profiles.get(index).cloned(),
            Command::ConnectIndex(index, keyring) => {
                connect = profiles.get(index).cloned().map(|mut p| {
                    p.keyring = keyring_value(keyring);
                    p
                })
            }
            Command::Connect(selection) => connect = Some(selection),
            Command::PlaylistChanged {
                playlist,
                session: origin,
            } => {
                if !session.as_ref().is_some_and(|s| Arc::ptr_eq(s, &origin)) {
                    continue;
                }
                if let Some(cache) = &origin.cache {
                    cache.invalidate("pages-v1").await;
                }
                reload_pages = pages.invalidate_playlist(&playlist, &current_view, &mut feed);
                let Some(_) = reload_pages else { continue };
                // Navigation may already have changed on the event loop while
                // its Load command is still queued. Never supersede that command.
                let Ok(previous) = generation.compare_exchange(
                    view_epoch,
                    view_epoch + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) else {
                    continue;
                };
                view_epoch = previous + 1;
                load = Some((false, view_epoch));
            }
            Command::Load { view, more, epoch } => {
                if generation.load(Ordering::Relaxed) != epoch {
                    continue;
                }
                refresh_history = view.is_none() && !more;
                view_epoch = epoch;
                if let Some(view) = view {
                    current_view = view;
                    feed = match pages.get(&current_view) {
                        Some(page) => page,
                        None => match &session {
                            Some(session) => {
                                session.cached_feed(&current_view).await.unwrap_or_default()
                            }
                            None => FeedPage::default(),
                        },
                    };
                }
                load = Some((more, epoch));
            }
        }
        if scan || connect.is_some() {
            if let Some(task) = active.take() {
                task.abort();
            }
            generation.fetch_add(1, Ordering::Relaxed);
        }
        if scan {
            match tokio::task::spawn_blocking(browser::discover).await {
                Ok(Ok(mut found)) => {
                    group_profiles(&mut found.profiles);
                    profiles = found.profiles.clone();
                    publish_profiles(&weak, found);
                }
                result => {
                    let message = match result {
                        Ok(Err(e)) => e.to_string(),
                        _ => "Browser discovery could not finish. Enter a profile manually.".into(),
                    };
                    fail(&weak, message);
                }
            }
            continue;
        }
        if let Some(selection) = connect {
            last_attempt = Some(selection.clone());
            let label = selection.label();
            update_startup(&weak, &cancel, move |app| {
                let state = app.global::<AppState>();
                let initial = label.chars().next().map(|c| c.to_uppercase().to_string());
                state.set_startup_initial(initial.unwrap_or_default().into());
                state.set_startup_label(label.into());
                state.set_startup_step(0);
                state.set_startup_failed(false);
                state.set_error("".into());
                state.set_initializing(true);
            });
            let step = weak.clone();
            let step_cancel = cancel.clone();
            let attempt = tokio::time::timeout(
                Duration::from_secs(120),
                Session::connect(&selection, move || {
                    update_startup(&step, &step_cancel, |app| {
                        app.global::<AppState>().set_startup_step(1)
                    });
                }),
            );
            let mut cancelled = cancel.clone();
            let result = tokio::select! {
                biased;
                _ = cancelled.changed() => None,
                result = attempt => Some(result),
            };
            // Cancelled from the splash: the queued ChangeBrowser command follows.
            let Some(result) = result else { continue };
            match result {
                Ok(Ok(new_session)) => {
                    if let Err(error) = Store::new().and_then(|s| s.save(&selection)) {
                        fail(
                            &weak,
                            format!(
                                "The profile connected, but its setting could not be saved: {error}"
                            ),
                        );
                        continue;
                    }
                    if let Ok(settings) = Store::new().and_then(|s| s.load_settings()) {
                        saved_profiles = publish_saved_profiles(&weak, &settings);
                    }
                    let epoch = generation.fetch_add(1, Ordering::Relaxed) + 1;
                    watch_changes = new_session.watch_updates();
                    let note = new_session.watch_note();
                    let sync = new_session.sync_status();
                    player.set_session(&new_session);
                    session = Some(Arc::new(new_session));
                    *session_slot.write().unwrap() = session.clone();
                    if let Some(session) = session.clone() {
                        let send = sender.clone();
                        let selection = selection.clone();
                        tokio::spawn(async move {
                            if let Some(account) = actions::account_profile(&session).await {
                                let _ = send.send(Command::ProfileDetails {
                                    selection,
                                    account,
                                    origin: Some(session),
                                });
                            }
                        });
                    }
                    pages.clear();
                    images.clear();
                    let label = selection.label();
                    update_startup(&weak, &cancel, move |app| {
                        apply_sync_status(&app, sync);
                        prepare(&app, &View::Home);
                        let state = app.global::<AppState>();
                        state.set_comments(ModelRc::default());
                        state.set_recommendations(ModelRc::default());
                        state.set_save_playlists(ModelRc::default());
                        let (browser, profile) =
                            label.split_once(" · ").unwrap_or((label.as_str(), ""));
                        state.set_account_browser(browser.into());
                        state.set_account_profile(profile.into());
                        state.set_account_label(label.clone().into());
                        state.set_connected(true);
                        state.set_setup_open(false);
                        state.set_error("".into());
                        state.set_watch_note(note.into());
                        state.set_startup_step(2);
                        state.set_status("Loading recommendations…".into());
                    });
                    current_view = View::Home;
                    feed = session
                        .as_ref()
                        .unwrap()
                        .cached_feed(&current_view)
                        .await
                        .unwrap_or_default();
                    initial_feed = feed.pages == 0;
                    load = Some((false, epoch));
                    view_epoch = epoch;
                }
                result => {
                    let message = match result { Ok(Err(e)) => e.to_string(), _ => "Connecting timed out. Unlock your keyring and check your internet connection, then retry.".into() };
                    fail(&weak, message);
                    continue;
                }
            }
        }
        if let Some((more, epoch)) = load {
            let Some(session) = session.as_ref() else {
                fail(&weak, "Choose a browser profile first.".into());
                continue;
            };
            if refresh_history {
                session.refresh_watch_history();
            }
            if let Some(task) = active.take() {
                task.abort();
            }
            let append = more && feed.continuation.is_some();
            let starting_page = if append {
                feed.clone()
            } else {
                FeedPage::default()
            };
            let count = reload_pages.unwrap_or_else(|| if append { 1 } else { feed.pages.max(1) });
            session.annotate(&mut feed.videos);
            let foreground = feed.pages == 0 || append;
            publish_feed(
                &weak,
                &feed,
                &images,
                &generation,
                epoch,
                (foreground, !foreground),
                session.watch_note(),
            );
            thumbnails(
                &weak,
                feed.videos.clone().into_iter().enumerate().collect(),
                generation.clone(),
                epoch,
                images.clone(),
            );
            let (session, view, warm_images) =
                (session.clone(), current_view.clone(), images.clone());
            let warm = initial_feed;
            active_epoch = epoch;
            active = Some(tokio::spawn(async move {
                let result = fetch_pages(&session, &view, starting_page, count).await;
                if warm && let Ok(feed) = &result {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(3),
                        warm_thumbnails(&feed.videos, warm_images),
                    )
                    .await;
                }
                (epoch, view, result)
            }));
        } else {
            fail(
                &weak,
                "That browser profile is no longer available. Scan again.".into(),
            );
        }
    }
    if let Some(task) = active {
        task.abort();
    }
}

async fn fetch_pages(
    session: &Session,
    view: &View,
    mut feed: FeedPage,
    count: usize,
) -> Result<FeedPage, String> {
    for _ in 0..count {
        let token = feed.continuation.clone();
        let page = match tokio::time::timeout(
            Duration::from_secs(60),
            session.page(view, token.as_deref(), feed.visitor.as_deref()),
        )
        .await
        {
            Ok(Ok(page)) => page,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("YouTube took too long to respond. Please retry.".into()),
        };
        feed.accept(view, token.as_deref(), page)
            .map_err(|e| e.to_string())?;
        if feed.continuation.is_none() {
            break;
        }
    }
    Ok(feed)
}

fn publish_feed(
    weak: &slint::Weak<AppWindow>,
    feed: &FeedPage,
    images: &ThumbnailCache,
    generation: &Arc<AtomicU64>,
    epoch: u64,
    activity: (bool, bool),
    note: String,
) {
    let (busy, refreshing) = activity;
    let display: Vec<_> = feed
        .videos
        .iter()
        .map(|video| (video.clone(), images.get(&video.thumbnail)))
        .collect();
    let has_more = feed.continuation.is_some();
    let title = feed.title.clone();
    let channel = feed.channel_info.clone();
    if let Some(info) = &channel {
        let (url, id, weak) = (info.banner.clone(), info.id.clone(), weak.clone());
        tokio::spawn(async move {
            if let Ok(pixels) = youtube::download_thumbnail(&reqwest::Client::new(), &url).await {
                update(&weak, move |app| {
                    if app.global::<AppState>().get_channel_id() == id {
                        app.global::<AppState>()
                            .set_channel_banner(thumbnail_image(&pixels));
                    }
                });
            }
        });
    }
    let generation = generation.clone();
    update(weak, move |app| {
        if generation.load(Ordering::Relaxed) != epoch {
            return;
        }
        let state = app.global::<AppState>();
        let old: std::collections::HashMap<_, _> = state
            .get_videos()
            .iter()
            .map(|v| ((v.id.to_string(), v.playlist), v.thumbnail))
            .collect();
        state.set_videos(ModelRc::new(VecModel::from(
            display
                .into_iter()
                .map(|(video, pixels)| {
                    let previous = old.get(&(video.id.clone(), video.playlist)).cloned();
                    let mut row = video_data(video);
                    row.thumbnail = pixels
                        .map(|p| thumbnail_image(&p))
                        .or(previous)
                        .unwrap_or_default();
                    row
                })
                .collect::<Vec<_>>(),
        )));
        reflow(&app, state.get_feed_columns());
        if let Some(channel) = channel {
            state.set_channel_name(channel.name.clone().into());
            state.set_channel_initials(initials(&channel.name).into());
            state.set_channel_tone(tone(&channel.name));
            state.set_channel_avatar(avatars::channel_avatar(&channel.id, channel.avatar).into());
            state.set_channel_meta(channel.metadata.into());
            state.set_channel_description(channel.description.into());
            state.set_channel_has_live(channel.has_live);
            state.set_channel_subscribed(channel.subscribed);
            state.set_channel_links(ModelRc::new(VecModel::from(
                channel
                    .links
                    .into_iter()
                    .take(4)
                    .map(|(title, url)| crate::LinkData {
                        title: title.into(),
                        url: url.into(),
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        state.set_has_more(has_more);
        state.set_busy(busy);
        if !busy {
            state.set_initializing(false);
        }
        state.set_refreshing(refreshing);
        state.set_status(if refreshing {
            "Refreshing…".into()
        } else if busy {
            "Loading from YouTube…".into()
        } else {
            "".into()
        });
        state.set_watch_note(note.into());
        if !title.is_empty() {
            state.set_page_title(title.into());
        }
    });
}

fn fail_page(
    weak: &slint::Weak<AppWindow>,
    message: String,
    generation: Arc<AtomicU64>,
    epoch: u64,
) {
    update(weak, move |app| {
        if generation.load(Ordering::Relaxed) != epoch {
            return;
        }
        let state = app.global::<AppState>();
        state.set_error(message.into());
        // During start-up the splash shows the failure with retry options.
        state.set_startup_failed(state.get_initializing());
        state.set_busy(false);
        state.set_refreshing(false);
        state.set_status("".into());
    });
}

fn fail(weak: &slint::Weak<AppWindow>, message: String) {
    update(weak, move |app| {
        let state = app.global::<AppState>();
        state.set_error(message.into());
        state.set_startup_failed(state.get_initializing());
        state.set_refreshing(false);
        state.set_status("".into());
        state.set_busy(false);
    });
}

pub(crate) fn video_data(mut video: Video) -> VideoData {
    shorts::apply(&mut video);
    let identity = if video.channel.is_empty() {
        &video.title
    } else {
        &video.channel
    };
    let initials = initials(identity);
    let tone = tone(identity);
    VideoData {
        id: video.id.into(),
        title: video.title.into(),
        channel: video.channel.into(),
        metadata: video.metadata.into(),
        duration: video.duration.into(),
        thumbnail: slint::Image::default(),
        playlist: video.playlist,
        watched: video.watched,
        watch_progress: video.watch_progress.map(i32::from).unwrap_or(-1),
        avatar: avatars::channel_avatar(&video.channel_id, video.avatar).into(),
        channel_id: video.channel_id.into(),
        initials: initials.into(),
        tone,
        group: video.group.into(),
        set_id: video.set_id.into(),
        subscribed: video.subscribed,
    }
}

fn thumbnails(
    weak: &slint::Weak<AppWindow>,
    videos: Vec<(usize, Video)>,
    generation: Arc<AtomicU64>,
    epoch: u64,
    images: ThumbnailCache,
) {
    let weak = weak.clone();
    tokio::spawn(async move {
        let Ok(http) = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return;
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(6));
        let mut jobs = tokio::task::JoinSet::new();
        for (index, video) in videos {
            if generation.load(Ordering::Relaxed) != epoch {
                break;
            }
            let permit = semaphore.clone().acquire_owned().await;
            let Ok(permit) = permit else {
                break;
            };
            let (http, weak, generation, images) = (
                http.clone(),
                weak.clone(),
                generation.clone(),
                images.clone(),
            );
            jobs.spawn(async move {
                let _permit = permit;
                if generation.load(Ordering::Relaxed) != epoch {
                    return;
                }
                let pixels = if let Some(pixels) = images.get(&video.thumbnail) {
                    Some(pixels)
                } else {
                    youtube::download_thumbnail(&http, &video.thumbnail)
                        .await
                        .ok()
                        .map(|pixels| {
                            let pixels = Arc::new(pixels);
                            images.put(video.thumbnail.clone(), pixels.clone());
                            pixels
                        })
                };
                if let Some(pixels) = pixels {
                    update(&weak, move |app| {
                        if generation.load(Ordering::Relaxed) != epoch {
                            return;
                        }
                        let model = app.global::<AppState>().get_videos();
                        if let Some(mut row) = model.row_data(index) {
                            if row.id != video.id {
                                return;
                            }
                            row.thumbnail = thumbnail_image(&pixels);
                            model.set_row_data(index, row);
                        }
                    });
                }
            });
        }
        while jobs.join_next().await.is_some() {}
    });
}

pub(crate) fn thumbnail_image(pixels: &youtube::Pixels) -> slint::Image {
    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &pixels.bytes,
        pixels.width,
        pixels.height,
    );
    slint::Image::from_rgba8(pixels)
}

async fn warm_thumbnails(videos: &[Video], images: ThumbnailCache) {
    let Ok(http) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return;
    };
    let mut jobs = tokio::task::JoinSet::new();
    for video in videos.iter().take(6) {
        let (http, images, url) = (http.clone(), images.clone(), video.thumbnail.clone());
        jobs.spawn(async move {
            if images.get(&url).is_none()
                && let Ok(pixels) = youtube::download_thumbnail(&http, &url).await
            {
                images.put(url, Arc::new(pixels));
            }
        });
    }
    while jobs.join_next().await.is_some() {}
}

pub(crate) fn initials(name: &str) -> String {
    let result: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|s| s.chars().find(|c| c.is_alphanumeric()))
        .flat_map(char::to_uppercase)
        .collect();
    if result.is_empty() {
        "?".into()
    } else {
        result
    }
}
pub(crate) fn tone(name: &str) -> slint::Color {
    let palette = [
        0x3b4a63, 0x5e4a35, 0x3e5a4c, 0x4a4f2e, 0x4a3a3a, 0x5a3f5e, 0x2e414e,
    ];
    let hash = name
        .bytes()
        .fold(0usize, |h, c| h.wrapping_mul(31).wrapping_add(c as usize));
    slint::Color::from_rgb_u8(
        (palette[hash % palette.len()] >> 16) as u8,
        (palette[hash % palette.len()] >> 8) as u8,
        palette[hash % palette.len()] as u8,
    )
}

pub(crate) fn reflow(app: &AppWindow, columns: i32) {
    let state = app.global::<AppState>();
    let model = state.get_videos();
    let mut rows = Vec::new();
    let columns = columns.clamp(1, 6) as usize;
    let grouped = matches!(state.get_view_kind(), 1 | 5);
    let mut index = 0;
    while index < model.row_count() {
        let group = model
            .row_data(index)
            .map(|v| v.group.to_string())
            .unwrap_or_default();
        let mut end = index + 1;
        while end < model.row_count()
            && (!grouped || model.row_data(end).is_some_and(|v| v.group == group))
        {
            end += 1;
        }
        let start = index;
        let detail = match end - start {
            1 => "1 video".to_string(),
            n => format!("{n} videos"),
        };
        while index < end {
            let count = (end - index).min(columns);
            let first = grouped && index == start;
            rows.push(crate::FeedSection {
                label: if first {
                    group.clone().into()
                } else {
                    "".into()
                },
                detail: if first {
                    detail.clone().into()
                } else {
                    "".into()
                },
                start: index as i32,
                count: count as i32,
            });
            index += count;
        }
    }
    state.set_sections(ModelRc::new(VecModel::from(rows)));
}
