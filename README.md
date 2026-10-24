# ossdeck

> **Disclaimer:** vibe-coded proof of concept. Built in one sitting with an AI assistant, barely tested, no stability guarantees. Use at your own risk.

Native macOS command centre for your open source GitHub activity: notifications from your own public repos and threads you take part in upstream, with a tiny task tracker on top. Rust + [GPUI](https://gpui.rs) (Zed's GPU UI framework), SQLite, no Electron.

## Features

- Inbox of public-repo notifications only: everything in your repos, plus upstream threads you authored, commented on, were mentioned or asked to review in. Private repos and watch-only repos are skipped.
- GitHub-style PR/issue status icons and the latest comment on every row.
- Read / done sync back to your GitHub inbox.
- Mini tasks per thread (`new -> reviewed -> merged`, `submitted -> feedback -> merged`, ...) with Linear-style priorities.
- Keyboard-first: every action has a hotkey, a button and a command palette entry (`cmd-K`).
- Menu bar item with unread count, Dock badge, native notifications, launch at login.
- Background sync every 1-30 minutes (GitHub's 60s poll interval is the floor), conditional requests, octocrab-style retries.

## Requirements

- macOS 13+, Rust (2024 edition), Xcode with the Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`)
- [`gh`](https://cli.github.com) logged in (`gh auth login`), or `GITHUB_TOKEN` set

## Build

```sh
./bundle.sh   # builds release, creates ossdeck.app, installs it to ~/Applications
```

`cargo run` works for development, but notifications and launch at login need the bundled app.

Data lives in `~/Library/Application Support/ossdeck/ossdeck.db`.

Icons: [Octicons](https://github.com/primer/octicons) (MIT).
