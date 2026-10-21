use super::*;
use crate::bookmarks::{DestinationMode, Sort};
use std::{cell::RefCell, collections::HashSet, rc::Rc, sync::Mutex};

pub(super) fn wire(
    app: &AppWindow,
    runtime: tokio::runtime::Handle,
    slot: actions::SessionSlot,
    sender: mpsc::UnboundedSender<Command>,
    epoch: Arc<AtomicU64>,
) {
    let ui = app.global::<AppState>();
    ui.set_bookmarks_enabled(
        Store::new()
            .and_then(|s| s.load_settings())
            .map(|s| s.bookmarks_enabled)
            .unwrap_or(false),
    );
    let sessions = slot.clone();
    ui.on_bookmark_state(move |id, _| {
        sessions
            .read()
            .unwrap()
            .as_ref()
            .and_then(|s| s.bookmarks.as_ref())
            .map(|b| b.membership(id.as_str()))
            .unwrap_or(0)
    });
    let (weak, send, generation) = (app.as_weak(), sender.clone(), epoch);
    ui.on_open_bookmarks(move || {
        if !weak
            .upgrade()
            .is_some_and(|a| a.global::<AppState>().get_bookmarks_enabled())
        {
            return;
        }
        navigate(&weak, &send, &generation, View::Bookmarks);
    });
    let (weak, sessions, rt) = (app.as_weak(), slot.clone(), runtime.clone());
    ui.on_set_bookmarks_enabled(move |enabled| {
        let result = Store::new().and_then(|s| s.save_bookmarks_enabled(enabled));
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            if let Err(e) = result {
                ui.set_bookmarks_error(format!("Could not save bookmark settings: {e}").into());
                return;
            }
            ui.set_bookmarks_enabled(enabled);
            if !enabled && ui.get_view_kind() == 9 {
                // Leave the Settings page open when disabling from there.
                let settings = ui.get_settings_open();
                ui.invoke_load_feed(false);
                ui.set_settings_open(settings);
            }
        }
        if let Some(session) = sessions.read().unwrap().clone() {
            let Some(b) = session.bookmarks.clone() else {
                return;
            };
            let mode = b.snapshot().preferences.mode;
            if let Err(e) = b.configure(enabled, mode, None) {
                report(&weak, &sessions, &session, Err(e), None);
                return;
            }
            if enabled {
                session.refresh_watch_history();
                let (weak, sessions, generation) = (weak.clone(), sessions.clone(), b.generation());
                rt.spawn(async move {
                    let result = b.refresh().await;
                    if b.generation() == generation {
                        report(&weak, &sessions, &session, result, None);
                    }
                });
            }
        }
    });
    let (weak, sessions, rt) = (app.as_weak(), slot.clone(), runtime.clone());
    ui.on_set_bookmark_destination(move |mode, id| {
        let Some(session) = sessions.read().unwrap().clone() else {
            return;
        };
        let Some(b) = session.bookmarks.clone() else {
            return;
        };
        let enabled = weak
            .upgrade()
            .is_some_and(|a| a.global::<AppState>().get_bookmarks_enabled());
        // Advance the destination generation immediately, before any queued network work.
        let result = b.configure(
            enabled,
            if mode == 0 {
                DestinationMode::Automatic
            } else {
                DestinationMode::Selected
            },
            if id.is_empty() {
                None
            } else {
                Some(id.to_string())
            },
        );
        if let Err(e) = result {
            report(&weak, &sessions, &session, Err(e), None);
            return;
        }
        let (weak, sessions, generation) = (weak.clone(), sessions.clone(), b.generation());
        rt.spawn(async move {
            let result = if mode != 0 && b.snapshot().preferences.selected_id.is_empty() {
                b.load_choices().await
            } else if enabled {
                b.refresh().await
            } else {
                Ok(())
            };
            if b.generation() == generation {
                report(&weak, &sessions, &session, result, None);
            }
        });
    });
    for choices in [true, false] {
        let (weak, sessions, rt) = (app.as_weak(), slot.clone(), runtime.clone());
        let callback = move || {
            let Some(session) = sessions.read().unwrap().clone() else {
                return;
            };
            let (weak, sessions) = (weak.clone(), sessions.clone());
            rt.spawn(async move {
                let result = if let Some(b) = &session.bookmarks {
                    if choices {
                        b.load_choices().await
                    } else {
                        b.refresh().await
                    }
                } else {
                    Err(anyhow::anyhow!(session.bookmarks_error.clone()))
                };
                report(&weak, &sessions, &session, result, None);
            });
        };
        if choices {
            ui.on_load_bookmark_choices(callback);
        } else {
            ui.on_refresh_bookmarks(callback);
        }
    }
    let (weak, sessions) = (app.as_weak(), slot.clone());
    ui.on_set_bookmark_sort(move |sort| {
        let Some(session) = sessions.read().unwrap().clone() else {
            return;
        };
        if let Some(b) = &session.bookmarks {
            report(
                &weak,
                &sessions,
                &session,
                b.set_sort(if sort == 0 { Sort::Added } else { Sort::Played }),
                None,
            );
            if sort == 1 {
                session.refresh_watch_history();
            }
        }
    });
    let pending = Arc::new(Mutex::new(HashSet::new()));
    let (weak, sessions, rt) = (app.as_weak(), slot.clone(), runtime.clone());
    ui.on_toggle_bookmark(move |id| {
        let Some(session) = sessions.read().unwrap().clone() else {
            return;
        };
        let Some(b) = session.bookmarks.clone() else {
            return;
        };
        if !b.snapshot().enabled {
            return;
        }
        let key = (Arc::as_ptr(&session) as usize, id.to_string());
        if !pending.lock().unwrap().insert(key.clone()) {
            return;
        }
        let generation = b.generation();
        b.mark_pending(id.as_str());
        let (weak, sessions, pending, sender) = (
            weak.clone(),
            sessions.clone(),
            pending.clone(),
            sender.clone(),
        );
        rt.spawn(async move {
            let playlist = b.snapshot().preferences.id().to_string();
            let result = b.toggle(id.as_str(), generation).await;
            if b.generation() == generation {
                b.clear_pending(id.as_str());
            }
            pending.lock().unwrap().remove(&key);
            if b.generation() != generation {
                return;
            }
            if result.is_ok() {
                let _ = sender.send(Command::PlaylistChanged {
                    bookmarks_current: true,
                    playlist,
                    session: session.clone(),
                });
            }
            report(
                &weak,
                &sessions,
                &session,
                result,
                Some("Bookmarks updated"),
            );
        });
    });
    // A small UI-thread publisher keeps pure membership bindings reactive, including
    // cards created by navigation, recommendations, and restored feed snapshots.
    let previous = Rc::new(RefCell::new((0usize, u64::MAX, u64::MAX, u64::MAX)));
    let timer = Rc::new(slint::Timer::default());
    let keep_alive = timer.clone();
    let weak = app.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(app) = weak.upgrade() else {
                keep_alive.stop();
                return;
            };
            let ui = app.global::<AppState>();
            let session = slot.read().unwrap().clone();
            let key = session
                .as_ref()
                .map(|s| Arc::as_ptr(s) as usize)
                .unwrap_or(0);
            let mut previous = previous.borrow_mut();
            if key != previous.0 {
                *previous = (key, u64::MAX, u64::MAX, u64::MAX);
                ui.set_bookmarks_error("".into());
                ui.set_bookmark_destination_label("".into());
                ui.set_bookmark_choices(ModelRc::default());
                ui.set_bookmark_epoch(ui.get_bookmark_epoch().wrapping_add(1));
                if let Some(s) = session.clone() {
                    runtime.spawn(async move {
                        if let Some(b) = &s.bookmarks
                            && b.snapshot().enabled
                        {
                            let _ = b.ensure_loaded().await;
                        }
                    });
                }
            }
            let Some(session) = session else { return };
            let Some(b) = &session.bookmarks else {
                ui.set_bookmarks_error(session.bookmarks_error.clone().into());
                return;
            };
            let snapshot = b.snapshot();
            if previous.1 != snapshot.revision {
                previous.1 = snapshot.revision;
                ui.set_bookmarks_enabled(snapshot.enabled);
                ui.set_bookmarks_busy(snapshot.busy || snapshot.choices_busy);
                ui.set_bookmarks_error(snapshot.error.into());
                ui.set_bookmarks_note(snapshot.note.into());
                ui.set_bookmark_destination_label(snapshot.title.into());
                ui.set_bookmark_destination_id(snapshot.preferences.id().into());
                ui.set_bookmark_mode(if snapshot.preferences.mode == DestinationMode::Automatic {
                    0
                } else {
                    1
                });
                ui.set_bookmark_sort(if snapshot.preferences.sort == Sort::Added {
                    0
                } else {
                    1
                });
                ui.set_bookmark_choices(ModelRc::new(VecModel::from(
                    snapshot
                        .choices
                        .into_iter()
                        .map(|c| crate::LinkData {
                            title: format!("{} · {}", c.title, c.visibility).into(),
                            url: c.id.into(),
                        })
                        .collect::<Vec<_>>(),
                )));
                ui.set_bookmark_epoch(ui.get_bookmark_epoch().wrapping_add(1));
            }
            let history_revision = session.history_order_revision();
            // Navigation loads the current snapshot itself. Off the page, only track
            // revisions. On it, reload when the rendered page is behind, at most once
            // per revision pair so a failing load cannot repeat every tick.
            if ui.get_view_kind() != 9 || previous.2 == u64::MAX {
                previous.2 = snapshot.content_revision;
                previous.3 = history_revision;
            }
            if ui.get_view_kind() == 9
                && snapshot.enabled
                && !ui.get_busy()
                && !ui.get_refreshing()
                && b.needs_render(history_revision)
                && (previous.2, previous.3) != (snapshot.content_revision, history_revision)
            {
                previous.2 = snapshot.content_revision;
                previous.3 = history_revision;
                ui.invoke_reload(false);
            }
        },
    );
}
fn report(
    weak: &slint::Weak<AppWindow>,
    slot: &actions::SessionSlot,
    session: &Arc<Session>,
    result: anyhow::Result<()>,
    success: Option<&str>,
) {
    let (slot, session, success) = (slot.clone(), session.clone(), success.map(str::to_owned));
    update(weak, move |app| {
        if !slot
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &session))
        {
            return;
        }
        let ui = app.global::<AppState>();
        match result {
            Ok(()) => {
                if let Some(text) = success {
                    ui.set_toast(text.into());
                }
            }
            Err(e) => {
                if let Some(b) = &session.bookmarks {
                    b.set_error(e.to_string());
                }
                ui.set_bookmarks_error(e.to_string().into());
                ui.set_toast(e.to_string().into());
            }
        }
    });
}
