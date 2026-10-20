//! All synchronous mpv commands and property reads run on this worker, never
//! on Slint's rendering thread. Each load gets its own event client and epoch.
use crate::{AppState, AppWindow};
use libmpv2::{
    Format, Mpv,
    events::{Event, PropertyData},
    mpv_end_file_reason,
};
use slint::ComponentHandle;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    time::{Duration, Instant},
};

pub enum Command {
    Stop(u64),
    Load {
        epoch: u64,
        source: String,
        user_agent: String,
        referer: String,
        position: f64,
        paused: bool,
        tracking: Option<Box<(Arc<super::progress::Client>, super::progress::Info)>>,
    },
    Toggle(u64, bool),
    Seek(u64, f32),
    Volume(u64, f32),
    Speed(u64, f32),
    Mute(u64, bool),
    Subtitle {
        epoch: u64,
        track: Option<super::subtitles::Track>,
        index: i32,
    },
    Shutdown(Option<tokio::sync::oneshot::Sender<()>>),
}

#[derive(Debug, PartialEq)]
pub(super) enum Update {
    Loaded,
    Progress(bool),
    Paused(bool),
    Position(f64),
    Duration(f64),
    Volume(f64),
    Buffering(bool),
    Ended(bool),
    Subtitle { index: i32, note: String },
    Error(libmpv2::Error, Option<u16>),
}

fn playback_error(error: &libmpv2::Error, http_status: Option<u16>) -> String {
    // libmpv's fixed error descriptions contain no media URLs or session data.
    // Do not forward player logs, which can include signed stream URLs.
    let detail = match error {
        libmpv2::Error::Raw(code) => {
            let description = unsafe { libmpv2_sys::mpv_error_string(*code) };
            if description.is_null() {
                format!("mpv error {code}")
            } else {
                let description = unsafe { std::ffi::CStr::from_ptr(description) };
                format!("{} (mpv {code})", description.to_string_lossy())
            }
        }
        _ => format!("mpv {error}"),
    };
    match http_status {
        Some(status) => format!(
            "Playback failed: the video server returned HTTP {status} ({detail}). Retry to get fresh video streams."
        ),
        None => format!("Playback failed: {detail}. Retry to get fresh video streams."),
    }
}

fn http_error_status(message: &str) -> Option<u16> {
    // Only retain a status code from FFmpeg's HTTP errors, never the original
    // log message, which may include signed URLs or other session information.
    let suffix = message.split_once("HTTP error ")?.1;
    let code = suffix.split_ascii_whitespace().next()?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    code.parse().ok().filter(|code| (400..600).contains(code))
}

pub(super) fn accepts(current: u64, event_epoch: u64, open: bool) -> bool {
    open && current == event_epoch
}

pub(super) fn publish(
    weak: &slint::Weak<AppWindow>,
    current: Arc<AtomicU64>,
    epoch: u64,
    update: Update,
) {
    let _ = weak.upgrade_in_event_loop(move |app| {
        let state = app.global::<AppState>();
        if !accepts(
            current.load(Ordering::Acquire),
            epoch,
            state.get_player_open(),
        ) {
            return;
        }
        match update {
            Update::Progress(ok) => state.set_player_progress_note(
                if ok {
                    ""
                } else {
                    "Could not sync watch progress to YouTube. Retrying…"
                }
                .into(),
            ),
            Update::Loaded => {
                state.set_player_loading(false);
                state.set_player_buffering(false);
            }
            Update::Paused(v) => state.set_player_paused(v),
            Update::Position(v) if v.is_finite() => state.set_player_position(v.max(0.) as f32),
            Update::Duration(v) if v.is_finite() => state.set_player_duration(v.max(0.) as f32),
            Update::Volume(v) if v.is_finite() => state.set_player_volume(v as f32),
            Update::Buffering(v) => state.set_player_buffering(v),
            Update::Ended(v) => {
                state.set_player_ended(v);
                if v && state.get_player_playlist_index() >= 0 {
                    state.invoke_player_next();
                }
            }
            Update::Subtitle { index, note } => {
                state.set_player_subtitle_index(index);
                state.set_player_subtitle_note(note.into());
                state.set_player_subtitles_loading(false);
            }
            Update::Error(error, status) => super::fail(&app, &playback_error(&error, status)),
            _ => {}
        }
    });
}

