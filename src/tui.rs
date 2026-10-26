//! The terminal UI: one window attached to the daemon.

use std::{
    num::NonZeroU32,
    path::PathBuf,
    time::{Duration, Instant},
};

use color_eyre::eyre::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind},
    execute,
};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, time::MissedTickBehavior};

use crate::{
    actions,
    app::{Action, App, FilePicker, PickerTarget},
    client::{Client, Connection, Event as DaemonEvent},
    config,
    engine::{self, FileList},
    proto::Request,
    theme::Theme,
    ui,
};

/// How often the screen is redrawn even when nothing happened (so status
/// messages expire).
const REDRAW: Duration = Duration::from_millis(500);

/// What `run` needs besides the connection.
pub struct Settings {
    pub config_path: PathBuf,
    pub themes: Vec<Theme>,
    pub theme_name: String,
    pub transparent: bool,
    /// Offer tags for torrents that arrive from a browser or another window.
    pub ask_tags: bool,
    /// Torrents to add as soon as the window opens.
    pub inputs: Vec<String>,
}

/// Why the window closed.
pub enum Exit {
    /// The user quit; the daemon keeps running.
    Quit,
    /// The user asked to stop the daemon too.
    StopDaemon,
    /// The daemon went away.
    Closed(String),
}

/// Results of background requests, delivered to the event loop.
enum Msg {
    Status(String, bool),
    /// A settings change (limits, tags) was acknowledged or failed.
    Setting(Result<Option<(String, bool)>>),
    Files {
        id: usize,
        name: String,
        result: Result<(FileList, Vec<usize>)>,
    },
    /// Fresh per-file progress for the side panel.
    PanelFiles {
        id: usize,
        result: Result<(FileList, Vec<usize>)>,
    },
}

/// How often the side panel refreshes its per-file progress.
const PANEL_REFRESH: Duration = Duration::from_secs(1);

/// Enter the alternate screen / raw mode. `ratatui::init` also installs a panic
/// hook that restores the terminal; we extend it to undo bracketed paste.
pub fn init_terminal() -> Result<DefaultTerminal> {
    let terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        prev(info);
    }));
    Ok(terminal)
}

/// The editor command: `$VISUAL`, else `$EDITOR`, else `vi`, split like a
/// command line so `code -w` works.
fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Result<Vec<String>> {
    let spec = [visual, editor]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .unwrap_or("vi");
    let parts = actions::split_command(spec)
        .map_err(|e| color_eyre::eyre::eyre!("cannot read $VISUAL/$EDITOR: {e}"))?;
    Ok(parts)
}

