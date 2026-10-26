//! Saved defaults for new videos. Per-video controls do not overwrite these.
use crate::{AppState, AppWindow, config::Store};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use std::sync::{Arc, RwLock};

pub const HEIGHTS: [Option<u16>; 5] = [Some(1080), None, Some(2160), Some(1440), Some(720)];
pub const SPEEDS: [f32; 5] = [1., 0.75, 1.25, 1.5, 2.];

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    #[default]
    Vp9,
    Av1,
    H264,
}
pub const CODECS: [Codec; 3] = [Codec::Vp9, Codec::Av1, Codec::H264];

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subtitles {
    #[default]
    Off,
    English,
    Automatic,
}
pub const SUBTITLES: [Subtitles; 3] = [Subtitles::Off, Subtitles::English, Subtitles::Automatic];

#[derive(Clone)]
pub struct StreamSelection {
    pub format: String,
    pub sort: String,
}

impl Default for StreamSelection {
    fn default() -> Self {
        Self {
            format: "bestvideo+bestaudio/best".into(),
            sort: String::new(),
        }
    }
}

impl StreamSelection {
    pub fn cache_key(&self) -> String {
        format!("{};{}", self.format, self.sort)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub max_height: Option<u16>,
    pub codec: Codec,
    pub speed: f32,
    pub subtitles: Subtitles,
    pub resume: bool,
    pub autoplay: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            max_height: Some(1080),
            codec: Codec::Vp9,
            speed: 1.,
            subtitles: Subtitles::Off,
            resume: true,
            autoplay: false,
        }
    }
}

impl Preferences {
    pub fn normalize(&mut self) {
        if !HEIGHTS.contains(&self.max_height) {
            self.max_height = Self::default().max_height;
        }
        if !SPEEDS.contains(&self.speed) {
            self.speed = 1.;
        }
    }

    /// Prefer the codec family among streams that fit the resolution limit.
    /// A portrait rendition fits by width; a landscape rendition fits by height.
    /// If none fit, res:N chooses the smallest available resolution above N.
    pub fn selection(&self) -> StreamSelection {
        let (filter, codec) = match self.codec {
            Codec::Vp9 => ("[vcodec~='^vp0?9']", "vcodec:vp9.2"),
            Codec::Av1 => ("[vcodec~='^av0?1']", "vcodec:av01"),
            Codec::H264 => ("[vcodec~='^(avc|h264)']", "vcodec:h264"),
        };
        let mut choices = Vec::new();
        for codec_filter in [filter, ""] {
            if let Some(h) = self.max_height {
                for (dimension, aspect) in [("height", ">=?1"), ("width", "<?1")] {
                    let eligible =
                        format!("[aspect_ratio{aspect}][{dimension}<={h}]{codec_filter}");
                    choices.push(format!("bestvideo{eligible}+bestaudio/best{eligible}"));
                }
            } else {
                choices.push(format!(
                    "bestvideo{codec_filter}+bestaudio/best{codec_filter}"
                ));
            }
        }
        choices.push("bestvideo+bestaudio/best".into());
        StreamSelection {
            format: choices.join("/"),
            sort: match self.max_height {
                Some(h) => format!("res:{h},{codec}"),
                None => format!("res,{codec}"),
            },
        }
    }

    /// A short notice when yt-dlp had to pick another codec, or None when the
    /// preferred one was used or the codec is unknown.
    pub fn codec_fallback(&self, vcodec: &str) -> Option<String> {
        let picked = codec_name(vcodec)?;
        let wanted = match self.codec {
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
            Codec::H264 => "H.264",
        };
        (picked != wanted).then(|| {
            format!("No {wanted} stream fits your playback preferences. Playing {picked} instead.")
        })
    }

