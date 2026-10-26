mod commands;
mod gl;
pub(crate) mod media_keys;
mod native;
pub(crate) mod progress;
mod quality;
mod queue;
mod render;
mod resolve;
#[cfg(test)]
mod smoke;
mod subtitles;

use crate::{AppState, AppWindow};
use commands::Command;
use libmpv2::Mpv;
use slint::ComponentHandle;
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{runtime::Handle, task::JoinHandle};

pub struct Playback {
    preferences: Arc<RwLock<crate::playback_settings::Preferences>>,
    automatic_selection: Mutex<crate::playback_settings::StreamSelection>,
    engine: Option<Arc<Mpv>>,
    progress: RwLock<Option<Arc<progress::Client>>>,
    report_tasks: progress::Tasks,
    cookies: RwLock<Option<Arc<str>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    subtitle_task: Mutex<Option<JoinHandle<()>>>,
    epoch: Arc<AtomicU64>,
    commands: Option<std::sync::mpsc::Sender<Command>>,
    ready: Arc<AtomicBool>,
    runtime: Handle,
    current: Mutex<(String, String)>,
    selected: Mutex<Option<String>>,
    choices: RwLock<Vec<quality::Quality>>,
    subtitle_choices: RwLock<Vec<subtitles::Track>>,
    cache: Arc<Mutex<quality::Cache>>,
    queue: Mutex<queue::Queue>,
    frame_step: Arc<AtomicBool>,
}

#[derive(Default)]
struct Start {
    new_video: bool,
    quality: Option<String>,
    subtitle: Option<String>,
    position: f32,
    paused: bool,
    refresh: bool,
    restore: bool,
}

fn selected_subtitle(app: &AppWindow, owner: &Playback) -> Option<String> {
    let index = app.global::<AppState>().get_player_subtitle_index();
    if index <= 0 {
        return None;
    }
    owner
        .subtitle_choices
        .read()
        .unwrap()
        .get(index as usize - 1)
        .map(|t| t.key.clone())
}

fn show_subtitles(
    app: &AppWindow,
    owner: &Arc<Playback>,
    tracks: Vec<subtitles::Track>,
    selected: Option<&str>,
    epoch: u64,
) {
    let index = selected
        .and_then(|key| tracks.iter().position(|t| t.key == key))
        .map_or(0, |i| i as i32 + 1);
    let mut labels = vec![slint::SharedString::from("Off")];
    labels.extend(
        tracks
            .iter()
            .map(|t| slint::SharedString::from(t.label.as_str())),
    );
    let state = app.global::<AppState>();
    state.set_player_subtitles(slint::ModelRc::new(slint::VecModel::from(labels)));
    state.set_player_subtitle_index(index);
    state.set_player_subtitle_note(if tracks.is_empty() {
        "No subtitles available.".into()
    } else {
        "".into()
    });
    *owner.subtitle_choices.write().unwrap() = tracks;
    if index == 0 || !state.get_player_error().is_empty() {
        state.set_player_subtitles_loading(false);
    } else {
        owner.load_subtitle(app, index, epoch);
    }
}

fn show_qualities(app: &AppWindow, owner: &Playback, catalogue: quality::Catalogue) {
    let selected = owner.selected.lock().unwrap().clone();
    let index = selected
        .as_ref()
        .and_then(|key| catalogue.options.iter().position(|q| &q.key == key))
        .map_or(0, |i| i as i32 + 1);
    // Labels read "1080p 60fps · codecs · ext · protocol · id"; the menu shows
    // the resolution as the title and everything after it as the detail line.
    let mut labels = vec![slint::SharedString::from("Use playback settings")];
    let mut details = vec![slint::SharedString::from("Preferred quality and codec")];
    for option in &catalogue.options {
        let (title, detail) = option
            .label
            .split_once(" · ")
            .unwrap_or((option.label.as_str(), ""));
        labels.push(title.into());
        details.push(if option.unavailable.is_empty() {
            detail.into()
        } else {
            format!("{detail} · unavailable").into()
        });
    }
    *owner.choices.write().unwrap() = catalogue.options;
    let state = app.global::<AppState>();
    state.set_player_qualities(slint::ModelRc::new(slint::VecModel::from(labels)));
    state.set_player_quality_details(slint::ModelRc::new(slint::VecModel::from(details)));
    state.set_player_quality_index(index);
    state.set_player_quality_note(catalogue.note.into());
    state.set_player_qualities_loading(false);
}

