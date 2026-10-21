//! Registers the player with the OS media session, so headphone buttons, media
//! keys and "now playing" widgets control it: MPRIS on Linux, System Media
//! Transport Controls on Windows and Now Playing on macOS.

use std::cell::RefCell;

use slint::ComponentHandle;

use crate::{AppState, AppWindow};

/// Next/Previous skip within the video, so headphone double and triple taps
/// never leave it.
const SKIP_SECONDS: f64 = 10.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Action {
    Toggle,
    Play,
    Pause,
    SeekBy(f64),
    SeekTo(f64),
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Effect {
    Toggle,
    Seek(f64),
    Close,
}

#[derive(Clone, Copy, Debug, Default)]
struct Player {
    open: bool,
    paused: bool,
    position: f64,
    duration: f64,
}

fn decide(action: Action, player: Player) -> Option<Effect> {
    if !player.open {
        return None;
    }
    let clamp = |seconds: f64| {
        let seconds = seconds.max(0.);
        if player.duration > 0. {
            seconds.min(player.duration)
        } else {
            seconds
        }
    };
    match action {
        Action::Toggle => Some(Effect::Toggle),
        Action::Play if player.paused => Some(Effect::Toggle),
        Action::Pause if !player.paused => Some(Effect::Toggle),
        Action::Play | Action::Pause => None,
        Action::SeekBy(offset) if offset.is_finite() => {
            Some(Effect::Seek(clamp(player.position + offset)))
        }
        Action::SeekTo(seconds) if seconds.is_finite() => Some(Effect::Seek(clamp(seconds))),
        Action::SeekBy(_) | Action::SeekTo(_) => None,
        Action::Stop => Some(Effect::Close),
    }
}

/// Runs an OS media action. Must be called on the UI thread.
pub(crate) fn apply(app: &AppWindow, action: Action) {
    let state = app.global::<AppState>();
    let player = Player {
        open: state.get_player_open(),
        paused: state.get_player_paused() || state.get_player_ended(),
        position: f64::from(state.get_player_position()),
        duration: f64::from(state.get_player_duration()),
    };
    // Stop remains available while a video is loading or has failed.
    if action != Action::Stop
        && (state.get_player_loading() || !state.get_player_error().is_empty())
    {
        return;
    }
    match decide(action, player) {
        Some(Effect::Toggle) => state.invoke_player_toggle(),
        Some(Effect::Seek(seconds)) => state.invoke_player_seek(seconds as f32),
        Some(Effect::Close) => state.invoke_player_close(),
        None => {}
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Status {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NowPlaying {
    id: String,
    title: String,
    artist: String,
    duration: f64,
    position: f64,
    status: Status,
    speed: f64,
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            artist: String::new(),
            duration: 0.,
            position: 0.,
            status: Status::Stopped,
            speed: 1.,
        }
    }
}

impl NowPlaying {
    fn from_app(app: &AppWindow) -> Self {
        let state = app.global::<AppState>();
        if !state.get_player_open() || !state.get_player_error().is_empty() {
            return Self::default();
        }
        Self {
            id: state.get_player_video_id().into(),
            title: state.get_player_title().into(),
            artist: state.get_player_channel().into(),
            duration: f64::from(state.get_player_duration()).max(0.),
            position: f64::from(state.get_player_position()).max(0.),
            speed: f64::from(state.get_player_speed()),
            status: if state.get_player_loading()
                || state.get_player_paused()
                || state.get_player_ended()
            {
                Status::Paused
            } else {
                Status::Playing
            },
        }
    }

    fn art_url(&self) -> Option<String> {
        (!self.id.is_empty()).then(|| format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", self.id))
    }

    fn same_track(&self, other: &Self) -> bool {
        self.id == other.id
            && self.title == other.title
            && self.artist == other.artist
            && self.duration == other.duration
    }
}

thread_local! {
    // Only touched on the UI thread, which macOS requires for Now Playing.
    static MEDIA: RefCell<Option<platform::Backend>> = const { RefCell::new(None) };
}

/// Sets up what the OS backend needs before the first video plays.
pub(crate) fn init(runtime: tokio::runtime::Handle) {
    platform::init(runtime);
}

/// Pushes the player's current state to the OS. Must be called on the UI thread.
pub(crate) fn refresh(app: &AppWindow) {
    let now = NowPlaying::from_app(app);
    MEDIA.with(|media| {
        let mut media = media.borrow_mut();
        // Register lazily, once the window exists (Windows needs its handle).
        // A failed registration is retried when another video starts.
        let retry = media
            .as_ref()
            .is_some_and(|backend| !backend.alive() && backend.attempt() != now.id);
        if (media.is_none() || retry) && now.status != Status::Stopped {
            *media = match platform::Backend::new(app, &now.id) {
                Ok(backend) => Some(backend),
                Err(error) => {
                    eprintln!("Media controls unavailable: {error}");
                    None
                }
            };
        }
        if let Some(backend) = media.as_mut() {
            backend.update(&now);
        }
    });
}

/// Records regular playback progress. Discontinuous changes use `seeked`.
pub(crate) fn position(seconds: f64) {
    if seconds.is_finite() {
        MEDIA.with(|media| {
            if let Some(backend) = media.borrow_mut().as_mut() {
                backend.position(seconds.max(0.));
            }
        });
    }
}

/// Called only after mpv completes a seek, using its actual resulting position.
pub(crate) fn seeked(seconds: f64) {
    if seconds.is_finite() {
        MEDIA.with(|media| {
            if let Some(backend) = media.borrow_mut().as_mut() {
                backend.seeked(seconds.max(0.));
            }
        });
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use slint::ComponentHandle;
    use zbus::{
        interface,
        object_server::SignalEmitter,
        zvariant::{ObjectPath, OwnedValue, Value},
    };

    use super::{Action, NowPlaying, SKIP_SECONDS, Status};
    use crate::AppWindow;

    const PATH: &str = "/org/mpris/MediaPlayer2";
    const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

    thread_local! {
        static RUNTIME: std::cell::RefCell<Option<tokio::runtime::Handle>> =
            const { std::cell::RefCell::new(None) };
    }

    pub fn init(runtime: tokio::runtime::Handle) {
        RUNTIME.with(|slot| *slot.borrow_mut() = Some(runtime));
    }

    pub struct Backend {
        runtime: tokio::runtime::Handle,
        shared: Arc<Mutex<NowPlaying>>,
        connection: Arc<Mutex<Option<zbus::Connection>>>,
        failed: Arc<AtomicBool>,
        attempt: String,
        sent: NowPlaying,
    }

    impl Backend {
        pub fn new(app: &AppWindow, attempt: &str) -> Result<Self, String> {
            let runtime = RUNTIME
                .with(|slot| slot.borrow().clone())
                .ok_or("the async runtime is not ready")?;
            let shared = Arc::new(Mutex::new(NowPlaying::default()));
            let connection = Arc::new(Mutex::new(None));
            let failed = Arc::new(AtomicBool::new(false));
            let player = Player {
                app: app.as_weak(),
                shared: shared.clone(),
            };
            let (slot, flag) = (connection.clone(), failed.clone());
            runtime.spawn(async move {
                // Each instance gets its own name, so a dev build and an
                // installed copy can run side by side.
                let name = format!(
                    "org.mpris.MediaPlayer2.slintyt.instance{}",
                    std::process::id()
                );
                let result = async {
                    zbus::connection::Builder::session()?
                        .name(name)?
                        .serve_at(PATH, Root)?
                        .serve_at(PATH, player)?
                        .build()
                        .await
                }
                .await;
                match result {
                    Ok(connection) => *slot.lock().unwrap() = Some(connection),
                    Err(error) => {
                        eprintln!("Media controls unavailable: {error}");
                        flag.store(true, Ordering::Release);
                    }
                }
            });
            Ok(Self {
                runtime,
                shared,
                connection,
                failed,
                attempt: attempt.into(),
                sent: NowPlaying::default(),
            })
        }

        pub fn alive(&self) -> bool {
            !self.failed.load(Ordering::Acquire)
        }

        pub fn attempt(&self) -> &str {
            &self.attempt
        }

        pub fn position(&mut self, seconds: f64) {
            self.shared.lock().unwrap().position = seconds;
        }

        pub fn seeked(&mut self, seconds: f64) {
            self.position(seconds);
            let connection = self.connection.lock().unwrap().clone();
            if let Some(connection) = connection {
                self.runtime.spawn(async move {
                    if let Ok(iface) = connection
                        .object_server()
                        .interface::<_, Player>(PATH)
                        .await
                    {
                        let _ = Player::seeked(iface.signal_emitter(), micros(seconds)).await;
                    }
                });
            }
        }

        pub fn update(&mut self, now: &NowPlaying) {
            let speed = self.sent.speed != now.speed;
            let status = self.sent.status != now.status;
            let track = !self.sent.same_track(now);
            *self.shared.lock().unwrap() = now.clone();
            self.sent = now.clone();
            if !status && !track && !speed {
                return;
            }
            let connection = self.connection.lock().unwrap().clone();
            // Before the connection is up there is nobody to notify; clients
            // read the current properties when they first see the player.
            let Some(connection) = connection else { return };
            self.runtime.spawn(async move {
                let Ok(iface) = connection
                    .object_server()
                    .interface::<_, Player>(PATH)
                    .await
                else {
                    return;
                };
                let player = iface.get().await;
                let emitter = iface.signal_emitter();
                if status {
                    let _ = player.playback_status_changed(emitter).await;
                }
                if track {
                    let _ = player.metadata_changed(emitter).await;
                }
                if speed {
                    let _ = player.rate_changed(emitter).await;
                }
            });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn same_track_speed_changes_and_confirmed_seeks_reach_mpris() {
            let shared = Arc::new(Mutex::new(NowPlaying::default()));
            let player = Player {
                app: slint::Weak::default(),
                shared: shared.clone(),
            };
            // Exercise updates before a bus connection exists as well: a new
            // client must see the latest state when it eventually connects.
            let mut backend = Backend {
                runtime: tokio::runtime::Handle::current(),
                shared,
                connection: Arc::new(Mutex::new(None)),
                failed: Arc::new(AtomicBool::new(false)),
                attempt: "video".into(),
                sent: NowPlaying::default(),
            };
            let mut now = NowPlaying {
                id: "video".into(),
                status: Status::Playing,
                duration: 600.,
                ..NowPlaying::default()
            };
            backend.update(&now);
            let original_metadata = player.metadata();
            now.speed = 2.;
            backend.update(&now);
            assert_eq!(player.rate(), 2.);
            assert_eq!(player.metadata(), original_metadata);
            backend.seeked(123.5);
            assert_eq!(player.position(), 123_500_000);
            assert_eq!(player.playback_status(), "Playing");
            assert_eq!(player.rate(), 2.);
            backend.update(&NowPlaying::default());
            assert_eq!(player.playback_status(), "Stopped");
            assert_eq!(player.position(), 0);
        }
    }

    struct Root;

    #[interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        fn raise(&self) {}

        fn quit(&self) {}

        #[zbus(property)]
        fn can_quit(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_raise(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn has_track_list(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn identity(&self) -> &str {
            "SlintYT"
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "slintyt"
        }

        #[zbus(property)]
        fn supported_uri_schemes(&self) -> Vec<String> {
            Vec::new()
        }

        #[zbus(property)]
        fn supported_mime_types(&self) -> Vec<String> {
            Vec::new()
        }
    }

    struct Player {
        app: slint::Weak<AppWindow>,
        shared: Arc<Mutex<NowPlaying>>,
    }

    impl Player {
        fn send(&self, action: Action) {
            let _ = self
                .app
                .upgrade_in_event_loop(move |app| super::apply(&app, action));
        }

        fn snapshot(&self) -> NowPlaying {
            self.shared.lock().unwrap().clone()
        }
    }

    /// MPRIS track IDs are D-Bus object paths, which only allow [A-Za-z0-9_].
    pub(super) fn track_path(id: &str) -> String {
        if id.is_empty() {
            return NO_TRACK.into();
        }
        let escaped: String = id
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    (b as char).to_string()
                } else {
                    format!("_{b:02x}")
                }
            })
            .collect();
        format!("/org/slintyt/track/{escaped}")
    }

    pub(super) fn micros(seconds: f64) -> i64 {
        (seconds * 1_000_000.).round() as i64
    }

    pub(super) fn metadata(now: &NowPlaying) -> HashMap<String, OwnedValue> {
        let mut map = HashMap::new();
        let mut insert = |key: &str, value: Value<'_>| {
            if let Ok(value) = OwnedValue::try_from(value) {
                map.insert(key.to_owned(), value);
            }
        };
        let path = ObjectPath::try_from(track_path(&now.id))
            .unwrap_or_else(|_| ObjectPath::from_static_str_unchecked(NO_TRACK));
        insert("mpris:trackid", Value::from(path));
        if now.id.is_empty() {
            return map;
        }
        insert("xesam:title", Value::from(now.title.as_str()));
        if !now.artist.is_empty() {
            insert("xesam:artist", Value::from(vec![now.artist.as_str()]));
        }
        if now.duration > 0. {
            insert("mpris:length", Value::from(micros(now.duration)));
        }
        if let Some(art) = now.art_url() {
            insert("mpris:artUrl", Value::from(art));
        }
        map
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        fn next(&self) {
            self.send(Action::SeekBy(SKIP_SECONDS));
        }

        fn previous(&self) {
            self.send(Action::SeekBy(-SKIP_SECONDS));
        }

        fn pause(&self) {
            self.send(Action::Pause);
        }

        fn play_pause(&self) {
            self.send(Action::Toggle);
        }

        fn stop(&self) {
            self.send(Action::Stop);
        }

        fn play(&self) {
            self.send(Action::Play);
        }

        fn seek(&self, offset: i64) {
            let now = self.snapshot();
            let mut target = (now.position + offset as f64 / 1_000_000.).max(0.);
            if now.duration > 0. {
                target = target.min(now.duration);
            }
            self.send(Action::SeekTo(target));
        }

        fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
            let now = self.snapshot();
            // The spec says to ignore stale track IDs and out-of-range positions.
            if track_id.as_str() != track_path(&now.id)
                || position < 0
                || (now.duration > 0. && position > micros(now.duration))
            {
                return;
            }
            self.send(Action::SeekTo(position as f64 / 1_000_000.));
        }

        fn open_uri(&self, _uri: String) {}

        #[zbus(signal)]
        async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

        #[zbus(property)]
        fn playback_status(&self) -> String {
            match self.snapshot().status {
                Status::Playing => "Playing",
                Status::Paused => "Paused",
                Status::Stopped => "Stopped",
            }
            .into()
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            metadata(&self.snapshot())
        }

        #[zbus(property(emits_changed_signal = "false"))]
        fn position(&self) -> i64 {
            micros(self.snapshot().position)
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            self.snapshot().speed
        }

        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            0.25
        }

        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            3.
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }

        #[zbus(property(emits_changed_signal = "const"))]
        fn can_control(&self) -> bool {
            true
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod platform {
    use std::time::{Duration, Instant};

    use slint::ComponentHandle;
    use souvlaki::{
        MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition,
        PlatformConfig, SeekDirection,
    };

    use super::{Action, NowPlaying, SKIP_SECONDS, Status};
    use crate::AppWindow;

    pub fn init(_runtime: tokio::runtime::Handle) {}

    pub struct Backend {
        controls: MediaControls,
        attempt: String,
        sent: Option<NowPlaying>,
        progress_sent: Instant,
    }

    fn action(event: MediaControlEvent) -> Option<Action> {
        let signed = |direction: SeekDirection, seconds: f64| match direction {
            SeekDirection::Forward => seconds,
            SeekDirection::Backward => -seconds,
        };
        Some(match event {
            MediaControlEvent::Toggle => Action::Toggle,
            MediaControlEvent::Play => Action::Play,
            MediaControlEvent::Pause => Action::Pause,
            MediaControlEvent::Next => Action::SeekBy(SKIP_SECONDS),
            MediaControlEvent::Previous => Action::SeekBy(-SKIP_SECONDS),
            MediaControlEvent::Seek(direction) => Action::SeekBy(signed(direction, SKIP_SECONDS)),
            MediaControlEvent::SeekBy(direction, by) => {
                Action::SeekBy(signed(direction, by.as_secs_f64()))
            }
            MediaControlEvent::SetPosition(MediaPosition(at)) => Action::SeekTo(at.as_secs_f64()),
            MediaControlEvent::Stop => Action::Stop,
            _ => return None,
        })
    }

    impl Backend {
        pub fn new(app: &AppWindow, attempt: &str) -> Result<Self, String> {
            #[cfg(windows)]
            let hwnd = {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                let handle = app.window().window_handle();
                let handle = handle.window_handle().map_err(|e| e.to_string())?;
                match handle.as_raw() {
                    RawWindowHandle::Win32(win32) => {
                        Some(win32.hwnd.get() as *mut std::ffi::c_void)
                    }
                    _ => return Err("the window has no Win32 handle".into()),
                }
            };
            #[cfg(not(windows))]
            let hwnd = None;
            let mut controls = MediaControls::new(PlatformConfig {
                display_name: "SlintYT",
                dbus_name: "slintyt",
                hwnd,
            })
            .map_err(|e| format!("{e:?}"))?;
            let weak = app.as_weak();
            controls
                .attach(move |event| {
                    if let Some(action) = action(event) {
                        let _ = weak.upgrade_in_event_loop(move |app| super::apply(&app, action));
                    }
                })
                .map_err(|e| format!("{e:?}"))?;
            Ok(Self {
                controls,
                attempt: attempt.into(),
                sent: None,
                progress_sent: Instant::now(),
            })
        }

        pub fn alive(&self) -> bool {
            true
        }

        pub fn attempt(&self) -> &str {
            &self.attempt
        }

        pub fn position(&mut self, seconds: f64) {
            // Keep native timelines anchored even at non-default playback speeds.
            // Souvlaki has no rate API, so bound drift with periodic corrections.
            if self.progress_sent.elapsed() >= Duration::from_millis(250) {
                self.seeked(seconds);
            }
        }

        pub fn seeked(&mut self, seconds: f64) {
            if let Some(mut now) = self.sent.clone() {
                now.position = seconds;
                self.update(&now);
            }
        }

        pub fn update(&mut self, now: &NowPlaying) {
            // Resending unchanged metadata would refetch the artwork.
            if self.sent.as_ref().is_none_or(|sent| !sent.same_track(now)) {
                let art = now.art_url();
                let _ = self.controls.set_metadata(MediaMetadata {
                    title: Some(now.title.as_str()).filter(|t| !t.is_empty()),
                    artist: Some(now.artist.as_str()).filter(|a| !a.is_empty()),
                    cover_url: art.as_deref(),
                    duration: (now.duration > 0.).then(|| Duration::from_secs_f64(now.duration)),
                    ..Default::default()
                });
            }
            let progress = Some(MediaPosition(Duration::from_secs_f64(now.position)));
            let _ = self.controls.set_playback(match now.status {
                Status::Playing => MediaPlayback::Playing { progress },
                Status::Paused => MediaPlayback::Paused { progress },
                Status::Stopped => MediaPlayback::Stopped,
            });
            self.sent = Some(now.clone());
            self.progress_sent = Instant::now();
        }
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod platform {
    use super::NowPlaying;
    use crate::AppWindow;

    pub fn init(_runtime: tokio::runtime::Handle) {}

    pub struct Backend;

    impl Backend {
        pub fn new(_app: &AppWindow, _attempt: &str) -> Result<Self, String> {
            Err("not supported on this platform".into())
        }
        pub fn alive(&self) -> bool {
            false
        }
        pub fn attempt(&self) -> &str {
            ""
        }
        pub fn position(&mut self, _seconds: f64) {}
        pub fn seeked(&mut self, _seconds: f64) {}
        pub fn update(&mut self, _now: &NowPlaying) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(position: f64, duration: f64) -> Player {
        Player {
            open: true,
            paused: false,
            position,
            duration,
        }
    }

    #[test]
    fn play_and_pause_only_act_when_the_state_differs() {
        let paused = Player {
            paused: true,
            ..playing(5., 60.)
        };
        assert_eq!(decide(Action::Play, paused), Some(Effect::Toggle));
        assert_eq!(decide(Action::Pause, paused), None);
        assert_eq!(
            decide(Action::Pause, playing(5., 60.)),
            Some(Effect::Toggle)
        );
        assert_eq!(decide(Action::Play, playing(5., 60.)), None);
        assert_eq!(decide(Action::Toggle, paused), Some(Effect::Toggle));
    }

    #[test]
    fn skips_and_seeks_stay_inside_the_video() {
        let player = playing(5., 60.);
        assert_eq!(
            decide(Action::SeekBy(SKIP_SECONDS), player),
            Some(Effect::Seek(15.))
        );
        assert_eq!(
            decide(Action::SeekBy(-SKIP_SECONDS), player),
            Some(Effect::Seek(0.))
        );
        assert_eq!(decide(Action::SeekTo(90.), player), Some(Effect::Seek(60.)));
        // Live streams report no duration, so only the start is a bound.
        assert_eq!(
            decide(Action::SeekBy(30.), playing(5., 0.)),
            Some(Effect::Seek(35.))
        );
        assert_eq!(decide(Action::SeekTo(f64::NAN), player), None);
    }

    #[test]
    fn a_closed_player_ignores_every_action() {
        for action in [
            Action::Toggle,
            Action::Play,
            Action::Pause,
            Action::SeekBy(10.),
            Action::SeekTo(1.),
            Action::Stop,
        ] {
            assert_eq!(decide(action, Player::default()), None);
        }
        assert_eq!(decide(Action::Stop, playing(0., 0.)), Some(Effect::Close));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mpris_metadata_uses_valid_track_paths_and_microseconds() {
        use platform::{metadata, micros, track_path};
        assert_eq!(track_path("ab-C_1"), "/org/slintyt/track/ab_2dC_5f1");
        assert!(zbus::zvariant::ObjectPath::try_from(track_path("x-_y")).is_ok());
        assert_eq!(track_path(""), "/org/mpris/MediaPlayer2/TrackList/NoTrack");
        assert_eq!(micros(1.5), 1_500_000);
        let now = NowPlaying {
            id: "dQw4w9WgXcQ".into(),
            title: "Title".into(),
            artist: "Channel".into(),
            duration: 212.,
            position: 3.,
            status: Status::Playing,
            speed: 1.,
        };
        let map = metadata(&now);
        for key in [
            "mpris:trackid",
            "xesam:title",
            "xesam:artist",
            "mpris:length",
            "mpris:artUrl",
        ] {
            assert!(map.contains_key(key), "{key}");
        }
        let stopped = metadata(&NowPlaying::default());
        assert_eq!(stopped.len(), 1);
    }
}
