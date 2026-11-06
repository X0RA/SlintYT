//! Account-scoped, bounded, memory-only stream catalogues. Signed URLs never
//! enter UI models, logs, or the on-disk watch-history cache.
use super::resolve::Media;
use super::storyboard::{self, Storyboard};
use super::subtitles;
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Quality {
    pub key: String,
    /// The menu row's title, and the line of detail under it.
    pub title: String,
    pub detail: String,
    pub media: Option<Media>,
    pub unavailable: String,
}

#[derive(Clone, Default)]
pub struct Catalogue {
    pub options: Vec<Quality>,
    pub note: String,
    pub subtitles: Vec<subtitles::Track>,
    pub storyboards: Vec<Arc<Storyboard>>,
}

#[derive(Default, Deserialize, Clone)]
struct Headers {
    #[serde(default, rename = "User-Agent")]
    agent: String,
    #[serde(default, rename = "Referer")]
    referer: String,
}

#[derive(Deserialize)]
struct Format {
    format_id: String,
    url: Option<String>,
    #[serde(default)]
    vcodec: String,
    #[serde(default)]
    acodec: String,
    height: Option<u32>,
    fps: Option<f64>,
    tbr: Option<f64>,
    format_note: Option<String>,
    #[serde(default)]
    ext: String,
    #[serde(default)]
    protocol: String,
    language: Option<String>,
    dynamic_range: Option<String>,
    has_drm: Option<bool>,
    fragments: Option<serde_json::Value>,
    #[serde(default)]
    http_headers: Headers,
}

impl Format {
    fn video(&self) -> bool {
        !self.vcodec.is_empty() && self.vcodec != "none" && self.vcodec != "images"
    }

    fn audio(&self) -> bool {
        !self.acodec.is_empty() && self.acodec != "none"
    }

    fn directly_playable(&self) -> bool {
        self.has_drm != Some(true)
            && self.fragments.is_none()
            && matches!(
                self.protocol.as_str(),
                "https" | "http" | "m3u8" | "m3u8_native"
            )
            && self.url.is_some()
    }
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    formats: Vec<Format>,
    #[serde(default)]
    http_headers: Headers,
    #[serde(flatten)]
    captions: subtitles::Info,
}

impl Format {
    /// YouTube rows read "1080p 60fps" over codecs, container, protocol and ID.
    fn youtube_label(&self, audio_codec: &str) -> (String, String) {
        let size = self
            .height
            .map_or_else(|| "Video".into(), |h| format!("{h}p"));
        let rate = self.fps.map_or(String::new(), |fps| format!(" {fps}fps"));
        let language = self
            .language
            .as_ref()
            .map_or(String::new(), |v| format!(" · {v}"));
        let range = self
            .dynamic_range
            .as_ref()
            .map_or(String::new(), |value| format!(" · {value}"));
        (
            format!("{size}{rate}"),
            format!(
                "{} + {audio_codec} · {} · {} · {}{range}{language}",
                self.vcodec, self.ext, self.protocol, self.format_id
            ),
        )
    }

