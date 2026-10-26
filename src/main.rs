slint::include_modules!();

mod app;
mod bookmarks;
mod browser;
mod cache;
mod config;
mod cookies;
mod feed;
mod github;
mod history;
#[cfg(target_os = "macos")]
mod macos_tools;
mod playback_settings;
mod player;
mod updates;
mod watch;
mod youtube;

fn main() -> Result<(), slint::PlatformError> {
    if std::env::var_os("SLINTYT_PREVIEW").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("femtovg".into())
            .require_opengl()
            .select()?;
    }
    let app = AppWindow::new()?;
    slint::set_xdg_app_id("slintyt")?;
    // Keep the runtime alive for the lifetime of the native event loop.
    let runtime = if std::env::var_os("SLINTYT_PREVIEW").is_none() {
        Some(app::wire(&app))
    } else {
        None
    };
    let result = app.run();
    if let Some((runtime, player)) = runtime {
        // The event loop has ended, but final watch reports still need the runtime.
        runtime.block_on(async {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(8), player.shutdown()).await;
            // An update may be committing files in an elevated helper. Let it
            // finish before dropping the runtime and its subprocess readers.
            updates::shutdown().await;
        });
        runtime.shutdown_background();
    }
    result
}
