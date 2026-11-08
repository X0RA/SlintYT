//! Search through Twitch's unofficial GraphQL API.
//!
//! Twitch only runs queries it already knows, named by their hash. A community
//! project publishes those hashes, so a copy is kept in the config folder and
//! only checked for changes (with its ETag) after Twitch rejects a hash.
use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub mod chat;

const OPERATIONS_URL: &str = "https://raw.githubusercontent.com/DmitryScaletta/twitch-gql-queries/main/packages/typescript/src/generated/operations-all.generated.ts";
const GQL_URL: &str = "https://gql.twitch.tv/gql";
/// Public client ID supporting both browsing and chat cursor queries.
const CLIENT_ID: &str = "kd1unb4b3q4t58fwlpcbzcbnm76a8fp";
const SEARCH_OPERATION: &str = "SearchResultsPage_SearchResults";
const VIDEOS_OPERATION: &str = "FilterableVideoTower_Videos";
const ABOUT_OPERATION: &str = "ChannelRoot_AboutPanel";
const VIDEO_PAGE: u32 = 50;
/// Follows show "30+ new videos" past this many.
pub const LATEST_PAGE: u32 = 30;
const OPERATIONS_LIMIT: usize = 8 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
/// Rejected hashes trigger at most one GitHub check in this many seconds.
const CHECK_INTERVAL: u64 = 10 * 60;
const SEARCH_TTL: u64 = 15 * 60;
const UNAVAILABLE: &str = "Twitch search is unavailable right now. Its API changed and no updated query list is published yet.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchMode {
    Video,
    User,
}

impl SearchMode {
    fn index(self) -> &'static str {
        match self {
            Self::Video => "VOD",
            Self::User => "CHANNEL",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Video => "videos",
            Self::User => "channels",
        }
    }
}

/// Which of a user's videos to list. Follows the buttons on the user page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoKind {
    PastBroadcast,
    Highlight,
    Upload,
}

impl VideoKind {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Highlight,
            2 => Self::Upload,
            _ => Self::PastBroadcast,
        }
    }

    /// The badge on a video's thumbnail.
    pub fn label(self) -> &'static str {
        match self {
            Self::PastBroadcast => "Broadcast",
            Self::Highlight => "Highlight",
            Self::Upload => "Upload",
        }
    }

    fn broadcast_type(self) -> &'static str {
        match self {
            Self::PastBroadcast => "ARCHIVE",
            Self::Highlight => "HIGHLIGHT",
            Self::Upload => "UPLOAD",
        }
    }
}

/// `id` is a video's number, or a user's login. `count` is views or followers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub metadata: String,
    pub duration: String,
    pub picture: String,
    #[serde(default)]
    pub avatar: String,
    pub count: u64,
    pub seconds: u64,
    pub date: String,
    /// Videos only: the game, the channel's login, when it was published,
    /// and its kind when the list says, such as "Highlight".
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub published_at: u64,
    #[serde(default)]
    pub kind: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub description: String,
    pub followers: String,
}

/// `total` is how many matches search found overall, when it says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub cursor: Option<String>,
    #[serde(default)]
    pub total: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Operations {
    etag: Option<String>,
    checked_at: u64,
    hashes: BTreeMap<String, String>,
}

pub struct Client {
    http: reqwest::Client,
    path: PathBuf,
    operations: Mutex<Option<Operations>>,
}

