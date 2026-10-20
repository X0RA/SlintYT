fn main() {
    println!("cargo:rerun-if-env-changed=SLINTYT_PREVIEW");
    let ui = match std::env::var("SLINTYT_PREVIEW") {
        Ok(name) => {
            assert!(
                [
                    "app-content",
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
                    "search-bar",
                    "library-row",
                    "settings",
                    "yt-dlp-settings",
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