    /// Twitch rows use twitch.tv's own names such as "720p60", over the
    /// bitrate. Every Twitch stream is H.264, so the codec says nothing.
    fn twitch_label(&self) -> (String, String) {
        let named = self
            .format_id
            .split_once('p')
            .is_some_and(|(height, rate)| {
                !height.is_empty()
                    && height.bytes().all(|b| b.is_ascii_digit())
                    && rate.bytes().all(|b| b.is_ascii_digit())
            });
        let title = if named {
            self.format_id.clone()
        } else {
            let size = self
                .height
                .map_or_else(|| "Video".into(), |h| format!("{h}p"));
            match self.fps.map(f64::round).filter(|fps| *fps >= 50.) {
                Some(fps) => format!("{size}{fps}"),
                None => size,
            }
        };
        let source = self.format_note.as_deref() == Some("Source");
        let rate = self
            .tbr
            .filter(|tbr| tbr.is_finite() && *tbr > 0.)
            .map(|tbr| {
                if tbr >= 1000. {
                    format!("{:.1} Mbps", tbr / 1000.)
                } else {
                    format!("{tbr:.0} Kbps")
                }
            });
        let detail = [source.then(|| "Source".to_owned()), rate]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        (title, detail)
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<Catalogue> {
    parse_with(bytes, false)
}

/// Twitch has no captions, and its rows are named and ordered like twitch.tv's.
pub(super) fn parse_twitch(bytes: &[u8]) -> Result<Catalogue> {
    parse_with(bytes, true)
}

fn parse_with(bytes: &[u8], twitch: bool) -> Result<Catalogue> {
    let info: Info = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Could not read the video quality list."))?;
    // yt-dlp sorts formats by preference, with the best last. Pair video-only
    // formats with its preferred playable audio from this same extraction.
    let audio = info
        .formats
        .iter()
        .rev()
        .find(|f| !f.video() && f.audio() && f.directly_playable());
    let mut options = Vec::new();
    let mut formats: Vec<_> = info.formats.iter().rev().filter(|f| f.video()).collect();
    if twitch {
        // yt-dlp orders by the playback preferences, so a capped height would
        // put Source last. Twitch lists the sharpest stream first.
        formats.sort_by(|a, b| {
            let rank = |f: &Format| (f.height.unwrap_or(0), f.fps.unwrap_or(0.).round() as u32);
            rank(b).cmp(&rank(a))
        });
    }
    for f in formats {
        let audio_codec = if f.audio() {
            f.acodec.as_str()
        } else {
            audio.map_or("no audio", |a| a.acodec.as_str())
        };
        let (title, detail) = if twitch {
            f.twitch_label()
        } else {
            f.youtube_label(audio_codec)
        };
        let key = format!(
            "{}|{}|{}|{}",
            f.format_id,
            f.protocol,
            f.vcodec,
            f.language.as_deref().unwrap_or_default()
        );
        let mut unavailable = if f.has_drm == Some(true) {
            "This stream is DRM protected and cannot be played by the embedded player.".into()
        } else if !f.directly_playable() {
            "This stream requires fragment assembly that this player does not yet support. Choose another format.".into()
        } else {
            String::new()
        };
        let media = if unavailable.is_empty() {
            let mut streams = vec![f.url.clone().unwrap()];
            if !f.audio()
                && let Some(audio) = audio
            {
                streams.push(audio.url.clone().unwrap());
            }
            let headers = &f.http_headers;
            let agent = if headers.agent.is_empty() {
                &info.http_headers.agent
            } else {
                &headers.agent
            };
            let referer = if headers.referer.is_empty() {
                &info.http_headers.referer
            } else {
                &headers.referer
            };
            let media = Media {
                streams,
                user_agent: agent.clone(),
                referer: if twitch {
                    super::resolve::TWITCH_REFERER.into()
                } else if referer.is_empty() {
                    "https://www.youtube.com/".into()
                } else {
                    referer.clone()
                },
                // Manual picks never show a codec fallback notice.
                vcodec: String::new(),
            };
            if media.source().is_ok() {
                Some(media)
            } else {
                unavailable = "This format has an invalid URL or headers.".into();
                None
            }
        } else {
            None
        };
        options.push(Quality {
            key,
            title,
            detail,
            media,
            unavailable,
        });
    }
    if options.is_empty() {
        bail!("No video quality options were returned.");
    }
    Ok(Catalogue {
        options,
        note: String::new(),
        subtitles: if twitch {
            Vec::new()
        } else {
            subtitles::tracks(&info.captions)
        },
        storyboards: storyboard::parse(bytes).into_iter().map(Arc::new).collect(),
    })
}

/// Preserve a muxed format's audio; pair only video-only choices. Keep the
/// explicit format ID in both branches so recovery cannot silently change it.
pub(super) fn manual_selection(key: &str) -> Option<String> {
    let id = key.split('|').next()?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return None;
    }
    Some(format!("{id}[acodec!=none]/{id}+bestaudio/{id}"))
}

const TTL: Duration = Duration::from_secs(300);
const CAPACITY: usize = 8;

struct Entry {
    id: String,
    created: Instant,
    automatic: Option<(String, Media)>,
    catalogue: Option<Catalogue>,
}

#[derive(Default)]
pub struct Cache {
    pub generation: u64,
    entries: VecDeque<Entry>,
}

pub fn fresh(media: &Media) -> bool {
    media.streams.iter().all(|stream| url_fresh(stream))
}

fn url_fresh(stream: &str) -> bool {
    let cutoff = crate::history::now().saturating_add(60);
    let Ok(url) = reqwest::Url::parse(stream) else {
        return false;
    };
    let expiry = url
        .query_pairs()
        .find(|(key, _)| key == "expire")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            url.path_segments().and_then(|parts| {
                let parts: Vec<_> = parts.collect();
                parts
                    .windows(2)
                    .find(|p| p[0] == "expire")
                    .map(|p| p[1].to_owned())
            })
        });
    expiry.is_none_or(|value| value.parse::<u64>().is_ok_and(|expiry| expiry > cutoff))
}

