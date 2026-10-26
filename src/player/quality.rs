//! Account-scoped, bounded, memory-only stream catalogues. Signed URLs never
//! enter UI models, logs, or the on-disk watch-history cache.
use super::resolve::{self, Media};
use super::subtitles;
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Quality {
    pub key: String,
    pub label: String,
    pub media: Option<Media>,
    pub unavailable: String,
}

#[derive(Clone, Default)]
pub struct Catalogue {
    pub options: Vec<Quality>,
    pub note: String,
    pub subtitles: Vec<subtitles::Track>,
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

fn parse(bytes: &[u8]) -> Result<Catalogue> {
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
    for f in info.formats.iter().rev().filter(|f| f.video()) {
        let size = f.height.map_or_else(|| "Video".into(), |h| format!("{h}p"));
        let rate = f.fps.map_or(String::new(), |fps| format!(" {fps}fps"));
        let audio_codec = if f.audio() {
            f.acodec.as_str()
        } else {
            audio.map_or("no audio", |a| a.acodec.as_str())
        };
        let language = f
            .language
            .as_ref()
            .map_or(String::new(), |v| format!(" · {v}"));
        let range = f
            .dynamic_range
            .as_ref()
            .map_or(String::new(), |value| format!(" · {value}"));
        let label = format!(
            "{size}{rate} · {} + {audio_codec} · {} · {} · {}{range}{language}",
            f.vcodec, f.ext, f.protocol, f.format_id
        );
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
                referer: if referer.is_empty() {
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
                unavailable = "YouTube returned an invalid URL or headers for this format.".into();
                None
            }
        } else {
            None
        };
        options.push(Quality {
            key,
            label,
            media,
            unavailable,
        });
    }
    if options.is_empty() {
        bail!("YouTube returned no video quality options.");
    }
    Ok(Catalogue {
        options,
        note: String::new(),
        subtitles: subtitles::tracks(&info.captions),
    })
}

pub async fn load(id: &str, cookies: &str) -> Result<Catalogue> {
    // Keep all video formats, including different codecs/frame rates and
    // signed-in-only choices. Do not use --check-all-formats: it would download
    // test fragments for every entry just to display the menu.
    let (public, account) = tokio::join!(
        resolve::catalogue_output(id, ""),
        resolve::catalogue_output(id, cookies),
    );
    merge(
        public.and_then(|v| parse(&v)),
        account.and_then(|v| parse(&v)),
    )
}

fn merge(public: Result<Catalogue>, account: Result<Catalogue>) -> Result<Catalogue> {
    match (public, account) {
        (Ok(mut public), Ok(account)) => {
            for track in account.subtitles {
                if !public.subtitles.iter().any(|t| t.key == track.key) {
                    public.subtitles.push(track);
                }
            }
            for entry in account.options {
                if let Some(existing) = public.options.iter_mut().find(|v| v.key == entry.key) {
                    // Prefer public URLs for duplicate formats: the signed-in
                    // equivalent may be rejected by YouTube's CDN (HTTP 403).
                    if existing.media.is_none() && entry.media.is_some() {
                        *existing = entry;
                    }
                } else {
                    public.options.push(entry);
                }
            }
            Ok(public)
        }
        (Ok(mut catalogue), Err(_)) => {
            catalogue.note = "Signed-in quality lookup failed; showing public options. Retry to refresh the list.".into();
            Ok(catalogue)
        }
        (Err(_), Ok(mut catalogue)) => {
            catalogue.note = "Public quality lookup failed; showing signed-in options. Retry to refresh the list.".into();
            Ok(catalogue)
        }
        (Err(error), Err(_)) => Err(error),
    }
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
    fn all_video_qualities_are_kept_and_audio_is_paired() {
        let catalogue = parse(br#"{"formats":[
          {"format_id":"sb","vcodec":"images","acodec":"none"},
          {"format_id":"audio","vcodec":"none","acodec":"opus","protocol":"https","url":"https://cdn.example/audio"},
          {"format_id":"4k","vcodec":"av01","acodec":"none","height":2160,"fps":60,"protocol":"https","url":"https://cdn.example/video"},
          {"format_id":"hdr","vcodec":"vp9.2","acodec":"none","height":2160,"protocol":"https","url":"https://cdn.example/hdr"},
          {"format_id":"drm","vcodec":"avc1","acodec":"aac","height":1080,"protocol":"https","has_drm":true,"url":"https://cdn.example/drm"}
        ]}"#).unwrap();
        assert_eq!(catalogue.options.len(), 3);
        let video = catalogue
            .options
            .iter()
            .find(|q| q.key.starts_with("4k|"))
            .unwrap();
        assert!(video.label.contains("2160p 60fps"));
        assert_eq!(video.media.as_ref().unwrap().streams.len(), 2);
        assert!(
            catalogue
                .options
                .iter()
                .any(|q| q.media.is_none() && q.unavailable.contains("DRM"))
        );
        let merged = merge(Ok(catalogue.clone()), Ok(catalogue)).unwrap();
        assert_eq!(merged.options.len(), 3);
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
    fn account_only_formats_survive_merging_and_partial_failures_are_visible() {
        let public = parse(br#"{"formats":[{"format_id":"public","vcodec":"vp9","acodec":"opus","protocol":"https","url":"https://cdn.example/public"}]}"#).unwrap();
        let account = parse(br#"{"formats":[{"format_id":"account","vcodec":"av01","acodec":"aac","protocol":"https","url":"https://cdn.example/account"}]}"#).unwrap();
        let merged = merge(Ok(public.clone()), Ok(account)).unwrap();
        assert_eq!(merged.options.len(), 2);
        let partial = merge(Ok(public), Err(anyhow::anyhow!("synthetic lookup failure"))).unwrap();
        assert_eq!(partial.options.len(), 1);
        assert!(partial.note.contains("Signed-in quality lookup failed"));
    }

    #[test]
    fn captions_merge_without_losing_account_tracks_and_expire_with_the_catalogue() {
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
        let merged = merge(
            Ok(catalogue(
                "https://www.youtube.com/api/timedtext?source=public",
                false,
            )),
            Ok(catalogue(
                "https://www.youtube.com/api/timedtext?source=account",
                true,
            )),
        )
        .unwrap();
        assert_eq!(merged.subtitles.len(), 2);
        assert!(merged.subtitles[0].url.ends_with("source=public"));
        let mut cache = Cache::default();
        cache.store(0, "video", None, Some(merged));
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