impl Client {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .https_only(true)
                .user_agent(crate::github::user_agent())
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()?,
            path: crate::config::Store::new()?
                .directory()
                .join("twitch-operations.json"),
            operations: Mutex::new(None),
        })
    }

    /// Makes sure the query list exists, downloading it only if it's missing.
    pub async fn prepare(&self) -> Result<()> {
        let mut operations = self.operations.lock().await;
        self.ensure(&mut operations).await?;
        Ok(())
    }

    pub async fn search(
        &self,
        mode: SearchMode,
        query: &str,
        cursor: Option<&str>,
    ) -> Result<SearchPage> {
        let key = format!(
            "{}\0{}\0{}",
            mode.index(),
            cursor.unwrap_or_default(),
            query.to_lowercase()
        );
        let mut target = json!({ "index": mode.index() });
        if let Some(cursor) = cursor {
            target["cursor"] = cursor.into();
        }
        let variables = json!({ "query": query, "options": { "targets": [target] } });
        self.cached("search-v3", &key, SEARCH_OPERATION, &variables, |data| {
            parse_search(data, mode)
        })
        .await
    }

    pub async fn user_videos(
        &self,
        login: &str,
        kind: VideoKind,
        popular: bool,
        cursor: Option<&str>,
    ) -> Result<SearchPage> {
        let sort = if popular { "VIEWS" } else { "TIME" };
        let key = format!(
            "{login}\0{}\0{sort}\0{}",
            kind.broadcast_type(),
            cursor.unwrap_or_default()
        );
        let mut variables = json!({
            "channelOwnerLogin": login,
            "limit": VIDEO_PAGE,
            "broadcastType": kind.broadcast_type(),
            "videoSort": sort,
        });
        if let Some(cursor) = cursor {
            variables["cursor"] = cursor.into();
        }
        self.cached("videos-v2", &key, VIDEOS_OPERATION, &variables, |data| {
            // Only this list says which kind its videos are.
            let mut page = parse_videos(data)?;
            for video in &mut page.results {
                video.kind = kind.label().into();
            }
            Ok(page)
        })
        .await
    }

    /// A channel's newest videos of every kind, for counting the new ones.
    pub async fn latest_videos(&self, login: &str) -> Result<SearchPage> {
        let variables = json!({
            "channelOwnerLogin": login,
            "limit": LATEST_PAGE,
            "videoSort": "TIME",
        });
        self.cached(
            "latest-v2",
            login,
            VIDEOS_OPERATION,
            &variables,
            parse_videos,
        )
        .await
    }

    pub async fn user_info(&self, login: &str) -> Result<UserInfo> {
        let variables =
            json!({ "channelLogin": login, "skipSchedule": true, "includeIsDJ": false });
        self.cached("user-v1", login, ABOUT_OPERATION, &variables, parse_user)
            .await
    }

    async fn cached<T: Serialize + DeserializeOwned>(
        &self,
        kind: &str,
        key: &str,
        operation: &str,
        variables: &Value,
        parse: impl FnOnce(&Value) -> Result<T>,
    ) -> Result<T> {
        let cache = crate::cache::Cache::new("twitch");
        if let Some(cache) = &cache
            && let Some(value) = cache.get(kind, key).await
        {
            return Ok(value);
        }
        let value = parse(&self.query(operation, variables).await?)?;
        if let Some(cache) = cache {
            cache.put(kind, key, &value, SEARCH_TTL);
        }
        Ok(value)
    }

    async fn query(&self, operation: &str, variables: &Value) -> Result<Value> {
        let known = {
            let mut operations = self.operations.lock().await;
            self.ensure(&mut operations)
                .await?
                .hashes
                .get(operation)
                .cloned()
        };
        if let Some(hash) = &known
            && let Some(data) = self.post(operation, hash, variables).await?
        {
            return Ok(data);
        }
        let hash = self
            .refresh()
            .await
            .context(UNAVAILABLE)?
            .hashes
            .get(operation)
            .cloned();
        match hash {
            Some(hash) if known.as_ref() != Some(&hash) => self
                .post(operation, &hash, variables)
                .await?
                .context(UNAVAILABLE),
            _ => bail!(UNAVAILABLE),
        }
    }

    async fn post(&self, operation: &str, hash: &str, variables: &Value) -> Result<Option<Value>> {
        let body = json!({
            "operationName": operation,
            "variables": variables,
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
        });
        let response = self
            .http
            .post(GQL_URL)
            .header("Client-Id", CLIENT_ID)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("Could not reach Twitch. Check your connection and try again.")?;
        let bytes = crate::github::read_limited(response, RESPONSE_LIMIT).await?;
        read_reply(serde_json::from_slice(&bytes).context("Could not read Twitch's response.")?)
    }

    async fn ensure<'a>(&self, slot: &'a mut Option<Operations>) -> Result<&'a Operations> {
        if slot.is_none() {
            *slot = load(&self.path);
        }
        if slot.is_none() {
            let (etag, hashes) = self
                .download(None)
                .await
                .context("Could not download Twitch's query list.")?
                .context("GitHub sent no query list.")?;
            let operations = Operations {
                etag,
                checked_at: now(),
                hashes,
            };
            save(&self.path, &operations)?;
            *slot = Some(operations);
        }
        slot.as_ref().context("Twitch's query list is missing.")
    }

    /// Picks up a newer query list from GitHub, if one has been published.
    async fn refresh(&self) -> Result<tokio::sync::MappedMutexGuard<'_, Operations>> {
        let mut guard = self.operations.lock().await;
        self.ensure(&mut guard).await?;
        let operations = guard.as_mut().context("Twitch's query list is missing.")?;
        if now().saturating_sub(operations.checked_at) >= CHECK_INTERVAL {
            operations.checked_at = now();
            if let Some((etag, hashes)) = self.download(operations.etag.as_deref()).await? {
                operations.etag = etag;
                operations.hashes = hashes;
            }
            save(&self.path, operations)?;
        }
        tokio::sync::MutexGuard::try_map(guard, Option::as_mut)
            .ok()
            .context("Twitch's query list is missing.")
    }

    /// Returns `None` when GitHub answers 304 Not Modified.
    async fn download(
        &self,
        etag: Option<&str>,
    ) -> Result<Option<(Option<String>, BTreeMap<String, String>)>> {
        let mut request = self.http.get(OPERATIONS_URL);
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = crate::github::read_limited(response, OPERATIONS_LIMIT).await?;
        let hashes = parse_operations(&String::from_utf8_lossy(&bytes));
        if hashes.is_empty() {
            bail!("Twitch's query list has an unexpected format.");
        }
        Ok(Some((etag, hashes)))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

fn load(path: &Path) -> Option<Operations> {
    let operations: Operations = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (!operations.hashes.is_empty()).then_some(operations)
}

fn save(path: &Path, operations: &Operations) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Could not create the SlintYT config folder.")?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(operations)?)
        .context("Could not save Twitch's query list.")?;
    fs::rename(temporary, path).context("Could not save Twitch's query list.")?;
    Ok(())
}

