# Telegram Drive

A private desktop drive powered by Telegram, built with Tauri, Rust, React, and Bun.

## Requirements

- Bun 1.3+
- Rust stable
- Platform prerequisites for Tauri 2

## Development

```sh
bun install
bun run tauri dev
```

## Production build

```sh
bun run build
bun run tauri build
```

The native backend intentionally remains in Rust. It owns Telegram sessions,
filesystem access, local streaming, archives, and transcoding without requiring
a separate Node.js server.
