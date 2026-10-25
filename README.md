# torrent-tui

A terminal BitTorrent client built on [ratatui](https://ratatui.rs) and the
[librqbit](https://crates.io/crates/librqbit) engine.

```
cargo run --release -- [--dir ~/Downloads] [MAGNET_OR_TORRENT ...]
```

Add magnet links, `.torrent` URLs, or local `.torrent` paths from the command
line or with `a` inside the app (bracketed paste is supported). Torrents are
persisted and resumed on restart.

| Key | Action |
| --- | --- |
| `a` | Add magnet / URL / file |
| `p`, `Space` | Pause / resume |
| `d`, `Del` | Remove (`y` keep files, `D` delete files) |
| `↑↓`, `j k`, `g G` | Navigate |
| `?` | Help |
| `q`, `Ctrl-C` | Quit |

## Layout
- `engine.rs` – librqbit wrapper; yields plain-data `TorrentView` snapshots
- `app.rs` – state + key handling (pure, unit-tested)
- `ui.rs` – rendering only (tested with ratatui's `TestBackend`)
- `main.rs` – terminal setup and the async `select!` event loop
