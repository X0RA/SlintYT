//! Profile pictures, downloaded the first time the UI draws one.
//!
//! Rows only carry the avatar URL. The UI asks for the image through
//! `avatar-image(url, epoch)`, and each finished download bumps the epoch so
//! every avatar binding re-reads the cache.
use super::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

const LIMIT: usize = 512;

#[derive(Default)]
struct Avatars {
    images: HashMap<String, slint::Image>,
    // In flight or failed. Failures stay here so a broken URL is not retried every frame.
    requested: HashSet<String>,
    // Channel ID to avatar URL, for rows such as playlist entries that arrive without one.
    channels: HashMap<String, String>,
}

thread_local! {
    static AVATARS: RefCell<Avatars> = RefCell::default();
}

pub(super) fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let weak = app.as_weak();
    app.global::<AppState>()
        .on_avatar_image(move |url, _epoch| {
            if url.is_empty() {
                return slint::Image::default();
            }
            AVATARS.with_borrow_mut(|avatars| {
                if let Some(image) = avatars.images.get(url.as_str()) {
                    return image.clone();
                }
                if avatars.requested.insert(url.to_string()) {
                    let (weak, http, url) = (weak.clone(), http.clone(), url.to_string());
                    runtime.spawn(async move {
                        let Ok(pixels) = youtube::download_thumbnail(&http, &url).await else {
                            return;
                        };
                        update(&weak, move |app| {
                            AVATARS.with_borrow_mut(|avatars| {
                                if avatars.images.len() >= LIMIT {
                                    avatars.images.clear();
                                }
                                avatars.requested.remove(&url);
                                avatars.images.insert(url, thumbnail_image(&pixels));
                            });
                            let state = app.global::<AppState>();
                            state.set_avatar_epoch(state.get_avatar_epoch().wrapping_add(1));
                        });
                    });
                }
                slint::Image::default()
            })
        });
}

/// Remembers a channel's avatar, or fills in one seen earlier when the row has none.
pub(super) fn channel_avatar(channel_id: &str, avatar: String) -> String {
    if channel_id.is_empty() {
        return avatar;
    }
    AVATARS.with_borrow_mut(|avatars| {
        if avatar.is_empty() {
            return avatars
                .channels
                .get(channel_id)
                .cloned()
                .unwrap_or_default();
        }
        if avatars.channels.len() >= LIMIT * 4 {
            avatars.channels.clear();
        }
        avatars.channels.insert(channel_id.into(), avatar.clone());
        avatar
    })
}
