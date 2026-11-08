//! VOD chat windows. Keep Twitch's original offsets and fragments intact.
use super::*;
use std::collections::HashSet;

pub const WINDOW: u64 = 120;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub content_offset_seconds: f64,
    pub created_at: String,
    pub commenter: Option<Commenter>,
    pub message: Message,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Commenter {
    pub display_name: String,
    pub login: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub fragments: Vec<Fragment>,
    #[serde(default)]
    pub user_color: Option<String>,
    #[serde(default)]
    pub user_badges: Vec<Badge>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Fragment {
    pub text: Option<String>,
    pub emote: Option<Emote>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Emote {
    #[serde(rename = "emoteID")]
    pub id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Badge {
    #[serde(default)]
    pub image_url: String,
    #[serde(rename = "setID")]
    pub set_id: String,
    pub version: String,
}
#[derive(Deserialize)]
struct Edge {
    cursor: String,
    node: Comment,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    edges: Vec<Edge>,
    page_info: PageInfo,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
}

fn parse(data: Value) -> Result<Page> {
    if data["video"].is_null() {
        bail!("This video's chat is unavailable.");
    }
    if data["video"]["comments"].is_null() {
        bail!("Twitch did not return chat. Try again shortly.");
    }
    serde_json::from_value(data["video"]["comments"].clone()).context("Could not read Twitch chat.")
}

impl Client {
    async fn chat_badges(&self, video: &str) -> Result<BTreeMap<String, String>> {
        let cache = crate::cache::Cache::new("twitch-chat");
        if let Some(cache) = &cache
            && let Some(badges) = cache.get("badges-v1", video).await
        {
            return Ok(badges);
        }
        let fields = "setID version imageURL(size:DOUBLE)";
        let query = format!(
            "query{{badges{{{fields}}} video(id:\"{video}\"){{owner{{broadcastBadges{{{fields}}}}}}}}}"
        );
        let response = self
            .http
            .post(GQL_URL)
            .header("Client-Id", CLIENT_ID)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"query": query}).to_string())
            .send()
            .await?
            .error_for_status()?;
        let bytes = crate::github::read_limited(response, RESPONSE_LIMIT).await?;
        let data = read_reply(serde_json::from_slice(&bytes)?)?.context("Badges unavailable")?;
        let mut badges = BTreeMap::new();
        for list in [&data["badges"], &data["video"]["owner"]["broadcastBadges"]] {
            if let Some(list) = list.as_array() {
                for badge in list {
                    if let (Some(set), Some(version), Some(url)) = (
                        badge["setID"].as_str(),
                        badge["version"].as_str(),
                        badge["imageURL"].as_str(),
                    ) {
                        badges.insert(format!("{set}:{version}"), url.to_owned());
                    }
                }
            }
        }
        if let Some(cache) = cache {
            cache.put("badges-v1", video, &badges, crate::cache::DAY);
        }
        Ok(badges)
    }

    pub async fn chat_window(
        &self,
        video: &str,
        start: u64,
        mut progress: impl FnMut(&[Comment]) + Send,
    ) -> Result<Vec<Comment>> {
        if video.is_empty() || !video.bytes().all(|b| b.is_ascii_digit()) {
            bail!("Invalid Twitch video ID.");
        }
        let cache = crate::cache::Cache::new("twitch-chat");
        let key = format!("{video}:{start}");
        if let Some(cache) = &cache
            && let Some(rows) = cache.get("windows-v2", &key).await
        {
            return Ok(rows);
        }
        let badges = tokio::time::timeout(Duration::from_secs(5), self.chat_badges(video))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let mut variables = json!({"videoID":video,"contentOffsetSeconds":start});
        let mut cursors = HashSet::new();
        let mut ids = HashSet::new();
        let mut rows = Vec::new();
        loop {
            let mut attempt = 0;
            let page = loop {
                let result = match self
                    .query("VideoCommentsByOffsetOrCursor", &variables)
                    .await
                {
                    Ok(data) => parse(data),
                    Err(e) => Err(e),
                };
                match result {
                    Ok(page) => break page,
                    Err(error) if attempt >= 3 => return Err(error),
                    Err(_) => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                    }
                }
            };
            let reached_end = page
                .edges
                .iter()
                .any(|e| e.node.content_offset_seconds >= (start + WINDOW) as f64);
            let cursor = page.edges.last().map(|e| e.cursor.clone());
            for edge in page.edges {
                let mut row = edge.node;
                for badge in &mut row.message.user_badges {
                    badge.image_url = badges
                        .get(&format!("{}:{}", badge.set_id, badge.version))
                        .cloned()
                        .unwrap_or_default();
                }
                if row.content_offset_seconds.is_finite()
                    && row.content_offset_seconds >= start as f64
                    && row.content_offset_seconds < (start + WINDOW) as f64
                    && ids.insert(row.id.clone())
                {
                    rows.push(row);
                }
            }
            rows.sort_by(|a, b| {
                a.content_offset_seconds
                    .total_cmp(&b.content_offset_seconds)
                    .then(a.id.cmp(&b.id))
            });
            progress(&rows);
            if reached_end || !page.page_info.has_next_page {
                break;
            }
            let cursor = cursor
                .filter(|s| !s.is_empty())
                .context("Twitch returned an empty chat page with more pages pending.")?;
            if !cursors.insert(cursor.clone()) {
                bail!("Twitch chat pagination stopped advancing. Please retry.");
            }
            variables = json!({"videoID":video,"cursor":cursor});
        }
        rows.sort_by(|a, b| {
            a.content_offset_seconds
                .total_cmp(&b.content_offset_seconds)
                .then(a.id.cmp(&b.id))
        });
        if let Some(cache) = cache {
            cache.put("windows-v2", &key, &rows, 60 * 60);
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires Twitch network access and an available VOD"]
    async fn live_chat_window_downloads_cursor_pages() {
        let client = Client::new().unwrap();
        let rows = client.chat_window("2921205471", 0, |_| {}).await.unwrap();
        assert!(rows.len() > 59, "must retrieve more than the first page");
        assert!(
            rows.iter()
                .all(|c| (0.0..WINDOW as f64).contains(&c.content_offset_seconds))
        );
        let ids: HashSet<_> = rows.iter().map(|c| &c.id).collect();
        assert_eq!(ids.len(), rows.len());
        assert!(
            rows.windows(2)
                .all(|w| w[0].content_offset_seconds <= w[1].content_offset_seconds)
        );
    }
    #[test]
    fn unavailable_chat_is_an_error_not_an_empty_history() {
        assert!(parse(json!({"video":null})).is_err());
        assert!(parse(json!({"video":{"comments":null}})).is_err());
        let page =
            parse(json!({"video":{"comments":{"edges":[],"pageInfo":{"hasNextPage":false}}}}))
                .unwrap();
        assert!(page.edges.is_empty());
    }
    #[test]
    fn keeps_unknown_authors_and_native_emotes() {
        let row: Comment = serde_json::from_value(json!({"id":"x","contentOffsetSeconds":12,"createdAt":"2026-11-08T00:00:12Z","commenter":null,"message":{"fragments":[{"text":"Kappa","emote":{"emoteID":"25"}}],"userColor":null,"userBadges":[{"setID":"moderator","version":"1"}]}})).unwrap();
        assert!(row.commenter.is_none());
        assert_eq!(row.message.fragments[0].emote.as_ref().unwrap().id, "25");
    }
}