impl Drop for Playback {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
        if let Some(task) = self.subtitle_task.get_mut().unwrap().take() {
            task.abort();
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Shutdown(None));
        }
    }
}

impl Playback {
    pub fn set_session(&self, session: &crate::youtube::Session) {
        self.set_cookies(session.playback_cookies());
        *self.progress.write().unwrap() = session.progress.clone();
    }

    pub async fn shutdown(&self) {
        if let Some(task) = self.subtitle_task.lock().unwrap().take() {
            task.abort();
        }
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
        if let Some(commands) = &self.commands {
            let (done, wait) = tokio::sync::oneshot::channel();
            if commands.send(Command::Shutdown(Some(done))).is_ok() {
                let _ = wait.await;
            }
        }
        let tasks = std::mem::take(&mut *self.report_tasks.lock().unwrap());
        for task in tasks {
            let _ = task.await;
        }
    }

    pub fn set_cookies(&self, cookies: Arc<str>) {
        self.cancel();
        *self.progress.write().unwrap() = None;
        self.cache.lock().unwrap().clear();
        self.subtitle_choices.write().unwrap().clear();
        *self.cookies.write().unwrap() = Some(cookies);
    }

    fn cancel(&self) {
        if let Some(task) = self.subtitle_task.lock().unwrap().take() {
            task.abort();
        }
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Stop(epoch));
        }
    }

    fn load_subtitle(self: &Arc<Self>, app: &AppWindow, index: i32, epoch: u64) {
        let track = if index == 0 {
            None
        } else {
            let choices = self.subtitle_choices.read().unwrap();
            let Some(track) = choices.get(index as usize - 1) else {
                return;
            };
            Some(track.clone())
        };
        if let Some(task) = self.subtitle_task.lock().unwrap().take() {
            task.abort();
        }
        let state = app.global::<AppState>();
        let Some(commands) = self.commands.clone() else {
            state.set_player_subtitles_loading(false);
            state.set_player_subtitle_note("Could not load subtitles. Please retry.".into());
            return;
        };
        let previous_index = state.get_player_subtitle_index();
        state.set_player_subtitles_loading(true);
        state.set_player_subtitle_note("".into());
        let cookies = self.cookies.read().unwrap().clone().unwrap_or_default();
        let current = self.epoch.clone();
        let weak = app.as_weak();
        *self.subtitle_task.lock().unwrap() = Some(self.runtime.spawn(async move {
            let result = match track {
                Some(track) => subtitles::prepare(track, &cookies).await.map(Some),
                None => Ok(None),
            };
            if current.load(Ordering::Acquire) != epoch {
                return;
            }
            if let Ok(track) = result
                && commands
                    .send(Command::Subtitle {
                        epoch,
                        track,
                        index,
                    })
                    .is_ok()
            {
                return;
            }
            commands::publish(
                &weak,
                current,
                epoch,
                commands::Update::Subtitle {
                    index: previous_index,
                    note: "Could not load subtitles. Try another track or refresh streams.".into(),
                },
            );
        }));
    }

    fn play(self: &Arc<Self>, app: &AppWindow, id: String, title: String, entry: Option<usize>) {
        use slint::{Model, ModelRc, VecModel};
        let state = app.global::<AppState>();
        self.queue.lock().unwrap().current = entry;
        state.set_player_playlist_index(entry.map_or(-1, |i| i as i32));
        let mut trail = if state.get_player_open() {
            state.get_breadcrumbs().iter().collect::<Vec<_>>()
        } else {
            state.set_player_origin(match state.get_view_kind() {
                9 => "Bookmarks".into(),
                1 => "History".into(),
                4 => state.get_page_title(),
                5 | 6 => "Subscriptions".into(),
                8 => state.get_channel_name(),
                _ => "Home".into(),
            });
            Vec::new()
        };
        if let Some(last) = trail.last_mut() {
            last.position = state.get_player_position();
        }
        trail.push(crate::Breadcrumb {
            id: id.clone().into(),
            title: title.clone().into(),
            position: 0.,
            playlist_index: entry.map_or(-1, |i| i as i32),
        });
        state.set_breadcrumbs(ModelRc::new(VecModel::from(trail)));
        state.set_player_video_id(id.clone().into());
        state.set_scroll_reset(state.get_scroll_reset() + 1);
        self.load(
            app,
            id.clone(),
            title,
            Start {
                new_video: true,
                restore: true,
                ..Default::default()
            },
        );
        state.invoke_load_video_details(id.into());
    }

    fn load(self: &Arc<Self>, app: &AppWindow, id: String, title: String, start: Start) {
        self.cancel();
        let state = app.global::<AppState>();
        let preferences = self.preferences.read().unwrap().clone();
        let restore = start.restore && preferences.resume;
        let speed = if start.new_video {
            preferences.speed
        } else {
            state.get_player_speed()
        };
        if start.new_video {
            *self.automatic_selection.lock().unwrap() = preferences.selection();
        }
        let selection = self.automatic_selection.lock().unwrap().clone();
        state.set_player_speed(speed);
        state.set_player_open(true);
        state.set_player_title(title.clone().into());
        state.set_player_error("".into());
        state.set_player_progress_note("".into());
        state.set_player_loading(true);
        state.set_player_current_quality("".into());
        state.set_player_buffering(false);
        state.set_player_ended(false);
        state.set_player_paused(start.paused);
        state.set_player_position(start.position);
        self.subtitle_choices.write().unwrap().clear();
        state.set_player_subtitles(slint::ModelRc::new(slint::VecModel::from(vec![
            slint::SharedString::from("Off"),
        ])));
        state.set_player_subtitle_index(0);
        state.set_player_subtitle_note("".into());
        state.set_player_subtitles_loading(true);
        if start.position == 0. {
            state.set_player_duration(0.);
        }
        media_keys::refresh(app);
        *self.selected.lock().unwrap() = start.quality.clone();
        if start.refresh {
            self.cache.lock().unwrap().invalidate(&id);
        }
        if let Some(catalogue) = self.cache.lock().unwrap().catalogue(&id) {
            show_qualities(app, self, catalogue);
        } else {
            self.choices.write().unwrap().clear();
            state.set_player_qualities(slint::ModelRc::new(slint::VecModel::from(vec![
                slint::SharedString::from("Use playback settings"),
            ])));
            state.set_player_quality_details(slint::ModelRc::new(slint::VecModel::from(vec![
                slint::SharedString::from("Loading the format list…"),
            ])));
            state.set_player_quality_index(0);
            state.set_player_quality_note("".into());
            state.set_player_qualities_loading(true);
        }
        *self.current.lock().unwrap() = (id.clone(), title);
        let Some(commands) = self.commands.clone() else {
            fail(app, "The embedded mpv player could not be initialized.");
            return;
        };
        let Some(cookies) = self.cookies.read().unwrap().clone() else {
            fail(app, "Connect your browser profile before playing a video.");
            return;
        };
        if !resolve::valid_id(&id) {
            fail(app, "This video has an invalid YouTube ID.");
            return;
        }
        let epoch = self.epoch.load(Ordering::Relaxed);
        let weak = app.as_weak();
        let ready = self.ready.clone();
        let owner = Arc::downgrade(self);
        let cache = self.cache.clone();
        let generation = cache.lock().unwrap().generation;
        let selection_key = selection.cache_key();
        let progress_client = self.progress.read().unwrap().clone();
        *self.task.lock().unwrap() = Some(self.runtime.spawn(async move {
            let metadata = async {
                match &progress_client {
                    Some(client) => client.info(&id, restore).await.ok(),
                    None => None,
                }
            };
            let streams = async {
            let cached = cache.lock().unwrap().automatic(&id, &selection_key);
            if let Some(key) = &start.quality {
                let cached_catalogue = cache.lock().unwrap().catalogue(&id);
                let catalogue = match cached_catalogue {
                    Some(catalogue) => Ok(catalogue),
                    None => quality::load(&id, &cookies).await,
                };
                catalogue.and_then(|catalogue| {
                    let selected = catalogue.options.iter().find(|q| &q.key == key)
                        .ok_or_else(|| anyhow::anyhow!("This quality is no longer offered by YouTube. Choose another format."))?;
                    let media = selected.media.clone().ok_or_else(|| anyhow::anyhow!(selected.unavailable.clone()));
                    cache.lock().unwrap().store(generation, &id, None, Some(catalogue));
                    media
                })
            } else if let Some(media) = cached {
                Ok(media)
            } else {
                let media = resolve::resolve(&id, &cookies, &selection).await;
                if let Ok(media) = &media { cache.lock().unwrap().store(generation, &id, Some((selection_key.clone(), media.clone())), None); }
                media
            }
            };
            let (result, info) = tokio::join!(streams, metadata);
            let position = if restore { info.as_ref().and_then(|info| info.resume).unwrap_or(0.) as f32 } else { start.position };
            let tracking = progress_client.zip(info).map(Box::new);
            // Only automatic picks for a new video report a codec fallback.
            let codec_notice = match &result {
                Ok(media) if start.new_video && start.quality.is_none() => {
                    preferences.codec_fallback(&media.vcodec)
                }
                _ => None,
            };
            // Loading before RenderingSetup can make mpv reject its video output.
            for _ in 0..100 {
                if ready.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let playback_owner = owner.clone();
            let _ = weak.upgrade_in_event_loop(move |app| {
                let Some(owner) = playback_owner.upgrade() else {
                    return;
                };
                if owner.epoch.load(Ordering::Relaxed) != epoch
                    || !app.global::<AppState>().get_player_open()
                {
                    return;
                }
                if !ready.load(Ordering::Acquire) {
                    fail(
                        &app,
                        "Video rendering is unavailable. This player requires OpenGL.",
                    );
                    return;
                }
                match result {
                    Ok(media) => {
                        app.global::<AppState>().set_player_position(position);
                        if tracking.is_none() { app.global::<AppState>().set_player_progress_note("YouTube progress is unavailable for this playback.".into()); }
                        let loaded = media.source().and_then(|source| {
                            commands
                                .send(Command::Load {
                                    epoch,
                                    source,
                                    user_agent: media.user_agent,
                                    referer: media.referer,
                                    position: f64::from(position),
                                    tracking,
                                    paused: start.paused,
                                    speed,
                                })
                                .map_err(|_| anyhow::anyhow!("Playback worker stopped"))
                        });
                        if loaded.is_err() {
                            fail(&app, "Could not start this video. Please retry.");
                        } else if let Some(notice) = codec_notice {
                            app.global::<AppState>().set_toast(notice.into());
                        }
                    }
                    Err(error) => fail(&app, &error.to_string()),
                }
            });
            let cached_catalogue = cache.lock().unwrap().catalogue(&id);
            let catalogue = match cached_catalogue {
                Some(catalogue) => Ok(catalogue),
                None => quality::load(&id, &cookies).await,
            };
            if let Ok(catalogue) = &catalogue {
                cache.lock().unwrap().store(generation, &id, None, Some(catalogue.clone()));
            }
            let _ = weak.upgrade_in_event_loop(move |app| {
                let Some(owner) = owner.upgrade() else { return; };
                if owner.epoch.load(Ordering::Acquire) != epoch || !app.global::<AppState>().get_player_open() { return; }
                match catalogue {
                    Ok(catalogue) => {
                        let tracks = catalogue.subtitles.clone();
                        show_qualities(&app, &owner, catalogue);
                        let selected = if start.new_video {
                            subtitles::preferred(&tracks, preferences.subtitles).map(|t| t.key.clone())
                        } else { start.subtitle };
                        show_subtitles(&app, &owner, tracks, selected.as_deref(), epoch);
                    }
                    Err(error) => {
                        app.global::<AppState>().set_player_subtitles_loading(false);
                        app.global::<AppState>().set_player_subtitle_note("Subtitle lookup failed. Refresh streams to retry.".into());
                        app.global::<AppState>().set_player_qualities_loading(false);
                        app.global::<AppState>().set_player_quality_note(error.to_string().into());
                    }
                }
            });
        }));
    }
}

