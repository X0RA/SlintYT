use crate::{AppState, AppWindow, config::Store};
use anyhow::{Context, Result};
use slint::ComponentHandle;
use std::process::Stdio;

pub(super) fn wire(app: &AppWindow, runtime: tokio::runtime::Handle) {
    let weak = app.as_weak();
    app.global::<AppState>().on_open_data_folder(move || {
        let weak = weak.clone();
        runtime.spawn(async move {
            if let Err(error) = open_data_folder().await {
                super::update(&weak, move |app| {
                    app.global::<AppState>().set_toast(error.to_string().into());
                });
            }
        });
    });
}

async fn open_data_folder() -> Result<()> {
    let store = Store::new()?;
    let path = store.directory();
    std::fs::create_dir_all(path).context("Could not create the app data folder.")?;
    #[cfg(target_os = "macos")]
    let opener = "/usr/bin/open";
    #[cfg(windows)]
    let opener = "explorer.exe";
    #[cfg(not(any(target_os = "macos", windows)))]
    let opener = "xdg-open";
    // Pass the native path directly, without shell parsing or URL encoding.
    // Keep waiting off the UI thread to reap the launcher. Some Linux file
    // managers keep it alive until their window closes, so don't kill on drop.
    let status = tokio::process::Command::new(opener)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("Could not open the app data folder in your file manager.")?;
    // Explorer may return 1 after handing the folder to an existing instance.
    if !status.success() && !(cfg!(windows) && status.code() == Some(1)) {
        anyhow::bail!("Could not open the app data folder in your file manager.");
    }
    Ok(())
}
