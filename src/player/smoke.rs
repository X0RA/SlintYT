//! Opt-in native playback check. Browser access requires SLINTYT_TEST_PROFILE.
use super::*;
use glow::HasContext;
use std::{
    cell::Cell,
    num::NonZeroU32,
    rc::Rc,
    time::{Duration, Instant},
};

type Frame = (u32, u32, [u8; 4]);
static FRAMES: Mutex<Vec<Frame>> = Mutex::new(Vec::new());

pub(super) fn record_frame(texture: &gl::Texture) {
    let mut pixel = [0; 4];
    unsafe {
        let gl = &texture.gl;
        let previous = NonZeroU32::new(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32)
            .map(glow::NativeFramebuffer);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(texture.fbo));
        gl.read_pixels(
            (texture.width / 2) as i32,
            (texture.height / 2) as i32,
            1,
            1,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut pixel)),
        );
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, previous);
    }
    FRAMES
        .lock()
        .unwrap()
        .push((texture.width, texture.height, pixel));
}

#[test]
#[ignore = "requires an OpenGL desktop, libmpv and ffmpeg"]
fn native_video_playback_controls_resize_and_close() {
    // Optional end-to-end pass uses YouTube, with no browser access unless an
    // explicitly authorized profile is supplied.
    let public_video = std::env::var("SLINTYT_TEST_VIDEO").ok();
    let online = public_video.is_some();
    let progress_check = std::env::var_os("SLINTYT_TEST_PROGRESS").is_some();
    let captions_check = std::env::var_os("SLINTYT_TEST_CAPTIONS").is_some();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .require_opengl()
        .select()
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let sample = directory.path().join("sample.mp4");
    let captions = directory.path().join("captions.vtt");
    std::fs::write(
        &captions,
        "WEBVTT\n\n00:00:00.000 --> 00:00:06.000\nLocal subtitle check\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=320x180:r=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "6",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-y"
            ])
            .arg(&sample)
            .status()
            .unwrap()
            .success()
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let app = AppWindow::new().unwrap();
    let player = install(&app, runtime.handle().clone());
    let engine = player.engine.clone().expect("libmpv initialization failed");
    let (cookies, progress) = if online && let Ok(profile) = std::env::var("SLINTYT_TEST_PROFILE") {
        let (pipe, cookies, identity) = runtime
            .block_on(crate::youtube::authenticated_client(
                &crate::browser::Selection {
                    browser: "chromium".into(),
                    profile: profile.into(),
                    keyring: None,
                },
            ))
            .expect("Native authentication failed");
        let progress =
            progress::Client::new(pipe, &cookies, &identity).expect("Playback client failed");
        (cookies, Some(progress))
    } else {
        ("# Netscape HTTP Cookie File\n".into(), None)
    };
    let (seek_target, resume_position) = if progress_check {
        let info = runtime
            .block_on(
                progress
                    .as_ref()
                    .expect("Authorized profile required")
                    .info(public_video.as_ref().unwrap(), true),
            )
            .unwrap();
        assert!(
            info.duration > 1200.,
            "Use a long video that supports YouTube resume"
        );
        (
            (info.resume.unwrap_or(37.) + 100.).min(info.duration - 60.) as f32,
            info.resume.unwrap_or(0.) as f32,
        )
    } else {
        (3., 0.)
    };
    player.set_cookies(Arc::from(cookies));
    *player.progress.write().unwrap() = progress.clone();
    let final_position = Rc::new(Cell::new(0.));
    engine.set_property("ao", "null").unwrap();
    let state = app.global::<AppState>();
    state.set_initializing(false);
    state.set_player_open(true);
    state.set_player_loading(true);
    state.set_player_title("Local GPU playback check".into());
    let stage = Rc::new(Cell::new(0));
    let next_calls = Rc::new(Cell::new(0));
    let next_count = next_calls.clone();
    state.on_player_next(move || next_count.set(next_count.get() + 1));
    let checkpoint = Rc::new(Cell::new(Instant::now()));
    let expected_texture = Rc::new(Cell::new((0u32, 0u32)));
    let started = Instant::now();
    let resume_checked = Cell::new(false);
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), {
        let (stage, checkpoint, player) = (stage.clone(), checkpoint.clone(), player.clone());
        let expected_texture = expected_texture.clone();
        let final_position = final_position.clone();
        move || {
            let app = weak.upgrade().unwrap();
            let state = app.global::<AppState>();
            if !state.get_player_error().is_empty()
                || started.elapsed() > Duration::from_secs(if online { 100 } else { 15 })
            {
                eprintln!(
                    "Playback stalled at stage {}, ready {}, position {}, error {}",
                    stage.get(),
                    player.ready.load(Ordering::Acquire),
                    state.get_player_position(),
                    state.get_player_error()
                );
                app.hide().unwrap();
                slint::quit_event_loop().unwrap();
                return;
            }
            if progress_check
                && stage.get() == 1
                && !state.get_player_loading()
                && commands::AUDIO_CHANNELS.load(Ordering::Acquire) > 0
                && !resume_checked.get()
            {
                let position = engine.get_property::<f64>("time-pos").unwrap();
                assert!(
                    (position - f64::from(resume_position)).abs() < 3.,
                    "mpv did not restore the YouTube resume point"
                );
                resume_checked.set(true);
            }
            match stage.get() {
                0 if player.ready.load(Ordering::Acquire) => {
                    if let Some(id) = &public_video {
                        state.invoke_play_video(
                            id.as_str().into(),
                            "Public YouTube playback check".into(),
                        );
                    } else {
                        player
                            .commands
                            .as_ref()
                            .unwrap()
                            .send(Command::Load {
                                epoch: player.epoch.load(Ordering::Acquire),
                                source: sample.to_str().unwrap().into(),
                                user_agent: String::new(),
                                referer: String::new(),
                                position: 0.,
                                tracking: None,
                                speed: 1.5,
                                paused: false,
                            })
                            .unwrap();
                    }
                    stage.set(1);
                }
                1 if !state.get_player_loading()
                    && commands::AUDIO_CHANNELS.load(Ordering::Acquire) > 0
                    && state.get_player_position()
                        > resume_position + if progress_check { 22. } else { 0.5 }
                    && (!captions_check || !state.get_player_subtitles_loading()) =>
                {
                    assert!(commands::AUDIO_CHANNELS.load(Ordering::Acquire) > 0);
                    if !online {
                        assert_eq!(engine.get_property::<f64>("speed").unwrap(), 1.5);
                        state.invoke_player_set_speed(2.);
                        *player.subtitle_choices.write().unwrap() = vec![subtitles::Track {
                            key: "uploaded:en".into(),
                            label: "English [en] · uploaded".into(),
                            language: "en".into(),
                            url: captions.to_str().unwrap().into(),
                        }];
                        state.set_player_subtitles(slint::ModelRc::new(slint::VecModel::from(
                            vec!["Off".into(), "English [en] · uploaded".into()],
                        )));
                        state.invoke_player_select_subtitle(1);
                    }
                    if captions_check {
                        let index = player
                            .subtitle_choices
                            .read()
                            .unwrap()
                            .iter()
                            .position(|t| t.language == "en")
                            .expect("English captions unavailable");
                        state.invoke_player_select_subtitle(index as i32 + 1);
                    }
                    state.invoke_player_toggle();
                    checkpoint.set(Instant::now());
                    stage.set(2);
                }
                2 if state.get_player_paused()
                    && checkpoint.get().elapsed() > Duration::from_millis(350)
                    && !state.get_player_subtitles_loading() =>
                {
                    if !online {
                        assert_eq!(engine.get_property::<f64>("speed").unwrap(), 2.);
                        assert_eq!(state.get_player_subtitle_index(), 1);
                        assert!(
                            engine
                                .get_property::<String>("sub-text")
                                .unwrap()
                                .contains("Local subtitle check")
                        );
                        assert!(engine.get_property::<bool>("sub-visibility").unwrap());
                        state.invoke_player_select_subtitle(0);
                    }
                    if captions_check {
                        assert!(
                            state.get_player_subtitle_index() > 0,
                            "Remote captions did not load"
                        );
                        assert!(engine.get_property::<bool>("sub-visibility").unwrap());
                    }
                    state.invoke_player_seek(seek_target);
                    state.invoke_player_volume_changed(25.);
                    app.window().set_size(slint::LogicalSize::new(900., 650.));
                    checkpoint.set(Instant::now());
                    stage.set(3);
                }
                3 if state.get_player_position() >= seek_target - 0.1
                    && (state.get_player_volume() - 25.).abs() < 0.1
                    && checkpoint.get().elapsed() > Duration::from_millis(250) =>
                {
                    if !online {
                        assert_eq!(state.get_player_subtitle_index(), 0);
                        assert_eq!(engine.get_property::<String>("sid").unwrap(), "no");
                        // A bad caption must not turn into a video failure.
                        player.subtitle_choices.write().unwrap()[0].url = captions
                            .with_file_name("missing.vtt")
                            .to_str()
                            .unwrap()
                            .into();
                        state.invoke_player_select_subtitle(1);
                    }
                    if captions_check {
                        assert!(
                            !engine
                                .get_property::<String>("sub-text")
                                .unwrap()
                                .is_empty(),
                            "No rendered remote caption text"
                        );
                        state.invoke_player_select_subtitle(0);
                    }
                    state.invoke_player_toggle();
                    stage.set(4);
                }
                4 if state.get_player_position() > seek_target + 0.5 => {
                    if !online {
                        assert!(
                            state
                                .get_player_subtitle_note()
                                .contains("Could not load subtitles")
                        );
                        assert_eq!(state.get_player_subtitle_index(), 0);
                        assert!(state.get_player_error().is_empty());
                    }
                    let scale = app.window().scale_factor();
                    expected_texture.set((
                        (state.get_player_width() * scale) as u32,
                        (state.get_player_height() * scale) as u32,
                    ));
                    final_position.set(state.get_player_position());
                    state.invoke_player_close();
                    stage.set(5);
                }
                5 if commands::IDLE.load(Ordering::Acquire) => {
                    assert!(!state.get_player_open());
                    player.set_cookies(Arc::from("# Netscape HTTP Cookie File\n"));
                    state.invoke_play_video("invalid".into(), "Invalid video".into());
                    assert!(state.get_player_error().contains("invalid YouTube ID"));
                    state.invoke_player_close();
                    let old_epoch = player.epoch.load(Ordering::Acquire);
                    // Queue old file events, then start a new session before
                    // those callbacks get a chance to run on Slint's thread.
                    for update in [
                        commands::Update::Error(libmpv2::Error::Raw(-13), None),
                        commands::Update::Ended(true),
                        commands::Update::Position(999.),
                    ] {
                        commands::publish(&app.as_weak(), player.epoch.clone(), old_epoch, update);
                    }
                    player.cancel();
                    state.set_player_open(true);
                    state.set_player_error("".into());
                    state.set_player_loading(true);
                    state.set_player_position(0.);
                    state.set_player_ended(false);
                    player
                        .commands
                        .as_ref()
                        .unwrap()
                        .send(Command::Load {
                            epoch: player.epoch.load(Ordering::Acquire),
                            source: sample.to_str().unwrap().into(),
                            user_agent: String::new(),
                            referer: String::new(),
                            position: 0.,
                            tracking: None,
                            speed: 1.,
                            paused: false,
                        })
                        .unwrap();
                    stage.set(6);
                }
                6 if !state.get_player_loading() && state.get_player_position() > 0.3 => {
                    assert_eq!(engine.get_property::<f64>("speed").unwrap(), 1.);
                    assert!(state.get_player_error().is_empty());
                    assert!(!state.get_player_ended());
                    assert!(state.get_player_position() < 5.);
                    state.set_player_playlist_index(0);
                    state.set_playback_autoplay(false);
                    commands::publish(
                        &app.as_weak(),
                        player.epoch.clone(),
                        player.epoch.load(Ordering::Acquire),
                        commands::Update::Ended(true),
                    );
                    stage.set(7);
                }
                7 if state.get_player_ended() => {
                    assert_eq!(next_calls.get(), 0, "Autoplay off must stop at EOF");
                    state.set_player_ended(false);
                    state.set_playback_autoplay(true);
                    for _ in 0..2 {
                        commands::publish(
                            &app.as_weak(),
                            player.epoch.clone(),
                            player.epoch.load(Ordering::Acquire),
                            commands::Update::Ended(true),
                        );
                    }
                    stage.set(8);
                }
                8 if state.get_player_ended() => {
                    assert_eq!(next_calls.get(), 1, "Repeated EOF must advance only once");
                    state.set_player_ended(false);
                    state.set_player_playlist_index(-1);
                    commands::publish(
                        &app.as_weak(),
                        player.epoch.clone(),
                        player.epoch.load(Ordering::Acquire),
                        commands::Update::Ended(true),
                    );
                    stage.set(9);
                }
                9 if state.get_player_ended() => {
                    assert_eq!(next_calls.get(), 1, "Autoplay only advances playlists");
                    stage.set(10);
                    // Exercise render-context teardown while video is active.
                    app.hide().unwrap();
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        }
    });
    app.run().unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), player.shutdown())
            .await
            .expect("Final reporting timed out");
        if progress_check {
            let client = progress.expect("Authorized account required for progress verification");
            let video = std::env::var("SLINTYT_TEST_VIDEO").unwrap();
            let mut saved = None;
            let matches = |saved: f64| {
                let rewind = f64::from(final_position.get()) - saved;
                // YouTube can return a 20-second preroll in playbackStartConfig.
                rewind.abs() < 3. || (rewind - 20.).abs() < 3.
            };
            for _ in 0..5 {
                let info = client
                    .info(&video, true)
                    .await
                    .expect("Resume lookup failed");
                saved = info.resume;
                if saved.is_some_and(matches) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            println!(
                "Final player position {:.1}; YouTube resume {:?}",
                final_position.get(),
                saved
            );
            assert!(
                saved.is_some_and(matches),
                "YouTube did not preserve the final position"
            );
        }
    });
    assert_eq!(
        stage.get(),
        10,
        "native player stalled: {}",
        app.global::<AppState>().get_player_error()
    );
    let frames = FRAMES.lock().unwrap();
    let visible: Vec<_> = frames
        .iter()
        .filter(|(_, _, rgba)| {
            if online {
                rgba[..3].iter().any(|value| *value > 30)
            } else {
                rgba[0] > 180 && rgba[1] < 60 && rgba[2] < 60
            }
        })
        .collect();
    assert!(
        !visible.is_empty(),
        "mpv did not render the video into its GPU texture"
    );
    assert!(
        visible
            .iter()
            .any(|(width, height, _)| (*width, *height) == expected_texture.get()),
        "video texture did not follow the player area's physical pixel size {:?}",
        expected_texture.get()
    );
}
