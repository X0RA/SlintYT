//! Playback-driven chat. UI state is owned by the event loop; requests are cancellable.
use super::*;
use crate::{
    ChatImage, ChatRow,
    twitch::{
        Client,
        chat::{Comment, WINDOW},
    },
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    rc::Rc,
    sync::mpsc,
};

struct Reply {
    epoch: u64,
    start: u64,
    result: anyhow::Result<Vec<Comment>>,
    complete: bool,
}
#[derive(Default)]
struct Timeline {
    video: String,
    epoch: u64,
    first: u64,
    position: f64,
    windows: BTreeMap<u64, Vec<Comment>>,
    complete: HashSet<u64>,
    job: Option<tokio::task::JoinHandle<()>>,
    shown: Vec<String>,
    retry: bool,
    older: bool,
}
impl Timeline {
    fn cancel(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
        self.epoch = self.epoch.wrapping_add(1);
    }
}
impl Drop for Timeline {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let client = Client::new().map(Arc::new);
    let timeline = Rc::new(RefCell::new(Timeline::default()));
    let state = app.global::<AppState>();
    let data = timeline.clone();
    state.on_chat_retry(move || data.borrow_mut().retry = true);
    let data = timeline.clone();
    state.on_chat_earlier(move || data.borrow_mut().older = true);
    let (send, receive) = mpsc::channel::<Reply>();
    let weak = app.as_weak();
    let timer = Rc::new(slint::Timer::default());
    let keep = timer.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(app) = weak.upgrade() else {
                keep.stop();
                return;
            };
            let ui = app.global::<AppState>();
            let mut data = timeline.borrow_mut();
            let active = ui.get_player_open() && ui.get_player_twitch();
            let video = ui
                .get_player_video_id()
                .strip_prefix("twitch:")
                .unwrap_or_default()
                .to_owned();
            if (active && data.video != video) || (!active && !data.video.is_empty()) {
                data.cancel();
                data.video = if active { video } else { String::new() };
                data.windows.clear();
                data.complete.clear();
                data.shown.clear();
                data.position = -100.;
                data.older = false;
                ui.set_chat_rows(ModelRc::default());
                ui.set_chat_error("".into());
                ui.set_chat_busy(false);
                ui.set_chat_follow(true);
                if !active {
                    ui.set_chat_open(false);
                }
            }
            if !active || !ui.get_chat_open() || ui.get_player_pip() {
                data.cancel();
                ui.set_chat_busy(false);
                return;
            }
            let client = match &client {
                Ok(c) => c.clone(),
                Err(e) => {
                    ui.set_chat_error(format!("{e:#}").into());
                    return;
                }
            };
            let position = f64::from(ui.get_player_position()).max(0.);
            let current = position as u64 / WINDOW * WINDOW;
            let seek = (position - data.position).abs() > 5. || position < data.position - 0.5;
            if seek {
                data.cancel();
                data.first = current.saturating_sub(WINDOW);
                data.shown.clear();
                ui.set_chat_rows(ModelRc::default());
                ui.set_chat_follow(true);
                ui.set_chat_error("".into());
                ui.set_chat_reset(ui.get_chat_reset().wrapping_add(1));
            }
            data.position = position;
            if data.retry {
                data.retry = false;
                ui.set_chat_error("".into());
            }
            if data.older {
                data.older = false;
                data.first = data.first.saturating_sub(WINDOW);
                ui.set_chat_error("".into());
            }
            for reply in receive.try_iter() {
                if reply.epoch != data.epoch {
                    continue;
                }
                if reply.complete {
                    data.job = None;
                }
                match reply.result {
                    Ok(rows) => {
                        if reply.complete {
                            data.complete.insert(reply.start);
                        }
                        data.windows.insert(reply.start, rows);
                    }
                    Err(e) => ui.set_chat_error(format!("{e:#}").into()),
                }
            }
            let comments = visible(&data.windows, data.first, position);
            let ids: Vec<_> = comments.iter().map(|c| c.id.clone()).collect();
            if ids != data.shown {
                let rows: Vec<_> = comments.into_iter().map(row).collect();
                // Preserve the model and its instantiated row anchor for both
                // history prepends and new playback messages.
                let offset = retained_offset(&ids, &data.shown);
                if let Some(offset) = offset
                    && let Some(model) = ui
                        .get_chat_rows()
                        .as_any()
                        .downcast_ref::<VecModel<ChatRow>>()
                {
                    // Single-row notifications keep Slint's anchor intact even
                    // when the prefix is larger than the visible row offset.
                    for item in rows[..offset].iter().rev() {
                        model.insert(0, item.clone());
                    }
                    for item in rows.into_iter().skip(offset + data.shown.len()) {
                        model.push(item);
                    }
                } else {
                    ui.set_chat_rows(ModelRc::new(VecModel::from(rows)));
                }
                data.shown = ids;
            }
            ui.set_chat_has_earlier(data.first > 0);
            let ahead = if position as u64 % WINDOW > WINDOW - 30 {
                current + WINDOW
            } else {
                current
            };
            let missing = std::iter::once(current)
                .chain((data.first..=ahead).step_by(WINDOW as usize))
                .find(|s| !data.complete.contains(s));
            if data.job.is_none()
                && ui.get_chat_error().is_empty()
                && let Some(start) = missing
            {
                let (video, epoch, send) = (data.video.clone(), data.epoch, send.clone());
                data.job = Some(runtime.spawn(async move {
                    let result = client
                        .chat_window(&video, start, |rows| {
                            if start < current {
                                return;
                            }
                            let _ = send.send(Reply {
                                epoch,
                                start,
                                result: Ok(rows.to_vec()),
                                complete: false,
                            });
                        })
                        .await;
                    let _ = send.send(Reply {
                        epoch,
                        start,
                        result,
                        complete: true,
                    });
                }));
            }
            ui.set_chat_busy(data.job.is_some());
        },
    );
}