#[cfg(test)]
pub(super) static IDLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
#[cfg(test)]
pub(super) static AUDIO_CHANNELS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

fn stop(engine: &Mpv) -> libmpv2::Result<()> {
    engine.command("stop", &[])?;
    // stop can be acknowledged before unloading finishes. Do not subscribe a
    // new client until the old file has actually left the core.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !engine.get_property::<bool>("idle-active")? {
        if Instant::now() >= deadline {
            return Err(libmpv2::Error::Raw(-1));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    #[cfg(test)]
    IDLE.store(true, Ordering::Release);
    Ok(())
}

fn observer(engine: &Mpv) -> libmpv2::Result<Mpv> {
    // A fresh client has no queued events from the previous file.
    let client = engine.create_client(None)?;
    unsafe {
        libmpv2_sys::mpv_request_log_messages(client.ctx.as_ptr(), c"warn".as_ptr());
    }
    for (index, name, format) in [
        (1, "pause", Format::Flag),
        (2, "time-pos", Format::Double),
        (3, "duration", Format::Double),
        (4, "volume", Format::Double),
        (5, "paused-for-cache", Format::Flag),
        (6, "eof-reached", Format::Flag),
    ] {
        client.observe_property(name, format, index)?;
    }
    Ok(client)
}

fn sample_progress(engine: &Mpv, tracker: &mut Option<super::progress::Tracker>) {
    if let Some(tracker) = tracker
        && let (Ok(position), Ok(duration)) = (
            engine.get_property::<f64>("time-pos"),
            engine.get_property::<f64>("duration"),
        )
    {
        tracker.observe(
            super::progress::Sample {
                position,
                duration,
                paused: engine.get_property::<bool>("pause").unwrap_or(true),
                buffering: engine
                    .get_property::<bool>("paused-for-cache")
                    .unwrap_or(true),
                speed: engine.get_property::<f64>("speed").unwrap_or(1.),
            },
            Instant::now(),
        );
    }
}

pub fn start(
    app: &AppWindow,
    engine: Arc<Mpv>,
    current: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
    tasks: super::progress::Tasks,
) -> Sender<Command> {
    let (sender, receiver) = mpsc::channel();
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let mut events: Option<(u64, Mpv, Option<u16>)> = None;
        let mut loaded = false;
        let mut subtitle = None;
        let mut tracker: Option<super::progress::Tracker> = None;
        let mut sampled = Instant::now();
        let mut shutdown = None;
        loop {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(Command::Shutdown(done)) => {
                    shutdown = done;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(command) => {
                    let epoch = match &command {
                        Command::Stop(e)
                        | Command::Load { epoch: e, .. }
                        | Command::Toggle(e, _)
                        | Command::Seek(e, _)
                        | Command::Volume(e, _)
                        | Command::Speed(e, _)
                        | Command::Mute(e, _) => *e,
                        Command::Subtitle { epoch, .. } => *epoch,
                        Command::Shutdown(_) => unreachable!(),
                    };
                    if current.load(Ordering::Acquire) != epoch {
                        continue;
                    }
                    if loaded {
                        sample_progress(&engine, &mut tracker);
                    }
                    let result = match command {
                        Command::Stop(_) => {
                            tracker = None;
                            events = None;
                            loaded = false;
                            subtitle = None;
                            stop(&engine)
                        }
                        Command::Load {
                            source,
                            user_agent,
                            referer,
                            position,
                            paused,
                            tracking,
                            ..
                        } => {
                            tracker = None;
                            events = None;
                            loaded = false;
                            subtitle = None;
                            (|| {
                                stop(&engine)?;
                                if current.load(Ordering::Acquire) != epoch {
                                    return Ok(());
                                }
                                engine.set_property("user-agent", user_agent.as_str())?;
                                engine.set_property("referrer", referer.as_str())?;
                                engine
                                    .set_property("start", position.max(0.).to_string().as_str())?;
                                engine.set_property("pause", paused)?;
                                engine.set_property("sid", "no")?;
                                let client = observer(&engine)?;
                                #[cfg(test)]
                                AUDIO_CHANNELS.store(0, Ordering::Release);
                                engine.command("loadfile", &[&source, "replace"])?;
                                events = Some((epoch, client, None));
                                if let Some((client, info)) = tracking.map(|t| *t) {
                                    let status_weak = weak.clone();
                                    let status_current = current.clone();
                                    tracker = super::progress::Tracker::new(
                                        client,
                                        info,
                                        runtime.clone(),
                                        &tasks,
                                        move |ok| {
                                            publish(
                                                &status_weak,
                                                status_current.clone(),
                                                epoch,
                                                Update::Progress(ok),
                                            )
                                        },
                                    );
                                    if tracker.is_none() {
                                        publish(
                                            &weak,
                                            current.clone(),
                                            epoch,
                                            Update::Progress(false),
                                        );
                                    }
                                }
                                #[cfg(test)]
                                IDLE.store(false, Ordering::Release);
                                Ok(())
                            })()
                        }
                        _ if !events.as_ref().is_some_and(|(e, _, _)| *e == epoch) => Ok(()),
                        Command::Toggle(_, replay) => {
                            if replay {
                                if let Some(tracker) = &mut tracker {
                                    tracker.seek(true);
                                }
                                engine
                                    .command("seek", &["0", "absolute"])
                                    .and_then(|()| engine.set_property("pause", false))
                            } else {
                                engine.command("cycle", &["pause"])
                            }
                        }
                        Command::Seek(_, seconds) => {
                            if let Some(tracker) = &mut tracker {
                                tracker.seek(true);
                            }
                            engine.command("seek", &[&seconds.to_string(), "absolute"])
                        }
                        Command::Volume(_, volume) => {
                            engine.set_property("volume", f64::from(volume))
                        }
                        Command::Speed(_, speed) => engine.set_property("speed", f64::from(speed)),
                        Command::Mute(_, muted) => engine.set_property("mute", muted),
                        Command::Subtitle { track, index, .. } => {
                            subtitle = Some((track, index));
                            Ok(())
                        }
                        Command::Shutdown(_) => unreachable!(),
                    };
                    if let Err(error) = result {
                        publish(&weak, current.clone(), epoch, Update::Error(error, None));
                    }
                }
            }
            if loaded && sampled.elapsed() >= Duration::from_millis(200) {
                sample_progress(&engine, &mut tracker);
                sampled = Instant::now();
            }
            if let Some((epoch, client, http_status)) = events.as_mut() {
                for _ in 0..64 {
                    let Some(event) = client.wait_event(0.) else {
                        break;
                    };
                    let update = match event {
                        #[cfg(test)]
                        Ok(Event::AudioReconfig) => {
                            AUDIO_CHANNELS.store(
                                engine
                                    .get_property::<i64>("audio-params/channel-count")
                                    .unwrap_or(0),
                                Ordering::Release,
                            );
                            None
                        }
                        Ok(Event::LogMessage { text, .. }) => {
                            if let Some(status) = http_error_status(text) {
                                *http_status = Some(status);
                            }
                            None
                        }
                        Ok(Event::Seek) => {
                            // Also fires for our own seeks; the tracker ignores the repeat.
                            if let Some(tracker) = &mut tracker {
                                tracker.seek(false);
                            }
                            None
                        }
                        Ok(Event::PlaybackRestart) => {
                            if let Some(tracker) = &mut tracker {
                                tracker.restarted();
                            }
                            None
                        }
                        Ok(Event::FileLoaded) => {
                            loaded = true;
                            *http_status = None;
                            #[cfg(test)]
                            AUDIO_CHANNELS.store(
                                engine
                                    .get_property::<i64>("audio-params/channel-count")
                                    .unwrap_or(0),
                                Ordering::Release,
                            );
                            Some(Update::Loaded)
                        }
                        Ok(Event::PropertyChange { name, change, .. }) => match (name, change) {
                            ("pause", PropertyData::Flag(v)) => Some(Update::Paused(v)),
                            ("time-pos", PropertyData::Double(v)) => Some(Update::Position(v)),
                            ("duration", PropertyData::Double(v)) => Some(Update::Duration(v)),
                            ("volume", PropertyData::Double(v)) => Some(Update::Volume(v)),
                            ("paused-for-cache", PropertyData::Flag(v)) => {
                                Some(Update::Buffering(v))
                            }
                            ("eof-reached", PropertyData::Flag(v)) => Some(Update::Ended(v)),
                            _ => None,
                        },
                        Ok(Event::EndFile(reason)) if reason == mpv_end_file_reason::Eof => {
                            Some(Update::Ended(true))
                        }
                        Err(error) => Some(Update::Error(error, *http_status)),
                        _ => None,
                    };
                    if let Some(update) = update {
                        if matches!(update, Update::Ended(true) | Update::Error(_, _)) {
                            sample_progress(&engine, &mut tracker);
                            if let Some(tracker) = &mut tracker {
                                tracker.flush(true);
                            }
                        }
                        publish(&weak, current.clone(), *epoch, update);
                    }
                }
                if loaded
                    && current.load(Ordering::Acquire) == *epoch
                    && let Some((track, index)) = subtitle.take()
                {
                    let update = match super::subtitles::select(&engine, track.as_ref()) {
                        Ok(()) => Update::Subtitle {
                            index,
                            note: String::new(),
                        },
                        Err(_) => {
                            let _ = engine.set_property("sid", "no");
                            Update::Subtitle { index: 0, note: "Could not load subtitles. Try another track or refresh streams.".into() }
                        }
                    };
                    publish(&weak, current.clone(), *epoch, update);
                }
            }
        }
        sample_progress(&engine, &mut tracker);
        drop(tracker);
        drop(events);
        let _ = stop(&engine);
        if let Some(done) = shutdown {
            let _ = done.send(());
        }
    });
    sender
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playback_failure_preserves_the_mpv_error_category() {
        let loading = playback_error(&libmpv2::Error::Raw(-13), None);
        let format = playback_error(&libmpv2::Error::Raw(-17), None);
        assert!(loading.contains("mpv -13"));
        assert!(format.contains("mpv -17"));
        assert_ne!(loading, format);
        assert!(!format.contains("Check your connection"));
    }

    #[test]
    fn network_errors_only_retain_the_http_status() {
        let log = "HTTP error 403 Forbidden https://cdn.example/?secret=synthetic-secret";
        let status = http_error_status(log);
        assert_eq!(status, Some(403));
        let message = playback_error(&libmpv2::Error::Raw(-13), status);
        assert!(message.contains("HTTP 403"));
        assert!(!message.contains("synthetic-secret"));
        assert!(!message.contains("https://"));
        for log in [
            "HTTP error 200 OK",
            "HTTP error 403secret",
            "HTTP error invalid",
            "unknown error",
        ] {
            assert_eq!(http_error_status(log), None);
        }
    }

    #[test]
    fn queued_events_from_an_old_video_or_retry_are_rejected() {
        assert!(accepts(1, 1, true));
        assert!(!accepts(2, 1, false)); // Back
        assert!(!accepts(3, 1, true)); // Open another video
        assert!(!accepts(4, 3, true)); // Retry
        assert!(accepts(4, 4, true));
    }
}
