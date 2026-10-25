//! The terminal UI: one window attached to the daemon.

use std::{num::NonZeroU32, path::PathBuf, time::Duration};

use color_eyre::eyre::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind},
    execute,
};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, time::MissedTickBehavior};

use crate::{
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
}

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
        add(&client, &tx, input);
    }

    loop {
        // A queued file picker opens once the user is out of other dialogs.
        app.open_next_picker();
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
                                Action::Add(input) => {
                                    app.clear_status();
                                    add(&client, &tx, input);
                                }
                                Action::SaveTheme(name) => {
                                    match config::set_value(&config_path, "theme", &config::yaml_scalar(&name)) {
                                        Ok(()) => app.set_status(format!("Theme: {name} (saved)"), false),
                                        Err(e) => app.set_status(format!("Theme applied but not saved: {e:#}"), true),
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
            },
        }
    }
}

/// Hand a magnet link, URL or .torrent path to the daemon. Adding a magnet
/// waits for the swarm, so the daemon does it in the background and reports
/// back with a notice.
fn add(client: &Client, tx: &mpsc::UnboundedSender<Msg>, input: String) {
    let input = engine::prepare_input(&input);
    if input.is_empty() {
        return;
    }
    spawn_request(client, tx, "Add failed", Request::Add { input });
}
