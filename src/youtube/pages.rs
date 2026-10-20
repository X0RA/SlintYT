use super::*;
use rustypipe::{
    model::{Channel, ChannelItem, Thumbnail, VideoItem},
    param::{ChannelOrder, ChannelVideoTab},
};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub metadata: String,
    pub description: String,
    pub banner: String,
    pub avatar: String,
    pub has_live: bool,
    pub subscribed: bool,
    pub links: Vec<(String, String)>,
}

pub fn number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.)
    } else {
        n.to_string()
    }
}
/// Picks the smallest avatar at least 160px wide, matching `youtube::avatar`.
pub fn avatar_url(items: &[Thumbnail]) -> String {
    let url = items
        .iter()
        .filter(|i| i.width >= 160)
        .min_by_key(|i| i.width)
        .or_else(|| items.iter().max_by_key(|i| i.width))
        .map(|i| i.url.as_str())
        .unwrap_or_default();
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.into()
    }
}
fn image(items: &[Thumbnail]) -> String {
    items
        .iter()
        .filter(|i| i.width <= 1280)
        .max_by_key(|i| i.width)
        .or_else(|| items.first())
        .map(|i| i.url.clone())
        .unwrap_or_default()
}
pub fn item(v: VideoItem) -> Video {
    let (channel, channel_id, avatar) = v
        .channel
        .map(|c| (c.name, c.id, avatar_url(&c.avatar)))
        .unwrap_or_default();
    let metadata = [
        v.view_count
            .map(|n| format!("{} views", number(n)))
            .unwrap_or_default(),
        v.publish_date_txt.unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    Video {
        id: v.id,
        title: v.name,
        channel,
        channel_id,
        avatar,
        metadata,
        duration: if v.is_live {
            "LIVE".into()
        } else {
            v.duration
                .map(|s| {
                    if s >= 3600 {
                        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
                    } else {
                        format!("{}:{:02}", s / 60, s % 60)
                    }
                })
                .unwrap_or_default()
        },
        thumbnail: image(&v.thumbnail),
        ..Default::default()
    }
}
fn channel_item(c: ChannelItem) -> Video {
    Video {
        id: c.id.clone(),
        channel_id: c.id,
        avatar: avatar_url(&c.avatar),
        title: c.name,
        metadata: c
            .subscriber_count
            .map(|n| format!("{} subscribers", number(n)))
            .unwrap_or(c.handle.unwrap_or_default()),
        subscribed: true,
        ..Default::default()
    }
}
fn channel<T>(c: &Channel<T>) -> ChannelInfo {
    ChannelInfo {
        id: c.id.clone(),
        name: c.name.clone(),
        metadata: [
            c.handle.clone().unwrap_or_default(),
            c.subscriber_count
                .map(|n| format!("{} subscribers", number(n)))
                .unwrap_or_default(),
            c.video_count
                .map(|n| format!("{n} videos"))
                .unwrap_or_default(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · "),
        description: c.description.clone(),
        banner: image(&c.banner),
        avatar: avatar_url(&c.avatar),
        has_live: c.has_live,
        ..Default::default()
    }
}
fn videos(p: Paginator<VideoItem>) -> Page {
    Page {
        videos: p.items.into_iter().map(item).collect(),
        continuation: p.ctoken,
        visitor_data: p.visitor_data,
        ..Default::default()
    }
}
fn playlists(p: Paginator<PlaylistItem>) -> Page {
    Page {
        videos: p
            .items
            .into_iter()
            .map(|i| Video {
                id: i.id,
                title: i.name,
                metadata: i
                    .video_count
                    .map(|n| format!("{n} videos"))
                    .unwrap_or_default(),
                channel: i.channel.map(|c| c.name).unwrap_or_default(),
                playlist: true,
                thumbnail: image(&i.thumbnail),
                ..Default::default()
            })
            .collect(),
        continuation: p.ctoken,
        visitor_data: p.visitor_data,
        ..Default::default()
    }
}
impl Session {
    pub(super) async fn extra_page(
        &self,
        view: &View,
        token: Option<&str>,
        visitor: Option<&str>,
    ) -> Result<Page> {
        let q = self.client.query().authenticated();
        match view {
            View::Subscriptions => {
                let body = token
                    .map(|t| json!({"continuation":t}))
                    .unwrap_or_else(|| json!({"browseId":"FEsubscriptions"}));
                let raw = q
                    .clone()
                    .visitor_data_opt(visitor)
                    .raw(ClientType::Desktop, "browse", &body)
                    .await
                    .ok();
                if let Some(raw) = raw
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                {
                    let content = super::browse_primary(&raw["contents"], super::ParseMode::Feed)
                        .unwrap_or(&raw);
                    let mut items = super::renderers::cards(content);
                    if !items.is_empty() {
                        for item in &mut items {
                            item.group = if item.metadata.contains("hour")
                                || item.metadata.contains("minute")
                                || (1..=6).any(|n| item.metadata.contains(&format!("{n} day")))
                            {
                                "This week".into()
                            } else {
                                "Earlier".into()
                            };
                        }
                        return Ok(Page {
                            videos: items,
                            continuation: super::find_renderer(content, "continuationItemRenderer")
                                .and_then(|v| super::find_renderer(v, "continuationCommand"))
                                .and_then(|v| v["token"].as_str())
                                .map(str::to_owned),
                            visitor_data: raw
                                .pointer("/responseContext/visitorData")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            ..Default::default()
                        });
                    }
                }
                let result: Paginator<VideoItem> = if let Some(token) = token {
                    q.continuation(token, ContinuationEndpoint::Browse, visitor)
                        .await
                } else {
                    q.subscription_feed().await
                }
                .map_err(|_| anyhow::anyhow!("Could not load subscriptions. Please retry."))?;
                let mut page = videos(result);
                for v in &mut page.videos {
                    v.group = if v.metadata.contains("hour")
                        || v.metadata.contains("minute")
                        || (1..=6).any(|n| v.metadata.contains(&format!("{n} day")))
                    {
                        "This week".into()
                    } else {
                        "Earlier".into()
                    };
                }
                Ok(page)
            }
            View::Channels => {
                let p: Paginator<ChannelItem> = if let Some(t) = token {
                    q.continuation(t, ContinuationEndpoint::Browse, visitor)
                        .await
                } else {
                    q.subscriptions().await
                }
                .map_err(|_| anyhow::anyhow!("Could not load your channels. Please retry."))?;
                if token.is_none() {
                    self.known_subscriptions.write().unwrap().clear();
                }
                for channel in &p.items {
                    self.remember_subscription(&channel.id, true);
                }
                Ok(Page {
                    videos: p.items.into_iter().map(channel_item).collect(),
                    continuation: p.ctoken,
                    visitor_data: p.visitor_data,
                    ..Default::default()
                })
            }
            View::Channel {
                id,
                tab,
                sort,
                query,
            } => {
                if !id.starts_with("UC")
                    || id.len() != 24
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                {
                    bail!("Invalid YouTube channel.");
                }
                if let Some(t) = token {
                    return if *tab == 2 {
                        q.continuation(t, ContinuationEndpoint::Browse, visitor)
                            .await
                            .map(playlists)
                    } else {
                        q.continuation(t, ContinuationEndpoint::Browse, visitor)
                            .await
                            .map(videos)
                    }
                    .map_err(|_| anyhow::anyhow!("Could not load more channel content."));
                }
                let params = if !query.is_empty() {
                    "EgZzZWFyY2jyBgQKAloA"
                } else if *tab == 1 {
                    "EgdzdHJlYW1z8gYECgJ6AA=="
                } else if *tab == 2 {
                    "EglwbGF5bGlzdHMgAQ=="
                } else {
                    "EgZ2aWRlb3PyBgQKAjoA"
                };
                let mut body = json!({"browseId":id,"params":params});
                if !query.is_empty() {
                    body["query"] = json!(query);
                }
                let raw = q.raw(ClientType::Desktop, "browse", &body).await.ok();
                let mut raw = raw
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok());
                if query.is_empty() && (*tab == 1 || *tab == 2) {
                    let action = raw.as_ref().and_then(|v| {
                        super::renderers::channel_tab_action(
                            v,
                            if *tab == 1 { "Live" } else { "Playlists" },
                        )
                    });
                    if let Some((false, mut action)) = action {
                        if action.get("browseId").is_none() {
                            action["browseId"] = json!(id);
                        }
                        if let Ok(response) = q.raw(ClientType::Desktop, "browse", &action).await {
                            raw = serde_json::from_str(&response).ok();
                        }
                    }
                }
                if !self.known_subscriptions.read().unwrap().contains_key(id)
                    && let Ok(subscriptions) = q.subscriptions().await
                {
                    for item in subscriptions.items {
                        self.remember_subscription(&item.id, true);
                    }
                    if subscriptions.ctoken.is_none()
                        && !self.known_subscriptions.read().unwrap().contains_key(id)
                    {
                        self.remember_subscription(id, false);
                    }
                }
                let mut page = if *tab == 2 {
                    let c = q
                        .channel_playlists(id)
                        .await
                        .map_err(|_| anyhow::anyhow!("Could not load channel playlists."))?;
                    let info = channel(&c);
                    let mut p = playlists(c.content);
                    p.channel_info = Some(info);
                    p
                } else {
                    let c = if !query.is_empty() {
                        q.channel_search(id, query).await
                    } else {
                        q.channel_videos_tab(
                            id,
                            if *tab == 1 {
                                ChannelVideoTab::Live
                            } else {
                                ChannelVideoTab::Videos
                            },
                        )
                        .await
                    }
                    .map_err(|_| anyhow::anyhow!("Could not load this channel. Please retry."))?;
                    let info = channel(&c);
                    let mut p = videos(c.content);
                    p.channel_info = Some(info);
                    p
                };
                if let Some(raw) = &raw {
                    if let Some(info) = page.channel_info.as_mut() {
                        let metadata = super::renderers::channel_metadata(raw);
                        if !metadata.is_empty() {
                            info.metadata = metadata;
                        }
                        info.subscribed = self
                            .known_subscriptions
                            .read()
                            .unwrap()
                            .get(id)
                            .copied()
                            .or_else(|| super::renderers::subscription(raw))
                            .unwrap_or(false);
                    }
                    if *sort == 0 || !query.is_empty() || *tab == 2 {
                        let content =
                            super::browse_primary(&raw["contents"], super::ParseMode::Feed)
                                .unwrap_or(&raw["contents"]);
                        let mut cards = super::renderers::cards(content);
                        if *tab == 2 {
                            cards.retain(|v| v.playlist);
                        }
                        if !cards.is_empty() || *tab == 2 {
                            page.videos = cards;
                            page.continuation =
                                super::find_renderer(content, "continuationItemRenderer")
                                    .and_then(|v| super::find_renderer(v, "continuationCommand"))
                                    .and_then(|v| v["token"].as_str())
                                    .map(str::to_owned);
                            page.visitor_data = raw
                                .pointer("/responseContext/visitorData")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                    }
                }
                if *sort > 0 && query.is_empty() && *tab != 2 {
                    let action = raw.as_ref().and_then(|r| {
                        super::renderers::sort_action(
                            r,
                            if *sort == 1 { "Popular" } else { "Oldest" },
                        )
                    });
                    if let Some(action) = action {
                        let response = q
                            .raw(ClientType::Desktop, "browse", &action)
                            .await
                            .map_err(|_| anyhow::anyhow!("Could not sort this channel."))?;
                        let value: Value = serde_json::from_str(&response)?;
                        let sorted = super::parse_home(&response).ok();
                        let cards = sorted
                            .as_ref()
                            .map(|p| p.videos.clone())
                            .unwrap_or_else(|| super::renderers::cards(&value));
                        if cards.is_empty() {
                            bail!("No videos were returned for this sort. Please retry.");
                        }
                        page.videos = cards;
                        page.continuation = sorted.and_then(|p| p.continuation).or_else(|| {
                            super::find_renderer(&value, "continuationItemRenderer")
                                .and_then(|v| super::find_renderer(v, "continuationCommand"))
                                .and_then(|v| v["token"].as_str())
                                .map(str::to_owned)
                        });
                        page.visitor_data = value
                            .pointer("/responseContext/visitorData")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    } else {
                        let sorted = q
                            .channel_videos_tab_order(
                                id,
                                if *tab == 1 {
                                    ChannelVideoTab::Live
                                } else {
                                    ChannelVideoTab::Videos
                                },
                                if *sort == 1 {
                                    ChannelOrder::Popular
                                } else {
                                    ChannelOrder::Oldest
                                },
                            )
                            .await
                            .map_err(|_| anyhow::anyhow!("Could not sort this channel."))?;
                        let info = page.channel_info.take();
                        page = videos(sorted);
                        page.channel_info = info;
                    }
                }
                if let Ok(about) = q.channel_info(id).await
                    && let Some(info) = page.channel_info.as_mut()
                {
                    info.links = about.links;
                }
                if let Some(info) = &page.channel_info {
                    for video in &mut page.videos {
                        if !video.playlist {
                            video.channel = info.name.clone();
                            video.channel_id = info.id.clone();
                        }
                    }
                }
                Ok(page)
            }
            _ => unreachable!(),
        }
    }
    pub(crate) fn remember_subscription(&self, id: &str, subscribed: bool) {
        self.known_subscriptions
            .write()
            .unwrap()
            .insert(id.to_owned(), subscribed);
    }
    pub fn client(&self) -> &RustyPipe {
        &self.client
    }
}