fn fail(app: &AppWindow, message: &str) {
    let state = app.global::<AppState>();
    state.set_player_loading(false);
    state.set_player_buffering(false);
    state.set_player_subtitles_loading(false);
    state.set_player_error(message.into());
    media_keys::refresh(app);
}

pub fn install(app: &AppWindow, runtime: Handle) -> Arc<Playback> {
    let engine = Mpv::with_initializer(|init| {
        init.set_option("vo", "libmpv")?;
        init.set_option("terminal", "no")?;
        init.set_option("config", "no")?;
        init.set_option("load-scripts", "no")?;
        init.set_option("ytdl", "no")?;
        init.set_option("keep-open", "yes")?;
        init.set_option("sub-auto", "no")?;
        init.set_option("network-timeout", 20.)?;
        init.set_option("volume", 70.)?;
        Ok(())
    })
    .ok()
    .map(Arc::new);
    let epoch = Arc::new(AtomicU64::new(0));
    media_keys::init(runtime.clone());
    let report_tasks = progress::Tasks::default();
    let commands = engine.as_ref().map(|engine| {
        commands::start(
            app,
            engine.clone(),
            epoch.clone(),
            runtime.clone(),
            report_tasks.clone(),
        )
    });
    let player = Arc::new(Playback {
        preferences: crate::playback_settings::wire(app),
        automatic_selection: Mutex::new(
            crate::playback_settings::Preferences::default().selection(),
        ),
        engine,
        progress: RwLock::new(None),
        report_tasks,
        cookies: RwLock::new(None),
        task: Mutex::new(None),
        subtitle_task: Mutex::new(None),
        epoch,
        commands,
        ready: Arc::new(AtomicBool::new(false)),
        runtime,
        current: Mutex::new((String::new(), String::new())),
        selected: Mutex::new(None),
        choices: RwLock::new(Vec::new()),
        subtitle_choices: RwLock::new(Vec::new()),
        cache: Arc::new(Mutex::new(quality::Cache::default())),
        queue: Mutex::default(),
        frame_step: Arc::new(AtomicBool::new(false)),
    });
    if let Some(engine) = &player.engine {
        register_renderer(app, engine.clone(), player.ready.clone());
    }
    let state = app.global::<AppState>();
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_play_video(move |id, title| {
        if let Some(app) = weak.upgrade() {
            owner.play(&app, id.to_string(), title.to_string(), None);
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_play_playlist_entry(move |index| {
        use slint::Model;
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<AppState>();
        if ui.get_view_kind() != 4 || index < 0 {
            return;
        }
        let entries = ui
            .get_videos()
            .iter()
            .map(|row| queue::Entry {
                id: row.id.to_string(),
                title: row.title.to_string(),
            })
            .collect::<Vec<_>>();
        let Some(row) = entries.get(index as usize).cloned() else {
            return;
        };
        owner.queue.lock().unwrap().entries = entries;
        owner.play(&app, row.id, row.title, Some(index as usize));
    });
    let weak = app.as_weak();
    state.on_play_playlist(move || {
        use slint::Model;
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            let rows = ui.get_videos();
            let count = rows.row_count();
            if count > 0 {
                let index = if ui.get_playlist_shuffle() {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos() as usize
                        % count
                } else {
                    0
                };
                ui.invoke_play_playlist_entry(index as i32);
            }
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_next(move || {
        use slint::Model;
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            if ui.get_player_playlist_index() >= 0 {
                let next = {
                    let queue = owner.queue.lock().unwrap();
                    let random = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos() as usize;
                    queue
                        .next(ui.get_playlist_shuffle(), random)
                        .map(|index| (index, queue.entries[index].clone()))
                };
                if let Some((index, row)) = next {
                    owner.play(&app, row.id, row.title, Some(index));
                } else {
                    ui.set_toast("No next video available".into());
                }
            } else if let Some(row) = ui.get_recommendations().row_data(0) {
                ui.invoke_play_video(row.id, row.title);
            } else {
                ui.set_toast("No next video available".into());
            }
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_breadcrumb(move |index| {
        use slint::{Model, ModelRc, VecModel};
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            let mut trail = ui.get_breadcrumbs().iter().collect::<Vec<_>>();
            let target = if index == -1 {
                trail.len() as i32 - 2
            } else {
                index
            };
            if target < 0 {
                ui.invoke_player_close();
                return;
            }
            if let Some(row) = trail.get(target as usize).cloned() {
                trail.truncate(target as usize + 1);
                ui.set_breadcrumbs(ModelRc::new(VecModel::from(trail)));
                ui.set_player_video_id(row.id.clone());
                ui.set_player_playlist_index(row.playlist_index);
                owner.queue.lock().unwrap().current = usize::try_from(row.playlist_index).ok();
                ui.set_scroll_reset(ui.get_scroll_reset() + 1);
                owner.load(
                    &app,
                    row.id.to_string(),
                    row.title.to_string(),
                    Start {
                        new_video: true,
                        position: row.position,
                        ..Default::default()
                    },
                );
                ui.invoke_load_video_details(row.id);
            }
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_set_speed(move |speed| {
        if speed.is_finite()
            && (0.25..=3.).contains(&speed)
            && let (Some(app), Some(commands)) = (weak.upgrade(), &owner.commands)
        {
            app.global::<AppState>().set_player_speed(speed);
            let _ = commands.send(Command::Speed(owner.epoch.load(Ordering::Acquire), speed));
        }
    });
    let owner = player.clone();
    state.on_player_frame_step(move |back| {
        // Skip while the previous step is still in progress, so holding the key
        // runs as fast as mpv can step and stops as soon as it is released.
        if let Some(commands) = &owner.commands
            && !owner.frame_step.swap(true, Ordering::AcqRel)
        {
            let _ = commands.send(Command::FrameStep(
                owner.epoch.load(Ordering::Acquire),
                back,
                owner.frame_step.clone(),
            ));
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_toggle_mute(move || {
        if let (Some(app), Some(commands)) = (weak.upgrade(), &owner.commands) {
            let ui = app.global::<AppState>();
            let muted = !ui.get_player_muted();
            ui.set_player_muted(muted);
            let _ = commands.send(Command::Mute(owner.epoch.load(Ordering::Acquire), muted));
        }
    });
    let weak = app.as_weak();
    let old_size = std::cell::Cell::new(slint::PhysicalSize::new(1440, 900));
    let old_maximized = std::cell::Cell::new(false);
    state.on_player_toggle_pip(move || {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<AppState>();
            let pip = !ui.get_player_pip();
            ui.set_player_pip(pip);
            if pip {
                old_size.set(app.window().size());
                old_maximized.set(app.window().is_maximized());
                app.window().set_maximized(false);
                app.window().set_fullscreen(false);
                ui.set_player_fullscreen(false);
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
                    if let Some(app) = weak.upgrade()
                        && app.global::<AppState>().get_player_pip()
                    {
                        app.window().set_size(slint::LogicalSize::new(640., 360.));
                    }
                });
            } else {
                app.window().set_size(old_size.get());
                app.window().set_maximized(old_maximized.get());
            }
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_retry(move || {
        if let Some(app) = weak.upgrade() {
            let (id, title) = owner.current.lock().unwrap().clone();
            let state = app.global::<AppState>();
            let quality = owner.selected.lock().unwrap().clone();
            owner.load(
                &app,
                id,
                title,
                Start {
                    new_video: false,
                    quality,
                    subtitle: selected_subtitle(&app, &owner),
                    position: state.get_player_position(),
                    paused: state.get_player_paused(),
                    refresh: true,
                    restore: false,
                },
            );
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_select_quality(move |index| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let state = app.global::<AppState>();
        if !state.get_player_open() || state.get_player_loading() || index < 0 {
            return;
        }
        let quality = if index == 0 {
            None
        } else {
            let choices = owner.choices.read().unwrap();
            let Some(choice) = choices.get(index as usize - 1) else {
                return;
            };
            Some(choice.key.clone())
        };
        let (id, title) = owner.current.lock().unwrap().clone();
        owner.load(
            &app,
            id,
            title,
            Start {
                new_video: false,
                quality,
                subtitle: selected_subtitle(&app, &owner),
                position: state.get_player_position(),
                paused: state.get_player_paused(),
                refresh: false,
                restore: false,
            },
        );
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_select_subtitle(move |index| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let state = app.global::<AppState>();
        if !state.get_player_open()
            || state.get_player_loading()
            || state.get_player_subtitles_loading()
            || !state.get_player_error().is_empty()
            || index < 0
        {
            return;
        }
        owner.load_subtitle(&app, index, owner.epoch.load(Ordering::Acquire));
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_close(move || {
        owner.cancel();
        if let Some(app) = weak.upgrade() {
            let state = app.global::<AppState>();
            if state.get_player_pip() {
                state.invoke_player_toggle_pip();
            }
            state.set_player_open(false);
            state.set_player_playlist_index(-1);
            *owner.queue.lock().unwrap() = queue::Queue::default();
            state.set_player_fullscreen(false);
            app.window().set_fullscreen(false);
            media_keys::refresh(&app);
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_toggle(move || {
        if let (Some(app), Some(commands)) = (weak.upgrade(), &owner.commands) {
            let state = app.global::<AppState>();
            if state.get_player_loading() || !state.get_player_error().is_empty() {
                return;
            }
            let _ = commands.send(Command::Toggle(
                owner.epoch.load(Ordering::Acquire),
                state.get_player_ended(),
            ));
        }
    });
    let owner = player.clone();
    state.on_player_seek(move |seconds| {
        if seconds.is_finite()
            && let Some(commands) = &owner.commands
        {
            let _ = commands.send(Command::Seek(
                owner.epoch.load(Ordering::Acquire),
                seconds.max(0.),
            ));
        }
    });
    let owner = player.clone();
    state.on_player_volume_changed(move |volume| {
        if volume.is_finite()
            && let Some(commands) = &owner.commands
        {
            let _ = commands.send(Command::Volume(
                owner.epoch.load(Ordering::Acquire),
                volume.clamp(0., 100.),
            ));
        }
    });
    let weak = app.as_weak();
    state.on_player_toggle_fullscreen(move || {
        if let Some(app) = weak.upgrade() {
            let state = app.global::<AppState>();
            let fullscreen = !app.window().is_fullscreen();
            app.window().set_fullscreen(fullscreen);
            state.set_player_fullscreen(fullscreen);
        }
    });
    player
}

fn register_renderer(app: &AppWindow, engine: Arc<Mpv>, ready: Arc<AtomicBool>) {
    let weak = app.as_weak();
    let mut renderer: Option<render::Renderer> = None;
    let result = app
        .window()
        .set_rendering_notifier(move |state, api| match state {
            slint::RenderingState::RenderingSetup => {
                let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
                    return;
                };
                let redraw = weak.clone();
                match render::Renderer::new(engine.clone(), get_proc_address, move || {
                    let _ = redraw.upgrade_in_event_loop(|app| app.window().request_redraw());
                }) {
                    Ok(value) => {
                        renderer = Some(value);
                        ready.store(true, Ordering::Release);
                    }
                    Err(_) => {
                        let _ = weak.upgrade_in_event_loop(|app| {
                            fail(&app, "Could not initialize OpenGL video rendering.")
                        });
                    }
                }
            }
            slint::RenderingState::BeforeRendering => {
                let (Some(renderer), Some(app)) = (renderer.as_mut(), weak.upgrade()) else {
                    return;
                };
                let state = app.global::<AppState>();
                if !state.get_player_open() {
                    return;
                }
                let scale = app.window().scale_factor();
                let width = (state.get_player_width() * scale).max(1.) as u32;
                let height = (state.get_player_height() * scale).max(1.) as u32;
                match renderer.render(width, height) {
                    Ok(Some(image)) => state.set_player_texture(image),
                    Ok(None) => {}
                    Err(_) => fail(&app, "Could not render the video. Please retry."),
                }
            }
            slint::RenderingState::RenderingTeardown => {
                ready.store(false, Ordering::Release);
                // Slint holds its window state mutably during teardown. Clear
                // the UI image later; setting it here requests a reentrant draw.
                let ready = ready.clone();
                let _ = weak.upgrade_in_event_loop(move |app| {
                    if !ready.load(Ordering::Acquire) {
                        app.global::<AppState>()
                            .set_player_texture(slint::Image::default());
                    }
                });
                renderer = None;
            }
            _ => {}
        });
    if result.is_err() {
        fail(app, "The OpenGL video renderer is unavailable.");
    }
}