impl Cache {
    pub fn clear(&mut self) {
        self.generation += 1;
        self.entries.clear();
    }
    pub fn remove(&mut self, id: &str) {
        self.entries.retain(|entry| entry.id != id);
    }
    pub fn invalidate(&mut self, id: &str) {
        // A canceled lookup must not repopulate an entry after an explicit
        // refresh, even if its network response races cancellation.
        self.generation += 1;
        self.remove(id);
    }
    fn get(&self, id: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.id == id && e.created.elapsed() < TTL)
    }
    pub fn automatic(&self, id: &str, format: &str) -> Option<Media> {
        self.get(id)?
            .automatic
            .as_ref()
            .filter(|(saved_format, media)| saved_format == format && fresh(media))
            .map(|(_, media)| media.clone())
    }
    pub fn catalogue(&self, id: &str) -> Option<Catalogue> {
        self.get(id)?
            .catalogue
            .as_ref()
            .filter(|catalogue| {
                catalogue
                    .options
                    .iter()
                    .all(|q| q.media.as_ref().is_none_or(fresh))
                    && catalogue.subtitles.iter().all(|t| url_fresh(&t.url))
            })
            .cloned()
    }
    pub fn store(
        &mut self,
        generation: u64,
        id: &str,
        automatic: Option<(String, Media)>,
        catalogue: Option<Catalogue>,
    ) {
        if generation != self.generation {
            return;
        }
        if self.get(id).is_none() {
            self.remove(id);
            if self.entries.len() >= CAPACITY {
                self.entries.pop_front();
            }
            self.entries.push_back(Entry {
                id: id.into(),
                created: Instant::now(),
                automatic: None,
                catalogue: None,
            });
        }
        let entry = self.entries.iter_mut().find(|e| e.id == id).unwrap();
        if automatic.is_some() {
            entry.automatic = automatic;
        }
        if catalogue.is_some() {
            entry.catalogue = catalogue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn media(url: &str) -> Media {
        Media {
            streams: vec![url.into()],
            user_agent: String::new(),
            referer: String::new(),
            vcodec: String::new(),
        }
    }
    #[test]
    #[ignore = "requires yt-dlp on PATH; local format fixture only"]
    fn manual_selection_keeps_muxed_audio_and_pairs_video_only_formats() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("formats.json");
        std::fs::write(&path, br#"{"id":"abcdefghijk","title":"Manual formats","extractor":"youtube","webpage_url":"https://www.youtube.com/watch?v=abcdefghijk","formats":[
            {"format_id":"audio","url":"https://example.invalid/a","vcodec":"none","acodec":"aac","ext":"m4a"},
            {"format_id":"18","url":"https://example.invalid/m","vcodec":"avc1","acodec":"aac","ext":"mp4"},
            {"format_id":"137","url":"https://example.invalid/v","vcodec":"avc1","acodec":"none","ext":"mp4"}
        ]}"#).unwrap();
        for (id, expected) in [("18", "18"), ("137", "137+audio")] {
            let output = std::process::Command::new("yt-dlp")
                .args([
                    "--ignore-config",
                    "--no-plugin-dirs",
                    "--simulate",
                    "--no-check-formats",
                    "--print",
                    "format_id",
                    "--format",
                    &manual_selection(id).unwrap(),
                    "--load-info-json",
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }

    #[test]
    fn all_video_qualities_are_kept_and_audio_is_paired() {
        let catalogue = parse(br#"{"formats":[
          {"format_id":"sb","vcodec":"images","acodec":"none"},
          {"format_id":"audio","vcodec":"none","acodec":"opus","protocol":"https","url":"https://cdn.example/audio"},
          {"format_id":"4k","vcodec":"av01","acodec":"none","height":2160,"fps":60,"protocol":"https","url":"https://cdn.example/video"},
          {"format_id":"hdr","vcodec":"vp9.2","acodec":"none","height":2160,"protocol":"https","url":"https://cdn.example/hdr"},
          {"format_id":"drm","vcodec":"avc1","acodec":"aac","height":1080,"protocol":"https","has_drm":true,"url":"https://cdn.example/drm"}
        ]}"#).unwrap();
        assert_eq!(catalogue.options.len(), 3);
        assert!(catalogue.storyboards.is_empty());
        let video = catalogue
            .options
            .iter()
            .find(|q| q.key.starts_with("4k|"))
            .unwrap();
        assert_eq!(video.title, "2160p 60fps");
        assert!(video.detail.starts_with("av01 + opus"));
        assert_eq!(video.media.as_ref().unwrap().streams.len(), 2);
        assert!(
            catalogue
                .options
                .iter()
                .any(|q| q.media.is_none() && q.unavailable.contains("DRM"))
        );
    }
    #[test]
    fn twitch_rows_are_named_like_twitch_and_sharpest_first() {
        // yt-dlp's order with a 720p cap, which puts Source last.
        let catalogue = parse_twitch(br#"{"duration":2750,"subtitles":{"en":[{"ext":"vtt","url":"https://x.example/a.vtt"}]},"formats":[
          {"format_id":"Audio_Only","vcodec":"none","acodec":"mp4a.40.2","protocol":"m3u8_native","url":"https://x.example/a.m3u8"},
          {"format_id":"1080p60","vcodec":"avc1.64002A","acodec":"mp4a.40.2","height":1080,"fps":60.0,"tbr":8591.8,"format_note":"Source","protocol":"m3u8_native","url":"https://x.example/s.m3u8"},
          {"format_id":"160p","vcodec":"avc1.4D400C","acodec":"mp4a.40.2","height":160,"fps":30.004,"tbr":287.7,"protocol":"m3u8_native","url":"https://x.example/1.m3u8"},
          {"format_id":"720p","vcodec":"avc1.4D401F","acodec":"mp4a.40.2","height":720,"fps":30.004,"tbr":2270.7,"protocol":"m3u8_native","url":"https://x.example/2.m3u8"},
          {"format_id":"chunked","vcodec":"avc1","acodec":"mp4a.40.2","height":900,"fps":59.94,"protocol":"m3u8_native","url":"https://x.example/c.m3u8"},
          {"format_id":"720p60","vcodec":"avc1.4D4020","acodec":"mp4a.40.2","height":720,"fps":60.0,"tbr":3168.8,"protocol":"m3u8_native","url":"https://x.example/3.m3u8"}
        ]}"#).unwrap();
        let rows: Vec<_> = catalogue
            .options
            .iter()
            .map(|q| (q.title.as_str(), q.detail.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("1080p60", "Source · 8.6 Mbps"),
                ("900p60", ""),
                ("720p60", "3.2 Mbps"),
                ("720p", "2.3 Mbps"),
                ("160p", "288 Kbps"),
            ]
        );
        assert!(catalogue.subtitles.is_empty());
        let media = catalogue.options[0].media.as_ref().unwrap();
        assert_eq!(media.referer, super::super::resolve::TWITCH_REFERER);
    }

    #[test]
    fn a_malformed_storyboard_keeps_the_quality_list() {
        let catalogue = parse(br#"{"duration":100,"formats":[
          {"format_id":"sb0","vcodec":"none","acodec":"none","width":320,"height":180,"fps":1,"columns":"three","rows":3,"fragments":[{"url":"https://i.ytimg.com/a.jpg"}]},
          {"format_id":"v","vcodec":"vp9","acodec":"opus","protocol":"https","url":"https://cdn.example/video"}
        ]}"#).unwrap();
        assert_eq!(catalogue.options.len(), 1);
        assert!(catalogue.storyboards.is_empty());
    }
    #[test]
    fn cache_is_bounded_expires_and_rejects_old_account_results() {
        let mut cache = Cache::default();
        for i in 0..10 {
            cache.store(
                0,
                &i.to_string(),
                Some(("default".into(), media("https://cdn.example/video"))),
                None,
            );
        }
        assert_eq!(cache.entries.len(), CAPACITY);
        assert!(cache.automatic("9", "default").is_some());
        assert!(cache.automatic("9", "changed quality or codec").is_none());
        assert!(cache.automatic("0", "default").is_none());
        cache.entries.back_mut().unwrap().created = Instant::now() - TTL;
        assert!(cache.automatic("9", "default").is_none());
        assert!(!fresh(&media("https://cdn.example/video?expire=1")));
        assert!(!fresh(&media("https://cdn.example/expire/1/video")));
        cache.clear();
        cache.store(
            0,
            "old",
            Some(("default".into(), media("https://cdn.example/video"))),
            None,
        );
        assert!(cache.entries.is_empty());
        let generation = cache.generation;
        cache.store(
            generation,
            "refresh",
            Some(("default".into(), media("https://cdn.example/video"))),
            None,
        );
        cache.invalidate("refresh");
        cache.store(
            generation,
            "refresh",
            Some(("default".into(), media("https://cdn.example/stale"))),
            None,
        );
        assert!(cache.automatic("refresh", "default").is_none());
    }

    #[test]
    fn captions_from_the_same_lookup_expire_with_the_catalogue() {
        let catalogue = |url: &str, account: bool| {
            let mut data = serde_json::json!({
                "formats": [{"format_id":"v", "vcodec":"vp9", "acodec":"opus", "protocol":"https", "url":"https://cdn.example/video"}],
                "subtitles": {"en": [{"ext":"vtt", "url":url}]}
            });
            if account {
                data["automatic_captions"] = serde_json::json!({"fr":[{"ext":"vtt", "url":"https://www.youtube.com/api/timedtext?lang=fr"}]});
            }
            parse(&serde_json::to_vec(&data).unwrap()).unwrap()
        };
        let catalogue = catalogue("https://www.youtube.com/api/timedtext?source=account", true);
        assert_eq!(catalogue.subtitles.len(), 2);
        let mut cache = Cache::default();
        cache.store(0, "video", None, Some(catalogue));
        assert!(cache.catalogue("video").is_some());
        cache
            .entries
            .back_mut()
            .unwrap()
            .catalogue
            .as_mut()
            .unwrap()
            .subtitles[0]
            .url = "https://www.youtube.com/api/timedtext?expire=1".into();
        assert!(cache.catalogue("video").is_none());
    }
}