/// Reads lines such as `export const NameQuery = ["Name", "<sha256>"] as const;`.
fn parse_operations(source: &str) -> BTreeMap<String, String> {
    source
        .lines()
        .filter(|line| line.starts_with("export const "))
        .filter_map(|line| {
            let (_, rest) = line.split_once("= [\"")?;
            let (name, rest) = rest.split_once("\", \"")?;
            let (hash, _) = rest.split_once('"')?;
            (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| (name.to_owned(), hash.to_owned()))
        })
        .collect()
}

/// Returns `None` when Twitch no longer knows the hash.
fn read_reply(reply: Value) -> Result<Option<Value>> {
    let errors = reply["errors"].as_array();
    if errors.is_some_and(|errors| {
        errors.iter().any(|error| {
            error["message"] == "PersistedQueryNotFound"
                || error["extensions"]["code"] == "PERSISTED_QUERY_NOT_FOUND"
        })
    }) {
        return Ok(None);
    }
    if errors.is_some_and(|errors| {
        errors
            .iter()
            .any(|e| e["extensions"]["code"] == "IntegrityCheckFailed")
    }) {
        bail!("Twitch rejected the chat request (integrity check). Please retry later.");
    }
    if reply["data"].is_object() {
        return Ok(Some(reply["data"].clone()));
    }
    let message = errors
        .and_then(|errors| errors.first())
        .and_then(|error| error["message"].as_str())
        .unwrap_or("Twitch sent an unexpected response.");
    bail!("Twitch search failed: {message}")
}

fn parse_search(data: &Value, mode: SearchMode) -> Result<SearchPage> {
    let section = &data["searchFor"][mode.field()];
    let edges = section["edges"]
        .as_array()
        .context("Twitch sent an unexpected search result.")?;
    let results: Vec<_> = edges
        .iter()
        .filter_map(|edge| match mode {
            SearchMode::Video => video_result(&edge["item"]),
            SearchMode::User => user_result(&edge["item"]),
        })
        .collect();
    let cursor = section["cursor"]
        .as_str()
        .filter(|cursor| !cursor.is_empty() && !results.is_empty())
        .map(str::to_owned);
    Ok(SearchPage {
        results,
        cursor,
        total: section["totalMatches"].as_u64(),
    })
}

fn parse_videos(data: &Value) -> Result<SearchPage> {
    let videos = &data["user"]["videos"];
    if data["user"].is_null() {
        bail!("This Twitch user could not be found.");
    }
    let edges = videos["edges"]
        .as_array()
        .context("Twitch sent an unexpected video list.")?;
    let cursor = edges
        .last()
        .filter(|_| videos["pageInfo"]["hasNextPage"] == true)
        .and_then(|edge| edge["cursor"].as_str())
        .map(str::to_owned);
    let results = edges
        .iter()
        .filter_map(|edge| video_result(&edge["node"]))
        .collect();
    Ok(SearchPage {
        results,
        cursor,
        total: None,
    })
}

