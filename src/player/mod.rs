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
    prefetch_task: Mutex<Option<JoinHandle<()>>>,
    prefetch_attempt: Mutex<Option<std::time::Instant>>,
    recovery: Mutex<resolve::Mode>,
    requested_position: Mutex<f32>,
    requested_subtitle: Mutex<Option<String>>,
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
    recovery: resolve::Mode,
}

fn selected_subtitle(owner: &Playback) -> Option<String> {
    // Preserve intent even if a stream fails while its subtitle is downloading.
    owner.requested_subtitle.lock().unwrap().clone()
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
    *owner.requested_subtitle.lock().unwrap() =
        (index > 0).then(|| tracks[index as usize - 1].key.clone());
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
        if let Some(task) = self.prefetch_task.get_mut().unwrap().take() {
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
        self.cancel_prefetch();
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

    /// Forgets the account entirely, used when its saved profile is removed.
    pub fn clear_session(&self) {
        self.cancel();
        *self.progress.write().unwrap() = None;
        self.cache.lock().unwrap().clear();
        self.subtitle_choices.write().unwrap().clear();
        *self.cookies.write().unwrap() = None;
    }

    pub fn set_cookies(&self, cookies: Arc<str>) {
        self.cancel();
        *self.progress.write().unwrap() = None;
        self.cache.lock().unwrap().clear();
        self.subtitle_choices.write().unwrap().clear();
        *self.cookies.write().unwrap() = Some(cookies);
    }

    fn cancel(&self) {
        self.cancel_prefetch();
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

    fn cancel_prefetch(&self) {
        *self.prefetch_attempt.lock().unwrap() = None;
        if let Some(task) = self.prefetch_task.lock().unwrap().take() {
            task.abort();
        }
    }

    fn prefetch_next(self: &Arc<Self>, app: &AppWindow) {
        let state = app.global::<AppState>();
        let preferences = self.preferences.read().unwrap().clone();
        if !preferences.prefetch
            || !state.get_player_open()
            || state.get_player_loading()
            || !state.get_player_error().is_empty()
            || state.get_playlist_shuffle()
            || state.get_player_paused()
            || state.get_player_buffering()
            || state.get_player_ended()
            || !prefetch_due(
                state.get_player_position(),
                state.get_player_duration(),
                state.get_player_speed(),
            )
        {
            return;
        }
        let id = {
            let queue = self.queue.lock().unwrap();
            let Some(index) = queue.next(false, 0) else {
                return;
            };
            queue.entries[index].id.clone()
        };
        let selection = preferences.selection();
        let key = selection.cache_key();
        if self.cache.lock().unwrap().automatic(&id, &key).is_some() {
            return;
        }
        let mut task = self.prefetch_task.lock().unwrap();
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let Some(cookies) = self.cookies.read().unwrap().clone() else {
            return;
        };
        let mut attempt = self.prefetch_attempt.lock().unwrap();
        if attempt.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(60)) {
            return;
        }
        *attempt = Some(std::time::Instant::now());
        let cache = self.cache.clone();
        let generation = cache.lock().unwrap().generation;
        // Only the next sequential playlist entry. No stream checks, media
        // requests, progress reporting, or subtitle downloads are made here.
        *task = Some(self.runtime.spawn(async move {
            if let Ok(lookup) =
                resolve::resolve(&id, &cookies, &selection, resolve::Mode::Normal).await
            {
                cache.lock().unwrap().store(
                    generation,
                    &id,
                    Some((key, lookup.media)),
                    lookup.catalogue.ok(),
                );
            }
        }));
    }

    fn recover(self: &Arc<Self>, app: &AppWindow) -> bool {
        let Some(mode) = self.recovery.lock().unwrap().next() else {
            return false;
        };
        let state = app.global::<AppState>();
        let (id, title) = self.current.lock().unwrap().clone();
        if !resolve::valid_id(&id) || !state.get_player_open() {
            return false;
        }
        let quality = self.selected.lock().unwrap().clone();
        let position = if state.get_player_loading() {
            *self.requested_position.lock().unwrap()
        } else {
            state.get_player_position()
        };
        self.load(
            app,
            id,
            title,
            Start {
                quality,
                subtitle: selected_subtitle(self),
                position,
                paused: state.get_player_paused(),
                refresh: true,
                recovery: mode,
                ..Default::default()
            },
        );
        true
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
        *self.requested_subtitle.lock().unwrap() = track.as_ref().map(|track| track.key.clone());
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
        *self.recovery.lock().unwrap() = start.recovery;
        *self.requested_position.lock().unwrap() = start.position;
        *self.requested_subtitle.lock().unwrap() = start.subtitle.clone();
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
        let mut selection = self.automatic_selection.lock().unwrap().clone();
        if let Some(key) = &start.quality {
            let Some(format) = quality::manual_selection(key) else {
                fail(
                    app,
                    "This format cannot be selected. Choose another quality.",
                );
                return;
            };
            selection.format = format;
        }
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
        let current_epoch = self.epoch.clone();
        *self.task.lock().unwrap() = Some(self.runtime.spawn(async move {
            let metadata = async {
                match &progress_client {
                    Some(client) => client.info(&id, restore).await.ok(),
                    None => None,
                }
            };
            let streams = async {
                if let Some(key) = &start.quality {
                    let catalogue = cache.lock().unwrap().catalogue(&id);
                    if let Some(catalogue) = catalogue {
                        return catalogue
                            .options
                            .iter()
                            .find(|q| &q.key == key)
                            .ok_or_else(|| anyhow::anyhow!("This quality is no longer available."))?
                            .media
                            .clone()
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "This format cannot be played. Choose another quality."
                                )
                            });
                    }
                } else if let Some(media) = cache.lock().unwrap().automatic(&id, &selection_key) {
                    return Ok(media);
                }
                let lookup = resolve::resolve(&id, &cookies, &selection, start.recovery).await?;
                if let Ok(catalogue) = &lookup.catalogue {
                    let catalogue = catalogue.clone();
                    let formats_owner = owner.clone();
                    let _ = weak.upgrade_in_event_loop(move |app| {
                        if let Some(owner) = formats_owner.upgrade()
                            && commands::accepts(
                                owner.epoch.load(Ordering::Acquire),
                                epoch,
                                app.global::<AppState>().get_player_open(),
                            )
                        {
                            show_qualities(&app, &owner, catalogue);
                        }
                    });
                }
                cache.lock().unwrap().store(
                    generation,
                    &id,
                    start
                        .quality
                        .is_none()
                        .then(|| (selection_key.clone(), lookup.media.clone())),
                    lookup.catalogue.ok(),
                );
                Ok(lookup.media)
            };
            let mut metadata = Box::pin(metadata);
            let (result, info, pending_metadata) =
                startup(streams, metadata.as_mut(), restore).await;
            let position = if restore {
                info.as_ref().and_then(|info| info.resume).unwrap_or(0.) as f32
            } else {
                start.position
            };
            let tracking = progress_client.clone().zip(info).map(Box::new);
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
            let playable = result.is_ok();
            let playback_owner = owner.clone();
            let playback_commands = commands.clone();
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
                        *owner.requested_position.lock().unwrap() = position;
                        app.global::<AppState>().set_player_position(position);
                        if tracking.is_none() && !pending_metadata {
                            app.global::<AppState>().set_player_progress_note(
                                "YouTube progress is unavailable for this playback.".into(),
                            );
                        }
                        let loaded = media.source().and_then(|source| {
                            playback_commands
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
                    Err(error) => {
                        // A failed public recovery can still try validated
                        // signed-in streams (e.g. an account-only video).
                        if start.recovery != resolve::Mode::Public || !owner.recover(&app) {
                            fail(&app, &error.to_string());
                        }
                    }
                }
            });
            let menus = async {
                if !playable {
                    return;
                }
                let cached = cache.lock().unwrap().catalogue(&id);
                let catalogue = match cached {
                    Some(catalogue) => Ok(catalogue),
                    None => resolve::catalogue(&id, &cookies, start.recovery).await,
                };
                if let Ok(catalogue) = &catalogue {
                    cache
                        .lock()
                        .unwrap()
                        .store(generation, &id, None, Some(catalogue.clone()));
                }
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let Some(owner) = owner.upgrade() else {
                        return;
                    };
                    if owner.epoch.load(Ordering::Acquire) != epoch
                        || !app.global::<AppState>().get_player_open()
                    {
                        return;
                    }
                    match catalogue {
                        Ok(catalogue) => {
                            let tracks = catalogue.subtitles.clone();
                            show_qualities(&app, &owner, catalogue);
                            let selected = if start.new_video {
                                subtitles::preferred(&tracks, preferences.subtitles)
                                    .map(|t| t.key.clone())
                            } else {
                                start.subtitle
                            };
                            show_subtitles(&app, &owner, tracks, selected.as_deref(), epoch);
                        }
                        Err(error) => {
                            app.global::<AppState>().set_player_subtitles_loading(false);
                            app.global::<AppState>().set_player_subtitle_note(
                                "Subtitle lookup failed. Refresh streams to retry.".into(),
                            );
                            app.global::<AppState>().set_player_qualities_loading(false);
                            app.global::<AppState>()
                                .set_player_quality_note(error.to_string().into());
                        }
                    }
                });
            };
            let tracking = async {
                if pending_metadata {
                    let info = metadata.await;
                    let tracking = progress_client.clone().zip(info).map(Box::new);
                    // Queue on Slint after Load, so fast metadata cannot attach to
                    // the previous file. The worker also checks the load epoch.
                    let _ = weak.upgrade_in_event_loop(move |app| {
                        if commands::accepts(
                            current_epoch.load(Ordering::Acquire),
                            epoch,
                            app.global::<AppState>().get_player_open(),
                        ) {
                            let _ = commands.send(Command::Tracking { epoch, tracking });
                        }
                    });
                }
            };
            tokio::join!(menus, tracking);
        }));
    }
}