    fn publish(&self, app: &AppWindow) {
        let state = app.global::<AppState>();
        state.set_playback_quality(
            HEIGHTS
                .iter()
                .position(|v| v == &self.max_height)
                .unwrap_or(0) as i32,
        );
        state.set_playback_codec(CODECS.iter().position(|v| v == &self.codec).unwrap_or(0) as i32);
        state.set_playback_speed(SPEEDS.iter().position(|v| v == &self.speed).unwrap_or(0) as i32);
        state.set_playback_subtitles(
            SUBTITLES
                .iter()
                .position(|v| v == &self.subtitles)
                .unwrap_or(0) as i32,
        );
        state.set_playback_resume(self.resume);
        state.set_playback_autoplay(self.autoplay);
    }

    fn change(&mut self, setting: i32, value: i32) -> Option<()> {
        let index = usize::try_from(value).ok()?;
        match setting {
            0 => self.max_height = *HEIGHTS.get(index)?,
            1 => self.codec = *CODECS.get(index)?,
            2 => self.speed = *SPEEDS.get(index)?,
            3 => self.subtitles = *SUBTITLES.get(index)?,
            4 if index <= 1 => self.resume = index == 1,
            5 if index <= 1 => self.autoplay = index == 1,
            _ => return None,
        }
        Some(())
    }
}

/// yt-dlp reports VP9 as both `vp9` and `vp09.*`, and H.264 as `avc1.*`.
fn codec_name(vcodec: &str) -> Option<&'static str> {
    let vcodec = vcodec.to_ascii_lowercase();
    if vcodec.starts_with("vp9") || vcodec.starts_with("vp09") {
        Some("VP9")
    } else if vcodec.starts_with("av01") {
        Some("AV1")
    } else if vcodec.starts_with("avc") || vcodec.starts_with("h264") {
        Some("H.264")
    } else if vcodec.starts_with("hev") || vcodec.starts_with("hvc") || vcodec.starts_with("h265") {
        Some("H.265")
    } else if vcodec.starts_with("vp8") {
        Some("VP8")
    } else {
        None
    }
}

