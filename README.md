# torrentui

A terminal BitTorrent client built on [ratatui](https://ratatui.rs) and the
[librqbit](https://crates.io/crates/librqbit) engine.

## Install

Requires Rust 1.89 or newer.

```
cargo install --git https://github.com/simoncoulton/torrentui   # or, from a checkout:
cargo install --path .                                          # installs ~/.cargo/bin/torrentui
```

Prebuilt Linux and macOS (Intel and Apple Silicon) binaries are attached to each
[GitHub release](https://github.com/simoncoulton/torrentui/releases); unpack the
archive and put `torrentui` on your `PATH`. Linux and macOS are supported; there
is no Windows version.

## Usage

```
torrentui [OPTIONS] [MAGNET_OR_TORRENT ...]    open the terminal UI

  -d, --dir <DIR>       download here instead of the configured directory
  -c, --config <FILE>   use this config file
      --theme <NAME>    use this theme for this run
      --list-themes     print available themes and exit
      --all-files       skip the file picker

torrentui open [--ui] INPUT...     add torrents without opening a window
torrentui register | unregister    handle magnet links from the browser
torrentui daemon start | stop | restart | status | run
torrentui daemon install-service | uninstall-service
```

## Using it

Multi-file torrents open a file picker before anything downloads; press `f`
later to change the selection of a running torrent. A piece can straddle two
files, so a few bytes of an unselected neighbour may still be written.

| Key | Action |
| --- | --- |
| `a` | Add magnet / URL / file |
| `p`, `Space` | Pause / resume |
| `f`, `Enter` | Choose which files to download |
| `F` | Show / hide a side panel with each file's download progress (`PgUp`/`PgDn` scroll it) |
| `d`, `Del` | Remove the torrent, keep its files (asks first) |
| `D` | Remove the torrent **and delete its data** (asks first) |
| `x` | Run the `on_complete` actions on a finished torrent |
| `t` | Edit the selected torrent's tags |
| `/` | Filter the list by tag (`Esc` shows everything again) |
| `T` | Theme picker with live preview (saved) |
| `l` | Speed limits (saved) |
| `↑↓`, `j k`, `g G` | Navigate |
| `?` | Help |
| `q`, `Ctrl-C` | Close the window; the daemon keeps running |
| `Q` | Close the window and stop the daemon (asks first) |

## How it runs

Torrents are owned by a **background daemon**, and every terminal window is a
client of it. The daemon starts by itself the first time you run `torrentui`
(or open a magnet link), keeps downloading when you close the window, and keeps
running your [post-download actions](#actions-after-a-download).

- Open as many windows as you like: they all show the same live state. Tags,
  limits and torrents are shared; selection, filter and theme preview are per
  window.
- `q` closes the window and leaves the daemon running. `Q` also stops the
  daemon (it always asks first).
- `torrentui daemon stop` stops it from anywhere and waits until it has saved
  its state; `restart` is how config changes (`download_dir`, `on_complete`, the
  peer limit, `dht`, `ask_files`) take effect. `status` shows whether it is
  running (exit code 3 if not).
- `-d` and `--all-files` apply when the command starts the daemon; if it is
  already running with other settings you get an error telling you to
  `torrentui daemon restart` with them.
- `torrentui daemon install-service` writes a systemd user unit (a launchd agent
  on macOS), then `systemctl --user enable --now torrentui` runs the daemon at
  login. Stop a daemon you started by hand first. The unit uses `KillMode=process`
  so a running action isn't killed when the service stops.
- The daemon listens only on a unix socket, `$XDG_RUNTIME_DIR/torrentui/daemon.sock`
  (the directory is mode 0700, the socket 0600, and each connection's user is
  checked), never on a network port. Its stderr goes to
  `<data>/torrentui/daemon.log`, where `<data>` is `~/.local/share` on Linux and
  `~/Library/Application Support` on macOS (see [Where data lives](#where-data-lives)).

## Magnet links from the browser

Clicking a magnet link (or a `.torrent` link) in your browser can add it to
torrentui automatically. On Linux this uses a desktop entry and `xdg-mime`; on
macOS it builds a small handler app. [macOS notes](#macos) are below.

### Setup

1. **Install the binary somewhere permanent.** Registration records the path of
   the executable you run it from, so don't register a throwaway build:

   ```
   cargo install --path .        # installs ~/.cargo/bin/torrentui
   ```

   Make sure `~/.cargo/bin` is on your `PATH`.

2. **Register it:**

   ```
   torrentui register
   ```

   This needs `xdg-mime` (package `xdg-utils`). It writes
   `~/.local/share/applications/torrentui-open.desktop`, makes it the default
   for `x-scheme-handler/magnet` and `application/x-bittorrent`, and prints what
   it did. If another application already handles magnet links it stops and
   tells you which; add `--force` to replace it.

3. **Check it** (Linux; on macOS just click a magnet link):

   ```
   xdg-mime query default x-scheme-handler/magnet    # torrentui-open.desktop
   xdg-open 'magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c'
   ```

   `xdg-open` does what the browser will do. You should see a notification (or
   `Added: …` in any open torrentui window), and the torrent in `torrentui`.

4. **Tell your browser, if it asks.** The first time you click a magnet link
   most browsers ask what to open it with:
   - **Firefox:** choose *torrentui (add torrent)* in the dialog, optionally
     tick *Always*. To change it later: Settings → General → Applications →
     search for `magnet`.
   - **Chrome, Chromium, Brave, Edge:** allow the "Open xdg-open?" prompt,
     optionally tick *Always allow*. They hand the link to the desktop default
     set in step 2.

   These dialogs are the browsers' own and vary by version. Flatpak and snap
   browsers run in a sandbox and may ignore the `xdg-mime` default entirely
   unless a desktop portal forwards the link; if links do nothing there, that
   is the likely reason.

### What happens when you click a link

`torrentui open <link>` needs no terminal: it hands the link to the daemon
(starting it if necessary) and exits.

- With torrentui windows open, every one shows `Added: …` (or the failure).
  With none open, a desktop notification does (via `notify-send`; set
  `notify: false` in the config to turn this off).
- Multi-file torrents wait for a file selection. Every attached window shows the
  picker; **the first answer wins** and the others close theirs. With no window
  open it waits, and notifies you, until you open one. Set `ask_files: false` to
  download everything without asking.
- To have a link also open a terminal when no window is attached, register with
  `--ui`: `torrentui register --ui`. The terminal is detected (`$TERMINAL`, then
  ghostty, kitty, alacritty, foot, wezterm, gnome-terminal, konsole, xterm);
  choose one with `--terminal "ghostty -e"`. When it finds a terminal,
  `register` also adds a `torrentui` entry to your application menu.
- `torrentui open` also takes `.torrent` file paths and `file://` URLs, which is
  how the file manager and browsers pass downloaded `.torrent` files.

### macOS

`torrentui register` works differently on a Mac, because a URL scheme there is
handled by an app bundle rather than a `.desktop` file and `xdg-mime`:

- It builds `~/Applications/torrentui.app`: a tiny AppleScript applet (made with
  `osacompile`, so it needs nothing installed) that receives magnet links and
  `.torrent` files and passes them to `torrentui open`. The app is ad-hoc signed
  and registered with LaunchServices, has no Dock icon, and does nothing if you
  open it directly.
- If only torrentui claims `magnet:` links, macOS uses it automatically. If you
  have another torrent client, macOS has no built-in way to choose between them;
  install [`duti`](https://github.com/moretension/duti) (`brew install duti`) and
  re-run `torrentui register`, which then sets the default for `magnet` and
  `.torrent`. Otherwise use the other client's own setting, or Finder > Get Info
  on a `.torrent` file > Open with > torrentui > Change All….
- `--force` has no effect on macOS (there is no existing default to check).
- Notifications use `osascript`. The first time, macOS may ask, or you may have
  to allow notifications for *Script Editor* in System Settings > Notifications.
- `--ui` opens `Terminal.app` by default. To use another terminal pass a command
  that takes the program to run as its last argument, for example
  `--terminal "open -na Ghostty --args -e"`.
- If `torrentui` lives in a versioned location (Homebrew's `Cellar`), the path
  recorded is the stable one on your `PATH` (such as `/opt/homebrew/bin`), so it
  survives upgrades.
- The first run may trigger a "find devices on your local network" prompt, which
  BitTorrent's local peer discovery uses. Allowing it is optional.
- `torrentui daemon install-service` writes a launchd agent
  (`~/Library/LaunchAgents/com.torrentui.daemon.plist`); start it with the
  `launchctl bootstrap gui/$(id -u) <that file>` command it prints. With
  Homebrew, `brew services start torrentui` does the same.

### Troubleshooting

| Symptom | Check |
| --- | --- |
| Nothing happens on click | `xdg-open 'magnet:?xt=urn:btih:…'` in a terminal. If that works, the problem is the browser's own handler setting (step 4). |
| `xdg-mime query default …` shows another app | Run `torrentui register --force`. |
| It stopped working after moving or rebuilding the binary | The `.desktop` file holds the old path. Run `torrentui register --force` again from the new location. |
| `xdg-mime was not found` (Linux) | Install `xdg-utils`. |
| Nothing happens on a Mac | Run `torrentui open 'magnet:?xt=urn:btih:…'` in Terminal. If that works, the handler app is the problem: re-run `torrentui register` (also needed if you moved the binary). |
| Link accepted but no torrent appears | `torrentui daemon status`, and read `<data>/torrentui/daemon.log` and the notification text. With `ask_files: true` a multi-file torrent is waiting for you to choose files in a window. |
| No notifications | Install `libnotify` (`notify-send`), or check your desktop's notification settings. |

### Removing it

```
torrentui unregister
```

removes the desktop entries. `xdg-mime` has no way to clear a default, but one
that names a missing file is ignored, so browsers go back to asking. You may
also want to delete the stale line from `~/.config/mimeapps.list`.

## Tags

Press `t` on a torrent to tag it, e.g. `movies 4k sci-fi`. Separate tags with
spaces or commas; a leading `#` is ignored and tags are lowercased. Tags may
contain letters, digits and `- _ . /` (so write `tv-shows`, not `tv shows`), up
to 32 characters, at most 16 per torrent. The editor lists the tags already in
use so you can stay consistent. An empty entry removes all tags.

Press `/` to filter by one tag, by `untagged`, or back to all torrents. While a
filter is active the list title shows it (`Torrents — #movies (3 of 10)`), `Esc`
clears it, and the speed totals in the header still cover every torrent. Tags
show in the Details pane, and as a column when the terminal is at least 110
columns wide.

Tags are stored by info-hash in `<data>/torrentui/tags.yml`, so they
survive restarts and renumbering, and are dropped when you remove the torrent.
The file is plain YAML and safe to edit by hand while torrentui is closed;
invalid tags in it are a startup error naming the offending torrent rather than
being discarded.

## Removing

`d` asks `y` / Enter to remove the torrent and leave the downloaded files on
disk. `D` opens a separate red dialog naming the folder; only `y` confirms it
(Enter does not), and Esc cancels. Nothing is deleted by a single keypress from
the list, and pressing `D` inside the `d` dialog just switches to the delete
dialog. For a single-file torrent the folder shown is the download directory;
only that torrent's file is deleted from it.

## Actions after a download

Run your own commands when a torrent finishes, for example to sort files or
send a notification. Define them in the config (read when the daemon starts, so `torrentui daemon
restart` after editing). They run inside the daemon, so they work with no
window open:

```yaml
on_complete:
  - name: sort movies
    run: ["~/bin/sort-movies.sh", "{path}", "{tags}"]
    tags: [movies]          # only torrents with at least one of these tags
    timeout_secs: 600       # default 600; 0 = no limit
  - run: [notify-send, "Download finished", "{name}"]   # no tags: every torrent
```

- `run` is the program followed by its arguments. **No shell is involved**, so
  the torrent name can't inject commands or split into extra arguments. To use
  shell features, run one explicitly: `["/bin/sh", "-c", "script", "sh", "{path}"]`.
- The program must be a bare name found on `PATH`, an absolute path, or `~/...`.
- In the arguments (not the program), `{path}`, `{name}`, `{hash}` and `{tags}`
  are replaced. `{tags}` is comma-separated and empty if the torrent has none.
- The same values are exported as `TORRENTUI_PATH`, `TORRENTUI_NAME`,
  `TORRENTUI_HASH`, `TORRENTUI_TAGS`, plus `TORRENTUI_DIR` (the torrent's
  output folder, which is also the working directory) and `TORRENTUI_TRIGGER`
  (`auto` or `manual`).
- `path` is the downloaded file for a single-file torrent and the torrent's
  folder otherwise. Torrent names come from the swarm and are untrusted: quote
  `"$TORRENTUI_PATH"` in scripts, and note a `{name}` argument could begin with
  `-`.
- Matching actions run one after another in config order; a failure doesn't
  stop the rest. The footer shows `[running N action(s)…]`, then the result.
  Output and exit status are appended to `<data>/torrentui/actions.log`
  (rotated at 1 MiB).
- Actions fire once when a download that torrentui saw in progress finishes. A
  torrent that was already complete when you started or added it does not fire;
  press `x` to run the actions on it by hand (it also reuses the current tags).
  If you only download some files, "finished" means the selected files.
- Closing windows or stopping the daemon does not stop an action that is already running.

## Configuration

`~/.config/torrentui/config.yml` (honours `$XDG_CONFIG_HOME`). A commented
template is created on first run. Unknown keys and invalid values are startup
errors that say what is wrong, rather than being ignored.

```yaml
download_dir: ~/Downloads     # absolute path or ~/...; default: your Downloads folder
theme: tokyo-night            # see below
max_download_speed: 2MiB/s    # B, KiB, MiB, GiB (K/M/G, KB/MB/GB mean the same);
max_upload_speed: 500KiB/s    #   "/s" optional; 0 = unlimited. Bits (Mbps) are rejected.
peer_limit: 0                 # max peers per torrent; 0 = engine default
dht: true                     # trackerless peer discovery (needed for most magnets)
ask_files: true               # show the file picker for multi-file torrents
```

`T` and `l` inside the app write `theme`, `max_download_speed` and
`max_upload_speed` back to this file. Only those lines change; your comments
and everything else are left alone, and a symlinked config (e.g. managed by
stow) stays a symlink. `download_dir`, `peer_limit` and `dht` take effect on the
next daemon restart (`torrentui daemon restart`).

### Themes

Built in: `default` (your terminal's own ANSI colours), `tokyo-night`,
`catppuccin-frappe`, `catppuccin-mocha`, `dracula`, `one-dark`. Names ignore
case, accents and punctuation (`Catppuccin Frappé` works). Everything except
`default` uses 24-bit colour, so use a true-colour terminal.

Define your own, or tweak a built-in, under `themes:`. Every colour is optional
and inherits from `base` (a built-in, default `default`). Colours are
`#rrggbb`, an ANSI name (`red`, `lightblue`) or a 0-255 index. Reusing a
built-in's name replaces it.

```yaml
themes:
  my-theme:
    base: dracula
    accent: "#ff79c6"
    fg: "#f8f8f2"
```

Colour keys: `fg muted accent border selection_bg success info warning error special`
(`success` = downloading, `info` = seeding/upload, `warning` = paused,
`special` = checking). Themes never set a background, so terminal transparency is preserved.

## Where data lives

The config is always `~/.config/torrentui/config.yml` (honouring
`$XDG_CONFIG_HOME`), including on macOS. Other files below use the platform's
data directory: `~/.local/share` on Linux (honouring `$XDG_DATA_HOME`) and
`~/Library/Application Support` on macOS, written as `<data>` here.

- Daemon socket and lock: `$XDG_RUNTIME_DIR/torrentui/` (else
  `<data>/torrentui/run/`); daemon stderr: `<data>/torrentui/daemon.log`
- Config: `~/.config/torrentui/config.yml`
- Torrent list and resume data: `<data>/torrentui/sessions/`. Older installs
  kept this in the engine's own `com.rqbit.session` folder under `<data>`; it is
  moved automatically on first start.
- Tags: `<data>/torrentui/tags.yml`
- Action log and bookkeeping: `<data>/torrentui/actions.log`,
  `pending-actions.yml` (downloads awaiting their actions)
- DHT routing cache: the engine's own `com.rqbit.dht` folder in your cache
  directory (disposable)

## Layout
- `config.rs` – YAML config, speed-limit parsing, comment-preserving edits
- `tags.rs` – tag parsing and the `tags.yml` store
- `actions.rs` – `on_complete` parsing, placeholder expansion, completion
  tracking and the command runner
- `theme.rs` – palettes, built-in themes, custom theme resolution
- `engine.rs` – librqbit wrapper; yields plain-data `TorrentView` snapshots
- `proto.rs` – the daemon/client wire protocol and socket locations
- `daemon.rs` – the background process: owns the session, tags and actions
- `client.rs` – connecting to (and starting) the daemon
- `desktop.rs` – magnet-link registration, notifications, systemd unit (Linux)
- `macos.rs` – the same for macOS: handler app, notifications, launchd agent
- `app.rs` – state + key handling (pure, unit-tested)
- `ui.rs` – rendering only (tested with ratatui's `TestBackend`)
- `tui.rs` – the window's event loop
- `main.rs` – command line and dispatch