/// Give the terminal to an editor and wait for it to exit.
fn run_editor(path: &std::path::Path) -> Result<()> {
    use color_eyre::eyre::{WrapErr, bail};
    let parts = editor_command(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    )?;
    let Some((program, args)) = parts.split_first() else {
        bail!("$VISUAL/$EDITOR is empty");
    };
    let status = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .wrap_err_with(|| format!("cannot start the editor '{program}'"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

pub fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

/// Run a request in the background and report a failure in the status bar.
/// Successful requests change state the daemon pushes back, so they need
/// no handling here.
fn spawn_request(
    client: &Client,
    tx: &mpsc::UnboundedSender<Msg>,
    what: &'static str,
    req: Request,
) {
    let (client, tx) = (client.clone(), tx.clone());
    tokio::spawn(async move {
        match client.request_message(req).await {
            Ok(Some((text, is_error))) => {
                let _ = tx.send(Msg::Status(text, is_error));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(Msg::Status(format!("{what}: {e:#}"), true));
            }
        }
    });
}

fn bps(rate: config::Rate) -> Option<u32> {
    rate.bps().map(NonZeroU32::get)
}

pub async fn run(
    mut terminal: DefaultTerminal,
    conn: Connection,
    settings: Settings,
) -> Result<Exit> {
    let Connection {
        client,
        welcome,
        mut events,
    } = conn;
    let mut app = App::new(welcome.download_dir);
    app.transparent = settings.transparent;
    app.ask_tags = settings.ask_tags;
    app.set_themes(settings.themes, &settings.theme_name);
    app.has_actions = welcome.has_actions;
    if let Some(n) = welcome.notice {
        app.set_status(n, false);
    }
    let config_path = settings.config_path;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(REDRAW);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    for input in settings.inputs {
        add(&client, &tx, input, Vec::new());
    }

    let mut panel_in_flight = false;
    let mut panel_fetched = Instant::now();

    loop {
        // A queued file picker opens once the user is out of other dialogs.
        app.open_next_picker();
        app.open_next_tag_prompt();

        // Keep the file panel's progress fresh: one request at a time, at
        // once for a newly selected torrent and otherwise every second.
        if let Some(id) = app.panel_wanted()
            && !panel_in_flight
            && (app.panel_files.as_ref().is_none_or(|p| p.id != id)
                || panel_fetched.elapsed() >= PANEL_REFRESH)
        {
            panel_in_flight = true;
            panel_fetched = Instant::now();
            let (client, tx) = (client.clone(), tx.clone());
            tokio::spawn(async move {
                let result = match client.request(Request::Files { id }).await {
                    Ok(crate::proto::Reply::Files { list, selected }) => Ok((list, selected)),
                    Ok(other) => Err(color_eyre::eyre::eyre!("unexpected reply: {other:?}")),
                    Err(e) => Err(e),
                };
                let _ = tx.send(Msg::PanelFiles { id, result });
            });
        }

        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            _ = tick.tick() => {}

            ev = events.recv() => match ev {
                Some(DaemonEvent::State(state)) => app.apply_state(*state),
                Some(DaemonEvent::Notice { text, is_error }) => app.set_status(text, is_error),
                Some(DaemonEvent::Closed(reason)) => return Ok(Exit::Closed(reason)),
                None => return Ok(Exit::Closed("the connection to the daemon was lost".into())),
            },

            maybe_event = term_events.next() => {
                // Stream end means stdin closed; nothing more can be done.
                let Some(event) = maybe_event else { return Ok(Exit::Quit) };
                match event? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = app.handle_key(key) {
                            match action {
                                Action::Quit => return Ok(Exit::Quit),
                                Action::StopDaemon => return Ok(Exit::StopDaemon),
                                Action::Add { input, tags } => {
                                    app.clear_status();
                                    add(&client, &tx, input, tags);
                                }
                                Action::SaveTheme { name, transparent } => {
                                    let mut note = format!("Theme: {name}");
                                    let mut failed = None;
                                    if let Err(e) = config::set_value(&config_path, "theme", &config::yaml_scalar(&name)) {
                                        failed = Some(e);
                                    } else if let Some(on) = transparent {
                                        note.push_str(if on { ", transparent background" } else { ", theme background" });
                                        if let Err(e) = config::set_value(&config_path, "transparent_background", &on.to_string()) {
                                            failed = Some(e);
                                        }
                                    }
                                    match failed {
                                        None => app.set_status(format!("{note} (saved)"), false),
                                        Some(e) => app.set_status(format!("{note} applied but not saved: {e:#}"), true),
                                    }
                                }
                                Action::SetLimits { download, upload } => {
                                    app.settings_in_flight += 1;
                                    let (client, tx) = (client.clone(), tx.clone());
                                    let req = Request::SetLimits { download: bps(download), upload: bps(upload) };
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Setting(client.request_message(req).await));
                                    });
                                }
                                Action::EditConfig => {
                                    let before = std::fs::read_to_string(&config_path).ok();
                                    restore_terminal();
                                    let outcome = run_editor(&config_path);
                                    terminal = init_terminal()?;
                                    // The old stream may have swallowed keys meant for the editor.
                                    term_events = EventStream::new();
                                    match outcome {
                                        Err(e) => app.set_status(format!("Editor: {e:#}"), true),
                                        Ok(()) if std::fs::read_to_string(&config_path).ok() == before => {}
                                        Ok(()) => match config::load(&config_path) {
                                            Err(e) => app.set_status(
                                                format!("The config has an error, fix it before restarting: {e:#}"),
                                                true,
                                            ),
                                            Ok(_) => app.set_status(
                                                "Config changed: run `torrentui daemon restart` to apply it",
                                                false,
                                            ),
                                        },
                                    }
                                }
                                Action::DeleteTag(tag) => {
                                    app.settings_in_flight += 1;
                                    let (client, tx) = (client.clone(), tx.clone());
                                    let req = Request::DeleteTag { tag };
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Setting(client.request_message(req).await));
                                    });
                                }
                                Action::SetHooks(hooks) => {
                                    app.settings_in_flight += 1;
                                    let (client, tx) = (client.clone(), tx.clone());
                                    let req = Request::SetHooks { hooks };
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Setting(client.request_message(req).await));
                                    });
                                }
                                Action::SetTags { info_hash, tags } => {
                                    app.settings_in_flight += 1;
                                    let (client, tx) = (client.clone(), tx.clone());
                                    let req = Request::SetTags { info_hash, tags };
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Setting(client.request_message(req).await));
                                    });
                                }
                                Action::OpenFiles(id) => {
                                    let name = app.current().map(|t| t.name.clone()).unwrap_or_default();
                                    let (client, tx) = (client.clone(), tx.clone());
                                    tokio::spawn(async move {
                                        let result = match client.request(Request::Files { id }).await {
                                            Ok(crate::proto::Reply::Files { list, selected }) => Ok((list, selected)),
                                            Ok(other) => Err(color_eyre::eyre::eyre!("unexpected reply: {other:?}")),
                                            Err(e) => Err(e),
                                        };
                                        let _ = tx.send(Msg::Files { id, name, result });
                                    });
                                }
                                Action::ApplyFiles { target, only } => {
                                    let req = match target {
                                        PickerTarget::New { token, .. } => Request::ApplyPicker { token, only },
                                        PickerTarget::Existing { id } => Request::SetFiles { id, only },
                                    };
                                    spawn_request(&client, &tx, "Updating files failed", req);
                                }
                                Action::CancelPicker(token) => {
                                    spawn_request(&client, &tx, "Cancel failed", Request::CancelPicker { token });
                                }
                                Action::TogglePause(id) => {
                                    spawn_request(&client, &tx, "Pause/resume failed", Request::TogglePause { id });
                                }
                                Action::Remove { id, delete_files } => {
                                    spawn_request(&client, &tx, "Remove failed", Request::Remove { id, delete_files });
                                }
                                Action::RunActions(id) => {
                                    app.set_status("Running actions…", false);
                                    spawn_request(&client, &tx, "Actions failed", Request::RunActions { id });
                                }
                            }
                        }
                    }
                    Event::Paste(text) => app.handle_paste(&text),
                    // Resize and everything else just triggers the redraw at the loop top.
                    _ => {}
                }
            }

            Some(msg) = rx.recv() => match msg {
                Msg::Status(text, is_error) => app.set_status(text, is_error),
                Msg::Setting(result) => {
                    app.settings_in_flight = app.settings_in_flight.saturating_sub(1);
                    match result {
                        Ok(Some((text, is_error))) => app.set_status(text, is_error),
                        Ok(None) => {}
                        Err(e) => app.set_status(format!("Change failed: {e:#}"), true),
                    }
                }
                Msg::Files { id, name, result } => match result {
                    Ok((list, _)) if list.entries.len() < 2 => {
                        app.set_status("This torrent has only one file", false);
                    }
                    Ok((list, selected)) => app.queue_picker(FilePicker::new(
                        PickerTarget::Existing { id },
                        name,
                        list,
                        |i| selected.contains(&i),
                    )),
                    Err(e) => app.set_status(format!("Cannot list files: {e:#}"), true),
                },
                Msg::PanelFiles { id, result } => {
                    panel_in_flight = false;
                    // Metadata may not be resolved yet; the panel says so and
                    // the next refresh tries again.
                    if let Ok((list, selected)) = result {
                        app.set_panel_files(id, list, selected);
                    }
                }
            },
        }
    }
}

/// Hand a magnet link, URL or .torrent path to the daemon. Adding a magnet
/// waits for the swarm, so the daemon does it in the background and reports
/// back with a notice.
fn add(client: &Client, tx: &mpsc::UnboundedSender<Msg>, input: String, tags: Vec<String>) {
    let input = engine::prepare_input(&input);
    if input.is_empty() {
        return;
    }
    spawn_request(client, tx, "Add failed", Request::Add { input, tags });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_prefers_visual_then_editor_then_vi() {
        let cmd = |v, e| editor_command(v, e).unwrap();
        assert_eq!(cmd(Some("nvim"), Some("nano")), ["nvim"]);
        assert_eq!(cmd(None, Some("code -w")), ["code", "-w"]);
        assert_eq!(cmd(Some("  "), Some("nano")), ["nano"]);
        assert_eq!(cmd(None, None), ["vi"]);
        assert_eq!(
            cmd(Some("'/Applications/My Editor/ed' --wait"), None),
            ["/Applications/My Editor/ed", "--wait"]
        );
        assert!(editor_command(Some("'oops"), None).is_err());
    }
}