pub fn wire(app: &AppWindow) -> Arc<RwLock<Preferences>> {
    let preferences = match Store::new().and_then(|s| s.load_settings()) {
        Ok(settings) => settings.playback,
        Err(_) => {
            app.global::<AppState>().set_playback_settings_error(
                "Could not read playback settings. Using defaults.".into(),
            );
            Preferences::default()
        }
    };
    preferences.publish(app);
    let preferences = Arc::new(RwLock::new(preferences));
    let (weak, saved) = (app.as_weak(), preferences.clone());
    app.global::<AppState>()
        .on_playback_setting_changed(move |setting, value| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let mut next = saved.read().unwrap().clone();
            if next.change(setting, value).is_none() {
                return;
            }
            let state = app.global::<AppState>();
            match Store::new().and_then(|s| s.save_playback(&next)) {
                Ok(()) => {
                    next.publish(&app);
                    *saved.write().unwrap() = next;
                    state.set_playback_settings_error("".into());
                }
                Err(_) => {
                    saved.read().unwrap().publish(&app);
                    state.set_playback_settings_error(
                        "Could not save playback settings. Please try again.".into(),
                    );
                }
            }
        });
    preferences
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_fallback_names_both_codecs_only_when_they_differ() {
        let vp9 = Preferences::default();
        assert_eq!(vp9.codec_fallback("vp9"), None);
        assert_eq!(vp9.codec_fallback("vp09.00.40.08"), None);
        assert_eq!(vp9.codec_fallback("vp9.2"), None);
        assert_eq!(vp9.codec_fallback("vp09.02.51.10"), None);
        assert_eq!(
            vp9.codec_fallback("avc1.640028").as_deref(),
            Some("No VP9 stream fits your playback preferences. Playing H.264 instead.")
        );
        let av1 = Preferences {
            codec: Codec::Av1,
            ..Preferences::default()
        };
        assert_eq!(av1.codec_fallback("av01.0.08M.08"), None);
        assert!(av1.codec_fallback("vp9").unwrap().contains("Playing VP9"));
        // Unknown or missing codecs never produce a notice.
        assert_eq!(av1.codec_fallback(""), None);
        assert_eq!(av1.codec_fallback("none"), None);
        assert_eq!(
            Preferences::default().selection().sort,
            "res:1080,vcodec:vp9.2"
        );
        assert_eq!(
            Preferences {
                max_height: None,
                codec: Codec::H264,
                ..Preferences::default()
            }
            .selection()
            .sort,
            "res,vcodec:h264"
        );
    }

    #[test]
    #[ignore = "requires yt-dlp on PATH; uses only local format fixtures, no network or account"]
    fn yt_dlp_selects_preferred_formats_and_falls_back() {
        use serde_json::json;
        let directory = tempfile::tempdir().unwrap();
        let info = directory.path().join("video.json");
        // Codec strings match what yt-dlp reports for YouTube: DASH VP9 is
        // `vp9`, HLS VP9 is `vp09.*`.
        let video = |id: &str, codec: &str, size: Option<(u16, u16)>, audio: &str| {
            json!({
                "format_id":id, "url":format!("https://example.invalid/{id}.mp4"),
                "vcodec":codec, "acodec":audio, "ext":"mp4",
                "width":size.map(|s| s.0), "height":size.map(|s| s.1)
            })
        };
        let audio = json!({"format_id":"audio", "url":"https://example.invalid/audio.m4a", "vcodec":"none", "acodec":"opus", "ext":"webm"});
        let landscape = vec![
            audio.clone(),
            video("av1-2160", "av01.0.12M.08", Some((3840, 2160)), "none"),
            video("vp9-1440", "vp9", Some((2560, 1440)), "none"),
            video("vp9-1080", "vp9", Some((1920, 1080)), "none"),
            video("vp9-720", "vp9", Some((1280, 720)), "none"),
            video("av1-1080", "av01.0.08M.08", Some((1920, 1080)), "none"),
            video("h264-1080", "avc1.640028", Some((1920, 1080)), "none"),
            video("h264-720", "avc1.4D401F", Some((1280, 720)), "none"),
        ];
        let vertical = vec![
            audio.clone(),
            video("vp09-1920", "vp09.00.40.08", Some((1080, 1920)), "none"),
            video("vp09-1280", "vp09.00.31.08", Some((720, 1280)), "none"),
            video("vp09-1080", "vp09.00.30.08", Some((608, 1080)), "none"),
            video("av1-1920", "av01.0.08M.08", Some((1080, 1920)), "none"),
        ];
        let hls_only = vec![
            audio.clone(),
            video("vp09-1080", "vp09.00.40.08", Some((1920, 1080)), "none"),
            video("av1-1080", "av01.0.08M.08", Some((1920, 1080)), "none"),
            video("h264-1080", "avc1.640028", Some((1920, 1080)), "none"),
        ];
        let no_av1 = vec![
            audio.clone(),
            video("vp9-1080", "vp9", Some((1920, 1080)), "none"),
            video("h264-1080", "avc1.640028", Some((1920, 1080)), "none"),
        ];
        let av1 = Preferences {
            codec: Codec::Av1,
            ..Preferences::default()
        };
        let cases = [
            (Preferences::default(), landscape.clone(), "vp9-1080+audio"),
            (av1.clone(), landscape.clone(), "av1-1080+audio"),
            (
                Preferences {
                    max_height: None,
                    ..av1.clone()
                },
                landscape.clone(),
                "av1-2160+audio",
            ),
            // Codec comes before resolution: VP9 1440p beats AV1 2160p.
            (
                Preferences {
                    max_height: Some(2160),
                    ..Preferences::default()
                },
                landscape.clone(),
                "vp9-1440+audio",
            ),
            (
                Preferences {
                    max_height: Some(720),
                    ..Preferences::default()
                },
                landscape.clone(),
                "vp9-720+audio",
            ),
            (
                Preferences {
                    codec: Codec::H264,
                    ..Preferences::default()
                },
                landscape,
                "h264-1080+audio",
            ),
            // The cap applies to the smaller dimension of vertical videos.
            (Preferences::default(), vertical.clone(), "vp09-1920+audio"),
            (
                Preferences {
                    max_height: Some(720),
                    ..Preferences::default()
                },
                vertical,
                "vp09-1280+audio",
            ),
            (Preferences::default(), hls_only, "vp09-1080+audio"),
            // A missing preferred codec falls back rather than failing.
            (av1.clone(), no_av1, "vp9-1080+audio"),
            // The preferred codec must not bypass an available capped stream.
            (
                av1.clone(),
                vec![
                    audio.clone(),
                    video("av1-2160", "av01.0.12M.08", Some((3840, 2160)), "none"),
                    video("vp9-1080", "vp9", Some((1920, 1080)), "none"),
                ],
                "vp9-1080+audio",
            ),
            (
                av1.clone(),
                vec![
                    audio.clone(),
                    video("av1-2160", "av01.0.12M.08", Some((3840, 2160)), "none"),
                    video("muxed", "avc1", Some((1920, 1080)), "aac"),
                ],
                "muxed",
            ),
            // Both DASH and HLS profile 2 belong to the preferred VP9 family.
            (
                Preferences::default(),
                vec![
                    audio.clone(),
                    video("vp9-hdr", "vp9.2", Some((1920, 1080)), "none"),
                    video("h264-1080", "avc1.640028", Some((1920, 1080)), "none"),
                ],
                "vp9-hdr+audio",
            ),
            (
                Preferences::default(),
                vec![
                    audio.clone(),
                    video("vp09-hdr", "vp09.02.51.10", Some((1080, 1920)), "none"),
                    video("h264-1080", "avc1.640028", Some((1080, 1920)), "none"),
                ],
                "vp09-hdr+audio",
            ),
            // Resolution wins between profiles of the same codec family.
            (
                Preferences::default(),
                vec![
                    audio.clone(),
                    video("vp9-hdr-720", "vp9.2", Some((1280, 720)), "none"),
                    video("vp9-1080", "vp9", Some((1920, 1080)), "none"),
                ],
                "vp9-1080+audio",
            ),
            // No format fits: preserve playback with the smallest above the cap.
            (
                av1.clone(),
                vec![
                    audio.clone(),
                    video("av1-2160", "av01.0.12M.08", Some((3840, 2160)), "none"),
                    video("vp9-1440", "vp9", Some((2560, 1440)), "none"),
                ],
                "vp9-1440+audio",
            ),
            (
                Preferences::default(),
                vec![video("muxed", "avc1", Some((640, 360)), "aac")],
                "muxed",
            ),
            (
                Preferences::default(),
                vec![audio, video("live", "vp9", None, "none")],
                "live+audio",
            ),
        ];
        for (preferences, formats, expected) in cases {
            let selection = preferences.selection();
            std::fs::write(&info, serde_json::to_vec(&json!({
                "id":"abcdefghijk", "title":"Offline format fixture", "formats":formats,
                "extractor":"youtube", "webpage_url":"https://www.youtube.com/watch?v=abcdefghijk"
            })).unwrap()).unwrap();
            let output = std::process::Command::new("yt-dlp")
                .args([
                    "--ignore-config",
                    "--no-plugin-dirs",
                    "--simulate",
                    "--no-check-formats",
                    "--print",
                    "format_id",
                    "--format",
                    &selection.format,
                    "--format-sort",
                    &selection.sort,
                    "--load-info-json",
                ])
                .arg(&info)
                .output()
                .expect("yt-dlp must be on PATH");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                expected,
                "{}",
                selection.cache_key()
            );
        }
    }
}
