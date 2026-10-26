fn main() {
    println!("cargo:rerun-if-env-changed=SLINTYT_PREVIEW");
    // Release builds read this through option_env! to check for updates.
    println!("cargo:rerun-if-env-changed=SLINTYT_VERSION");
    let ui = match std::env::var("SLINTYT_PREVIEW") {
        Ok(name) => {
            assert!(
                [
                    "app-content",
                    "bookmark-button",
                    "bookmarks-settings",
                    "app-loading",
                    "browser-picker",
                    "video-feed",
                    "video-card",
                    "video-row",
                    "video-player",
                    "action-button",
                    "avatar",
                    "icon",
                    "nav-rail",
                    "sync-menu",
                    "search-bar",
                    "library-row",
                    "settings",
                    "yt-dlp-settings",
                    "app-update-settings",
                    "channel-header",
                    "video-details",
                    "pill-input",
                    "menu-item",
                    "choice",
                    "back-link",
                    "scrubber"
                ]
                .contains(&name.as_str()),
                "Unknown SlintYT preview"
            );
            format!("ui/previews/{name}-preview.slint")
        }
        Err(_) => "ui/main.slint".into(),
    };
    slint_build::compile(&ui).expect("Slint UI compilation failed");
}
