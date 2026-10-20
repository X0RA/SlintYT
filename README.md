# SlintYT

SlintYT is a desktop app built in rust with slint for watching [YouTube](https://www.youtube.com) through your own account, with playback handled by [mpv](https://mpv.io).

There's no API key or separate login. It borrows the YouTube sign-in from a browser profile you're already logged into, so your recommendations, subscriptions, history and playlists all show up as they normally would.

YouTube in the browser is heavy, and I wanted something that felt like a native app and played through mpv. Watch progress syncs back to YouTube, so you can stop halfway through a video here and pick it up on your phone or TV later.

It is built around a simple flow:

- pick a signed in browser profile (Chromium and Firefox based browsers are supported)
- browse your home feed, subscriptions, history, playlists and channels
- play videos with quality selection, subtitles, comments and watched state
- switch between saved profiles from the account menu

You'll need `yt-dlp` (kept up to date), libmpv and a JavaScript runtime like Deno on your `PATH`. Then just run `cargo run --locked`.