/// Use wall-clock remaining time so slow playback cannot outlast the cache TTL.
fn prefetch_due(position: f32, duration: f32, speed: f32) -> bool {
    position.is_finite()
        && position >= 0.
        && duration.is_finite()
        && duration > position
        && speed.is_finite()
        && speed > 0.
        && (duration - position) / speed <= 120.
}

/// Poll both operations concurrently, but only resume makes metadata a gate.
async fn startup<T, M>(
    streams: impl std::future::Future<Output = T>,
    mut metadata: std::pin::Pin<&mut impl std::future::Future<Output = Option<M>>>,
    restore: bool,
) -> (T, Option<M>, bool) {
    if restore {
        let (streams, metadata) = tokio::join!(streams, metadata);
        return (streams, metadata, false);
    }
    tokio::pin!(streams);
    tokio::select! {
        result = &mut streams => (result, None, true),
        info = &mut metadata => (streams.await, info, false),
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
        prefetch_task: Mutex::new(None),
        prefetch_attempt: Mutex::new(None),
        recovery: Mutex::new(resolve::Mode::Normal),
        requested_position: Mutex::new(0.),
        requested_subtitle: Mutex::new(None),
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
    state.on_player_recover_stream(move || weak.upgrade().is_some_and(|app| owner.recover(&app)));
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_player_prefetch_check(move || {
        if let Some(app) = weak.upgrade() {
            owner.prefetch_next(&app);
        }
    });
    let (weak, owner) = (app.as_weak(), player.clone());
    state.on_playback_prefetch_changed(move || {
        owner.cancel_prefetch();
        if let Some(app) = weak.upgrade() {
            owner.prefetch_next(&app);
        }
    });
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
                    subtitle: selected_subtitle(&owner),
                    position: state.get_player_position(),
                    paused: state.get_player_paused(),
                    refresh: true,
                    restore: false,
                    recovery: resolve::Mode::Normal,
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
                subtitle: selected_subtitle(&owner),
                position: state.get_player_position(),
                paused: state.get_player_paused(),
                refresh: false,
                restore: false,
                recovery: resolve::Mode::Normal,
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

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn prefetch_waits_until_two_minutes_remain_at_the_current_speed() {
        assert!(!prefetch_due(0., 600., 1.));
        assert!(!prefetch_due(479., 600., 1.));
        assert!(prefetch_due(480., 600., 1.));
        assert!(prefetch_due(0., 90., 1.));
        assert!(!prefetch_due(480., 600., 0.25));
        assert!(prefetch_due(570., 600., 0.25));
        assert!(prefetch_due(360., 600., 2.));
        for (position, duration, speed) in [
            (0., 0., 1.),
            (600., 600., 1.),
            (601., 600., 1.),
            (-1., 90., 1.),
            (0., f32::INFINITY, 1.),
            (f32::NAN, 90., 1.),
            (0., 90., 0.),
        ] {
            assert!(!prefetch_due(position, duration, speed));
        }
    }

    #[tokio::test]
    async fn playback_without_resume_does_not_wait_for_metadata() {
        let mut metadata = Box::pin(std::future::pending::<Option<u32>>());
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            startup(async { "stream" }, metadata.as_mut(), false),
        )
        .await
        .unwrap();
        assert_eq!(result, ("stream", None, true));
    }

    #[tokio::test]
    async fn resume_waits_for_metadata_and_retains_the_server_position() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut metadata = Box::pin(async { receive.await.ok() });
        let mut startup = Box::pin(startup(async { "stream" }, metadata.as_mut(), true));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut startup)
                .await
                .is_err()
        );
        send.send(42).unwrap();
        assert_eq!(startup.await, ("stream", Some(42), false));
    }

    #[tokio::test]
    async fn early_metadata_is_kept_while_streams_load() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut metadata = Box::pin(async { Some(42) });
        let mut startup = Box::pin(startup(
            async { receive.await.unwrap() },
            metadata.as_mut(),
            false,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut startup)
                .await
                .is_err()
        );
        send.send("stream").unwrap();
        assert_eq!(startup.await, ("stream", Some(42), false));
    }
}