fn parse_user(data: &Value) -> Result<UserInfo> {
    let user = &data["user"];
    if user.is_null() {
        bail!("This Twitch user could not be found.");
    }
    Ok(UserInfo {
        description: text(&user["description"]),
        followers: followers(user["followers"]["totalCount"].as_u64().unwrap_or(0)),
    })
}

pub fn followers(count: u64) -> String {
    format!(
        "{} {}",
        compact(count),
        if count == 1 { "follower" } else { "followers" }
    )
}

/// Search has no server-side sort, so what's loaded so far gets sorted here.
/// The first choice keeps Twitch's own order.
pub fn sorted(results: &[SearchResult], users: bool, sort: i32) -> Vec<SearchResult> {
    let mut results = results.to_vec();
    match (users, sort) {
        (true, 1) | (false, 1) => results.sort_by_key(|r| std::cmp::Reverse(r.count)),
        (true, 2) => results.sort_by_key(|r| r.title.to_lowercase()),
        (false, 2) => {
            results.sort_by(|a, b| (b.published_at, &b.date).cmp(&(a.published_at, &a.date)))
        }
        (false, 3) => results.sort_by_key(|r| std::cmp::Reverse(r.seconds)),
        _ => {}
    }
    results
}

/// Twitch only searches a creator's videos for signed-in owners, so a user's
/// videos and the local bookmarks are filtered here. Every word has to appear
/// in the title, channel or game.
pub fn matches_filter(result: &SearchResult, filter: &str) -> bool {
    let haystack =
        format!("{} {} {}", result.title, result.subtitle, result.metadata).to_lowercase();
    filter
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

fn video_result(item: &Value) -> Option<SearchResult> {
    let id = item["id"].as_str().filter(|id| !id.is_empty())?;
    let picture = item["templatePreviewThumbnailURL"]
        .as_str()
        .map(|url| url.replace("{width}", "320").replace("{height}", "180"))
        .or_else(|| item["previewThumbnailURL"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let views = item["viewCount"].as_u64().unwrap_or(0);
    let created = item["createdAt"]
        .as_str()
        .or_else(|| item["publishedAt"].as_str())
        .unwrap_or_default();
    let date = created.get(..10).unwrap_or_default().to_owned();
    let parts = [
        item["game"]["displayName"].as_str().map(str::to_owned),
        Some(format!(
            "{} {}",
            compact(views),
            if views == 1 { "view" } else { "views" }
        )),
        Some(date.clone()).filter(|date| !date.is_empty()),
    ];
    let seconds = item["lengthSeconds"].as_u64().unwrap_or(0);
    Some(SearchResult {
        id: id.into(),
        title: text(&item["title"]),
        subtitle: text(&item["owner"]["displayName"]),
        avatar: text(&item["owner"]["profileImageURL"]),
        metadata: parts.into_iter().flatten().collect::<Vec<_>>().join(" · "),
        duration: clock(seconds),
        picture,
        count: views,
        seconds,
        date,
        game: text(&item["game"]["displayName"]),
        channel: text(&item["owner"]["login"]),
        published_at: timestamp(created).unwrap_or(0),
        kind: String::new(),
    })
}

fn user_result(item: &Value) -> Option<SearchResult> {
    let login = item["login"].as_str().filter(|login| !login.is_empty())?;
    let count = item["followers"]["totalCount"].as_u64().unwrap_or(0);
    let title = text(&item["displayName"]);
    Some(SearchResult {
        id: login.into(),
        title: if title.is_empty() {
            login.into()
        } else {
            title
        },
        subtitle: format!("@{login} · {}", followers(count)),
        // The channel's own description, or else its stream title.
        metadata: Some(text(&item["description"]))
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| text(&item["broadcastSettings"]["title"])),
        picture: text(&item["profileImageURL"]),
        count,
        ..SearchResult::default()
    })
}

/// Reads Twitch's UTC times, such as `2026-11-05T16:03:19Z`, as Unix seconds.
pub fn timestamp(time: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| time.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days since 1970-01-01, from Howard Hinnant's days_from_civil.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second.min(59)).ok()
}

/// "3 days ago" and the like, or nothing for an unknown time.
pub fn ago(time: u64, now: u64) -> String {
    if time == 0 {
        return String::new();
    }
    let seconds = now.saturating_sub(time);
    let (count, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3600 => (seconds / 60, "minute"),
        3600..86_400 => (seconds / 3600, "hour"),
        86_400..2_592_000 => (seconds / 86_400, "day"),
        2_592_000..31_536_000 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

/// The Length filter's choices: any, under 30 minutes, 30 minutes to 2 hours, over 2 hours.
pub fn length_matches(seconds: u64, filter: i32) -> bool {
    match filter {
        1 => seconds < 30 * 60,
        2 => (30 * 60..2 * 3600).contains(&seconds),
        3 => seconds >= 2 * 3600,
        _ => true,
    }
}

pub fn views(count: u64) -> String {
    format!(
        "{} {}",
        compact(count),
        if count == 1 { "view" } else { "views" }
    )
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn compact(count: u64) -> String {
    let scaled = |divisor: f64, suffix: &str| {
        let value = format!("{:.1}", count as f64 / divisor);
        format!("{}{suffix}", value.trim_end_matches(".0"))
    };
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => scaled(1e3, "K"),
        _ => scaled(1e6, "M"),
    }
}

fn clock(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operation_lines() {
        let hash = "a".repeat(64);
        let source = format!(
            "export const FooQuery = [\"Foo_Bar\", \"{hash}\"] as const;\n\
             export const ShortQuery = [\"Short\", \"abc\"] as const;\n\
             export type Foo = string;\n"
        );
        let hashes = parse_operations(&source);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes["Foo_Bar"], hash);
    }

    #[test]
    fn saved_operations_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/twitch-operations.json");
        assert!(load(&path).is_none());
        let operations = Operations {
            etag: Some("\"abc\"".into()),
            checked_at: 5,
            hashes: BTreeMap::from([("Foo".into(), "hash".into())]),
        };
        save(&path, &operations).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.etag.as_deref(), Some("\"abc\""));
        assert_eq!(loaded.checked_at, 5);
        assert_eq!(loaded.hashes["Foo"], "hash");
    }

    #[test]
    fn detects_a_rejected_hash() {
        let reply = json!({ "errors": [{ "message": "PersistedQueryNotFound" }] });
        assert_eq!(read_reply(reply).unwrap(), None);
        let reply = json!({ "errors": [{ "message": "x", "extensions": { "code": "PERSISTED_QUERY_NOT_FOUND" } }] });
        assert_eq!(read_reply(reply).unwrap(), None);
        assert!(read_reply(json!({ "errors": [{ "message": "service error" }] })).is_err());
        let data = json!({ "data": { "searchFor": null } });
        assert!(read_reply(data).unwrap().is_some());
    }

    #[test]
    fn parses_videos() {
        let data = json!({ "searchFor": { "videos": { "cursor": "MTU=", "totalMatches": 170, "edges": [{ "item": {
            "id": "42", "title": "Any% run", "lengthSeconds": 6631, "viewCount": 12500,
            "createdAt": "2026-11-04T17:32:28Z",
            "owner": { "displayName": "Runner", "login": "runner", "profileImageURL": "https://example.com/runner.png" }, "game": { "displayName": "Celeste" },
            "templatePreviewThumbnailURL": "https://static-cdn.jtvnw.net/t-{width}x{height}.jpg",
        } }, { "item": { "title": "No ID" } }] } } });
        let page = parse_search(&data, SearchMode::Video).unwrap();
        assert_eq!(page.cursor.as_deref(), Some("MTU="));
        assert_eq!(page.total, Some(170));
        assert_eq!(
            page.results,
            [SearchResult {
                id: "42".into(),
                title: "Any% run".into(),
                subtitle: "Runner".into(),
                metadata: "Celeste · 12.5K views · 2026-11-04".into(),
                duration: "1:50:31".into(),
                picture: "https://static-cdn.jtvnw.net/t-320x180.jpg".into(),
                avatar: "https://example.com/runner.png".into(),
                count: 12500,
                seconds: 6631,
                date: "2026-11-04".into(),
                game: "Celeste".into(),
                channel: "runner".into(),
                published_at: 1_793_813_548,
                kind: String::new(),
            }]
        );
    }

    #[test]
    fn parses_users_and_empty_pages() {
        let data = json!({ "searchFor": { "channels": { "cursor": "", "edges": [{ "item": {
            "id": "7", "login": "runner", "displayName": "Runner",
            "followers": { "totalCount": 2000000 },
            "broadcastSettings": { "title": "Live now" }, "profileImageURL": "https://a/b.png",
        } }] } } });
        let page = parse_search(&data, SearchMode::User).unwrap();
        assert_eq!(page.cursor, None);
        assert_eq!(page.results[0].subtitle, "@runner · 2M followers");
        assert_eq!(page.results[0].id, "runner");
        let data = json!({ "searchFor": { "channels": { "cursor": "x", "edges": [] } } });
        assert_eq!(
            parse_search(&data, SearchMode::User).unwrap(),
            SearchPage::default()
        );
        assert!(parse_search(&json!({ "searchFor": null }), SearchMode::User).is_err());
    }

    #[test]
    fn parses_user_videos_and_their_next_page() {
        let node = json!({ "id": "1", "title": "VOD", "publishedAt": "2026-01-02T00:00:00Z" });
        let data = json!({ "user": { "videos": {
            "edges": [{ "cursor": "next", "node": node }],
            "pageInfo": { "hasNextPage": true },
        } } });
        let page = parse_videos(&data).unwrap();
        assert_eq!(page.cursor.as_deref(), Some("next"));
        assert_eq!(page.results[0].date, "2026-01-02");
        let data = json!({ "user": { "videos": {
            "edges": [{ "cursor": "next", "node": node }],
            "pageInfo": { "hasNextPage": false },
        } } });
        assert_eq!(parse_videos(&data).unwrap().cursor, None);
        assert!(parse_videos(&json!({ "user": null })).is_err());
    }

    #[test]
    fn parses_user_info() {
        let data = json!({ "user": { "description": "Hi", "followers": { "totalCount": 1 } } });
        assert_eq!(parse_user(&data).unwrap().followers, "1 follower");
        assert!(parse_user(&json!({ "user": null })).is_err());
    }

    #[test]
    fn sorts_and_filters_locally() {
        let result = |title: &str, count, seconds, date: &str| SearchResult {
            title: title.into(),
            metadata: "Celeste".into(),
            count,
            seconds,
            date: date.into(),
            ..SearchResult::default()
        };
        let all = [
            result("b", 1, 30, "2026-01-02"),
            result("A", 3, 10, "2026-01-01"),
            result("c", 2, 20, "2026-01-03"),
        ];
        let titles = |users, sort| -> String {
            sorted(&all, users, sort)
                .iter()
                .map(|r| r.title.as_str())
                .collect()
        };
        assert_eq!(titles(false, 0), "bAc");
        assert_eq!(titles(false, 1), "Acb");
        assert_eq!(titles(false, 2), "cbA");
        assert_eq!(titles(false, 3), "bcA");
        assert_eq!(titles(true, 1), "Acb");
        assert_eq!(titles(true, 2), "Abc");
        assert!(matches_filter(&all[1], ""));
        assert!(matches_filter(&all[1], "a CELESTE"));
        assert!(!matches_filter(&all[1], "a portal"));
    }

    #[test]
    fn reads_twitch_times_and_says_how_long_ago() {
        assert_eq!(timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(timestamp("2026-11-04T17:32:28Z"), Some(1_793_813_548));
        assert_eq!(timestamp("2024-02-29T12:00:00.5Z"), Some(1_709_208_000));
        assert_eq!(timestamp("2026-13-01T00:00:00Z"), None);
        assert_eq!(timestamp("yesterday"), None);
        let now = 1_000_000_000;
        assert_eq!(ago(now - 30, now), "just now");
        assert_eq!(ago(now - 3600, now), "1 hour ago");
        assert_eq!(ago(now - 2 * 86_400, now), "2 days ago");
        assert_eq!(ago(now - 400 * 86_400, now), "1 year ago");
        assert_eq!(ago(0, now), "");
    }

    #[test]
    fn filters_videos_by_length() {
        assert!(length_matches(100_000, 0));
        assert!(length_matches(29 * 60, 1) && !length_matches(30 * 60, 1));
        assert!(length_matches(30 * 60, 2) && !length_matches(2 * 3600, 2));
        assert!(length_matches(2 * 3600, 3) && !length_matches(3599, 3));
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1000), "1K");
        assert_eq!(compact(2_075_647), "2.1M");
        assert_eq!(clock(59), "0:59");
        assert_eq!(clock(3661), "1:01:01");
    }
}
