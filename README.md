# SlintYT

SlintYT is a desktop app built in Rust with Slint for watching [YouTube](https://www.youtube.com) through your own account, with playback handled by [mpv](https://mpv.io).

There's no API key or separate login. It borrows the YouTube sign-in from a browser profile you're already logged into, so your recommendations, subscriptions, history and playlists all show up as they normally would.

YouTube in the browser is heavy, and I wanted something that felt like a native app and played through mpv. Watch progress syncs back to YouTube, so you can stop halfway through a video here and pick it up on your phone or TV later.

It is built around a simple flow:

- pick a signed in browser profile (Chromium and Firefox based browsers are supported)
- browse your home feed, subscriptions, history, playlists and channels
- play videos with quality selection, subtitles, comments and watched state
- switch between saved profiles from the account menu

You'll need libmpv. On Linux and Windows, also install `yt-dlp` (kept up to date) and a JavaScript runtime like Deno on your `PATH`. On macOS, SlintYT downloads and manages its own yt-dlp and Deno in `~/Library/Application Support/SlintYT/tools/`, checks for updates on startup, and provides a manual check in Settings → yt-dlp. The first launch needs an internet connection to install these tools. Then just run `cargo run --locked`.

## Installation

### Linux (direct install)

```sh
curl -fsSL https://raw.githubusercontent.com/X0RA/SlintYT/main/scripts/install-linux.sh | bash
```

### macOS (Apple Silicon)

```sh
curl -fsSL https://raw.githubusercontent.com/X0RA/SlintYT/main/scripts/install-macos.sh | bash
```

### Windows

Download the latest installer from the [Releases page](https://github.com/X0RA/SlintYT/releases/latest).