// Locate the retained contiguous rows; only actual replacements need a new model.
fn retained_offset(ids: &[String], shown: &[String]) -> Option<usize> {
    if shown.is_empty() {
        return Some(0);
    }
    let offset = ids.iter().position(|id| id == &shown[0])?;
    ids.get(offset..offset + shown.len())
        .filter(|retained| *retained == shown)
        .map(|_| offset)
}

fn visible(windows: &BTreeMap<u64, Vec<Comment>>, first: u64, position: f64) -> Vec<&Comment> {
    let current = position.max(0.) as u64 / WINDOW * WINDOW;
    if first > current {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    windows
        .range(first..=current)
        .flat_map(|(_, rows)| rows)
        .filter(|row| row.content_offset_seconds <= position && seen.insert(row.id.as_str()))
        .collect()
}

fn row(comment: &Comment) -> ChatRow {
    let author = comment
        .commenter
        .as_ref()
        .map(|c| c.display_name.as_str())
        .unwrap_or("Unknown user");
    let seconds = comment.content_offset_seconds.max(0.) as u64;
    let images: Vec<_> = comment
        .message
        .fragments
        .iter()
        .filter_map(|f| {
            let emote = f.emote.as_ref()?;
            if !emote
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return None;
            }
            Some(ChatImage {
                label: f.text.as_deref().unwrap_or("Emote").into(),
                url: format!(
                    "https://static-cdn.jtvnw.net/emoticons/v2/{}/static/dark/2.0",
                    emote.id
                )
                .into(),
            })
        })
        .collect();
    let color = comment
        .message
        .user_color
        .as_deref()
        .and_then(|s| u32::from_str_radix(s.strip_prefix('#')?, 16).ok())
        .map(|v| {
            slint::Color::from_rgb_u8(
                ((v >> 16) as u8).max(80),
                ((v >> 8) as u8).max(80),
                (v as u8).max(80),
            )
        })
        .unwrap_or(slint::Color::from_rgb_u8(191, 148, 255));
    ChatRow {
        id: comment.id.as_str().into(),
        author: author.into(),
        color,
        time: format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
        .into(),
        text: comment
            .message
            .fragments
            .iter()
            .filter_map(|f| f.text.as_deref())
            .collect::<String>()
            .into(),
        badge_images: ModelRc::new(VecModel::from(
            comment
                .message
                .user_badges
                .iter()
                .filter(|b| !b.image_url.is_empty())
                .map(|b| ChatImage {
                    label: b.set_id.as_str().into(),
                    url: b.image_url.as_str().into(),
                })
                .collect::<Vec<_>>(),
        )),
        badges: comment
            .message
            .user_badges
            .iter()
            .filter(|b| b.image_url.is_empty())
            .map(|b| b.set_id.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
            .into(),
        emotes: ModelRc::new(VecModel::from(images)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn comment(id: &str, seconds: f64) -> Comment {
        serde_json::from_value(serde_json::json!({"id":id,"contentOffsetSeconds":seconds,"createdAt":"2026-11-08T00:00:00Z","commenter":null,"message":{"fragments":[],"userBadges":[]}})).unwrap()
    }
    #[test]
    fn history_updates_retain_existing_rows() {
        let ids = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let shown = ids(&["b", "c"]);
        assert_eq!(
            retained_offset(&ids(&["a", "b", "c", "d"]), &shown),
            Some(1)
        );
        assert_eq!(retained_offset(&ids(&["b", "c", "d"]), &shown), Some(0));
        assert_eq!(retained_offset(&ids(&["a", "b", "d"]), &shown), None);
        assert_eq!(retained_offset(&ids(&["a"]), &[]), Some(0));
    }

    #[test]
    fn playback_hides_future_messages_and_rewinds_cached_history() {
        let windows = BTreeMap::from([
            (
                0,
                vec![comment("a", 2.), comment("b", 49.), comment("b", 49.)],
            ),
            (WINDOW, vec![comment("c", 125.)]),
        ]);
        assert_eq!(
            visible(&windows, 0, 50.)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(visible(&windows, 0, 130.).len(), 3);
        assert_eq!(visible(&windows, 0, 3.).len(), 1);
        assert!(visible(&windows, WINDOW, 3.).is_empty());
    }
}
