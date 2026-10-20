//! Shorts cards arrive without a channel or upload date, so each one is looked up once.
use super::*;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[derive(Clone)]
struct Info {
    channel: String,
    channel_id: String,
    avatar: String,
    date: String,
}

// `None` marks a lookup still in flight.
static KNOWN: LazyLock<Mutex<HashMap<String, Option<Info>>>> = LazyLock::new(Default::default);

fn needs(video: &Video) -> bool {
    video.duration == "SHORT" && video.channel.is_empty() && !video.id.is_empty()
}

fn with_date(metadata: &str, date: &str) -> String {
    if date.is_empty() || metadata.contains(date) {
        metadata.into()
    } else if metadata.is_empty() {
        date.into()
    } else {
        format!("{metadata} · {date}")
    }
}

/// Relative age in YouTube's style, matching the other cards.
fn age(published: std::time::SystemTime) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(published)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (count, unit) = match seconds {
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3600, "hour"),
        s if s < 7 * 86_400 => (s / 86_400, "day"),
        s if s < 30 * 86_400 => (s / (7 * 86_400), "week"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
        s => (s / (365 * 86_400), "year"),
    };
    let count = count.max(1);
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

/// Fills in a Short that was looked up earlier, so republished rows keep their details.
pub(super) fn apply(video: &mut Video) {
    if !needs(video) {
        return;
    }
    if let Some(Some(info)) = KNOWN.lock().unwrap().get(&video.id) {
        video.channel = info.channel.clone();
        video.channel_id = info.channel_id.clone();
        video.avatar = info.avatar.clone();
        video.metadata = with_date(&video.metadata, &info.date);
    }
}

/// Looks up the channel, avatar and upload date of every Short not seen before.
pub(super) fn lookup(weak: &slint::Weak<AppWindow>, session: Arc<Session>, videos: &[Video]) {
    let ids: Vec<String> = {
        let mut known = KNOWN.lock().unwrap();
        if known.len() > 4096 {
            known.retain(|_, info| info.is_none());
        }
        videos
            .iter()
            .filter(|video| needs(video))
            .filter(|video| {
                let fresh = !known.contains_key(&video.id);
                if fresh {
                    known.insert(video.id.clone(), None);
                }
                fresh
            })
            .map(|video| video.id.clone())
            .collect()
    };
    if ids.is_empty() {
        return;
    }
    let weak = weak.clone();
    tokio::spawn(async move {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
        let mut jobs = tokio::task::JoinSet::new();
        for id in ids {
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let (session, weak) = (session.clone(), weak.clone());
            jobs.spawn(async move {
                let _permit = permit;
                let details = session.client().query().video_details(&id).await;
                let Ok(details) = details else {
                    // Forget the failure so a later visit can try again.
                    KNOWN.lock().unwrap().remove(&id);
                    return;
                };
                let info = Info {
                    channel: details.channel.name,
                    channel_id: details.channel.id,
                    avatar: youtube::avatar_url(&details.channel.avatar),
                    date: details
                        .publish_date
                        .map(|date| age(std::time::SystemTime::from(date)))
                        .or(details.publish_date_txt)
                        .unwrap_or_default(),
                };
                KNOWN.lock().unwrap().insert(id.clone(), Some(info.clone()));
                update(&weak, move |app| {
                    let model = app.global::<AppState>().get_videos();
                    for index in 0..model.row_count() {
                        let Some(mut row) = model.row_data(index) else {
                            continue;
                        };
                        if row.id != id.as_str() || !row.channel.is_empty() {
                            continue;
                        }
                        row.channel = info.channel.as_str().into();
                        row.channel_id = info.channel_id.as_str().into();
                        row.avatar =
                            avatars::channel_avatar(&info.channel_id, info.avatar.clone()).into();
                        row.initials = initials(&info.channel).into();
                        row.tone = tone(&info.channel);
                        row.metadata = with_date(&row.metadata, &info.date).into();
                        model.set_row_data(index, row);
                    }
                });
            });
        }
        while jobs.join_next().await.is_some() {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn ages_read_like_youtube_and_dates_join_metadata_once() {
        let ago = |s: u64| age(SystemTime::now() - Duration::from_secs(s));
        assert_eq!(ago(30), "1 minute ago");
        assert_eq!(ago(2 * 3600 + 5), "2 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(15 * 86_400), "2 weeks ago");
        assert_eq!(ago(400 * 86_400), "1 year ago");
        assert_eq!(
            with_date("1.3M views", "2 days ago"),
            "1.3M views · 2 days ago"
        );
        assert_eq!(
            with_date("1.3M views · 2 days ago", "2 days ago"),
            "1.3M views · 2 days ago"
        );
        assert_eq!(with_date("", "2 days ago"), "2 days ago");
    }
}
