//! Preserve navigation and action data omitted by the typed response models.
use super::*;

/// Read video/playlist cards only from a content subtree, skipping advertisements.
pub(crate) fn cards(value: &Value) -> Vec<Video> {
    fn visit(value: &Value, out: &mut Vec<Video>) {
        match value {
            Value::Object(map) => {
                if map.keys().any(|k| {
                    matches!(
                        k.as_str(),
                        "adSlotRenderer" | "promotedSparklesWebRenderer" | "promotedVideoRenderer"
                    )
                }) {
                    return;
                }
                let card = map
                    .get("videoRenderer")
                    .or_else(|| map.get("compactVideoRenderer"))
                    .or_else(|| map.get("gridVideoRenderer"))
                    .and_then(super::legacy_video)
                    .or_else(|| map.get("lockupViewModel").and_then(super::lockup_video))
                    .or_else(|| {
                        map.get("playlistRenderer")
                            .or_else(|| map.get("gridPlaylistRenderer"))
                            .and_then(super::legacy_playlist)
                    });
                if let Some(card) = card {
                    if !out
                        .iter()
                        .any(|v| v.id == card.id && v.playlist == card.playlist)
                    {
                        out.push(card);
                    }
                    return;
                }
                for child in map.values() {
                    visit(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    visit(item, out);
                }
            }
            _ => {}
        }
    }
    let mut result = Vec::new();
    visit(value, &mut result);
    result
}

pub(crate) fn recommendations(value: &Value) -> Vec<Video> {
    let root = value
        .pointer("/contents/twoColumnWatchNextResults/secondaryResults/secondaryResults/results")
        .or_else(|| {
            value.pointer("/contents/singleColumnWatchNextResults/results/results/contents")
        });
    root.map(cards).unwrap_or_default()
}

pub(crate) fn channel_metadata(value: &Value) -> String {
    fn visit(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(parts) = map.get("metadataParts").and_then(Value::as_array) {
                    for part in parts {
                        let text = super::text(&part["text"]);
                        if (text.starts_with('@')
                            || text.contains(" subscribers")
                            || text.contains(" videos"))
                            && !out.contains(&text)
                        {
                            out.push(text);
                        }
                    }
                }
                for child in map.values() {
                    visit(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    visit(item, out);
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    visit(&value["header"], &mut parts);
    parts.join(" · ")
}

pub(crate) fn subscription(value: &Value) -> Option<bool> {
    fn visit(value: &Value) -> Option<bool> {
        if let Some(v) = value
            .get("subscribed")
            .or_else(|| value.get("isSubscribed"))
            .and_then(Value::as_bool)
        {
            return Some(v);
        }
        let text = super::text(value);
        if text == "Subscribed" {
            return Some(true);
        }
        match value {
            Value::Object(map) => map.values().find_map(visit),
            Value::Array(items) => items.iter().find_map(visit),
            _ => None,
        }
    }
    visit(&value["header"])
}

pub(crate) fn sort_action(value: &Value, label: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if ["text", "title", "label"]
                .iter()
                .any(|k| super::text(&value[*k]).eq_ignore_ascii_case(label))
            {
                if let Some(token) = super::find_renderer(value, "continuationCommand")
                    .and_then(|v| v["token"].as_str())
                {
                    return Some(json!({"continuation":token}));
                }
                if let Some(endpoint) = super::find_renderer(value, "browseEndpoint") {
                    return Some(endpoint.clone());
                }
            }
            map.values().find_map(|v| sort_action(v, label))
        }
        Value::Array(items) => items.iter().find_map(|v| sort_action(v, label)),
        _ => None,
    }
}

pub(crate) fn channel_tab_action(value: &Value, label: &str) -> Option<(bool, Value)> {
    match value {
        Value::Object(map) => {
            if let Some(tab) = map.get("tabRenderer")
                && super::text(&tab["title"]) == label
            {
                let mut endpoint = tab.pointer("/endpoint/browseEndpoint")?.clone();
                if let Some(params) = endpoint["params"].as_str() {
                    endpoint["params"] = json!(params.replace("%3D", "=").replace("%3d", "="));
                }
                return Some((tab["selected"].as_bool().unwrap_or(false), endpoint));
            }
            map.values().find_map(|v| channel_tab_action(v, label))
        }
        Value::Array(items) => items.iter().find_map(|v| channel_tab_action(v, label)),
        _ => None,
    }
}

#[derive(Clone, Default)]
pub(crate) struct CommentActions {
    pub like: Option<Value>,
    pub unlike: Option<Value>,
    pub reply: Option<String>,
    pub liked: bool,
    pub liked_count: String,
    pub unliked_count: String,
    pub avatar: String,
}

pub(crate) fn comment_actions(raw: &Value, id: &str) -> CommentActions {
    fn view<'a>(value: &'a Value, id: &str) -> Option<&'a Value> {
        if value.get("commentId").and_then(Value::as_str) == Some(id)
            && (value.get("toolbarSurfaceKey").is_some() || value.get("actionButtons").is_some())
        {
            return Some(value);
        }
        match value {
            Value::Object(map) => map.values().find_map(|v| view(v, id)),
            Value::Array(items) => items.iter().find_map(|v| view(v, id)),
            _ => None,
        }
    }
    let Some(view) = view(raw, id) else {
        return CommentActions::default();
    };
    let mutations = raw
        .pointer("/frameworkUpdates/entityBatchUpdate/mutations")
        .and_then(Value::as_array);
    let mutation = |key: &str, payload: &str| {
        mutations
            .and_then(|items| {
                items.iter().find(|m| {
                    view[key].is_string()
                        && (m["entityKey"].as_str() == view[key].as_str()
                            || m["payload"][payload]["key"].as_str() == view[key].as_str())
                })
            })
            .map(|m| &m["payload"][payload])
    };
    let surface = mutation("toolbarSurfaceKey", "engagementToolbarSurfaceEntityPayload");
    let state = mutation("toolbarStateKey", "engagementToolbarStateEntityPayload");
    let entity = mutation("commentKey", "commentEntityPayload");
    let like = surface
        .and_then(|s| super::find_renderer(&s["likeCommand"], "performCommentActionEndpoint"))
        .cloned()
        .or_else(|| {
            super::find_renderer(view, "toggleButtonRenderer")
                .and_then(|v| {
                    super::find_renderer(
                        &v["defaultServiceEndpoint"],
                        "performCommentActionEndpoint",
                    )
                })
                .cloned()
        });
    let unlike = surface
        .and_then(|s| super::find_renderer(&s["unlikeCommand"], "performCommentActionEndpoint"))
        .cloned()
        .or_else(|| {
            super::find_renderer(view, "toggleButtonRenderer")
                .and_then(|v| {
                    super::find_renderer(
                        &v["toggledServiceEndpoint"],
                        "performCommentActionEndpoint",
                    )
                })
                .cloned()
        });
    let reply = surface
        .and_then(|s| super::find_renderer(&s["replyCommand"], "createCommentReplyEndpoint"))
        .or_else(|| super::find_renderer(view, "createCommentReplyEndpoint"))
        .and_then(|e| e["createReplyParams"].as_str())
        .map(str::to_owned);
    CommentActions {
        like,
        unlike,
        reply,
        liked: state.is_some_and(|s| s["likeState"] == "TOOLBAR_LIKE_STATE_LIKED"),
        liked_count: entity
            .and_then(|e| e["toolbar"]["likeCountLiked"].as_str())
            .unwrap_or_default()
            .into(),
        unliked_count: entity
            .and_then(|e| e["toolbar"]["likeCountNotliked"].as_str())
            .unwrap_or_default()
            .into(),
        avatar: entity
            .map(|e| super::avatar(&e["avatar"]["image"]["sources"]))
            .filter(|url| !url.is_empty())
            .or_else(|| {
                entity
                    .and_then(|e| e["author"]["avatarThumbnailUrl"].as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_metadata_uses_labels_instead_of_row_positions() {
        let raw = json!({"header":{"pageHeaderRenderer":{"metadata":{"metadataRows":[{"metadataParts":[{"text":{"content":"@example"}},{"text":{"content":"1.1M subscribers"}}]},{"metadataParts":[{"text":{"content":"583 videos"}}]}]}}}});
        assert_eq!(
            channel_metadata(&raw),
            "@example · 1.1M subscribers · 583 videos"
        );
    }
    #[test]
    fn sort_tokens_are_taken_from_the_selected_choice() {
        let raw = json!({"chips":[{"chipCloudChipRenderer":{"text":{"simpleText":"Popular"},"navigationEndpoint":{"continuationCommand":{"token":"server-token"}}}}]});
        assert_eq!(
            sort_action(&raw, "Popular"),
            Some(json!({"continuation":"server-token"}))
        );
        assert!(sort_action(&raw, "Oldest").is_none());
    }
    #[test]
    fn channel_tabs_follow_their_own_endpoint_with_decoded_padding() {
        let raw = json!({"tabs":[{"tabRenderer":{"title":"Playlists","endpoint":{"browseEndpoint":{"browseId":"UC-example","params":"playlist%3D%3D"}}}}]});
        let (selected, action) = channel_tab_action(&raw, "Playlists").unwrap();
        assert!(!selected);
        assert_eq!(action["params"], "playlist==");
        assert!(channel_tab_action(&raw, "Live").is_none());
    }
    #[test]
    fn comment_actions_join_toolbar_entities_by_key() {
        let raw = json!({"commentViewModel":{"commentId":"c","toolbarSurfaceKey":"surface","toolbarStateKey":"state","commentKey":"entity"},"frameworkUpdates":{"entityBatchUpdate":{"mutations":[{"entityKey":"surface","payload":{"engagementToolbarSurfaceEntityPayload":{"likeCommand":{"performCommentActionEndpoint":{"action":"like-token"}},"unlikeCommand":{"performCommentActionEndpoint":{"action":"unlike-token"}},"replyCommand":{"dialog":{"createCommentReplyEndpoint":{"createReplyParams":"reply-token"}}}}}},{"entityKey":"state","payload":{"engagementToolbarStateEntityPayload":{"likeState":"TOOLBAR_LIKE_STATE_LIKED"}}}]}}});
        let mut raw = raw;
        raw["frameworkUpdates"]["entityBatchUpdate"]["mutations"][0]["payload"]["commentEntityPayload"] =
            json!({"properties":{"commentId":"c"}});
        let actions = comment_actions(&raw, "c");
        assert!(actions.liked);
        assert_eq!(actions.like.unwrap()["action"], "like-token");
        assert_eq!(actions.reply.as_deref(), Some("reply-token"));
    }
}
