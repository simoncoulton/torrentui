mod app;
mod engine;
mod fmt;
mod ui;

use std::{path::PathBuf, time::Duration};

use clap::Parser;
use color_eyre::eyre::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind},
    execute,
};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, time::MissedTickBehavior};

use app::{Action, App};
use engine::Engine;

/// A terminal BitTorrent client.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Magnet links, .torrent URLs or .torrent file paths to add on startup.
    inputs: Vec<String>,

    /// Directory to download into [default: your Downloads folder].
    #[arg(short, long)]
    dir: Option<PathBuf>,
}

/// Results of background work, delivered to the event loop.
enum Msg {
    Added(Result<String>),
    Toggled(Result<()>),
    Removed(Result<()>),
}

/// How often torrent stats are re-read and the screen is redrawn.
const REFRESH: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let dir = cli
        .dir
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let engine = Engine::start(dir.clone()).await?;

    let terminal = init_terminal()?;
    let result = run(terminal, &engine, &dir, cli.inputs).await;
    restore_terminal();

    // Shut the session down only after the screen is restored, so the user
    // isn't staring at a frozen UI while persistence is flushed.
    engine.shutdown().await;
    result
}

/// Enter the alternate screen / raw mode. `ratatui::init` also installs a panic
/// hook that restores the terminal; we extend it to undo bracketed paste.
fn init_terminal() -> Result<DefaultTerminal> {
    let terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        prev(info);
    }));
    Ok(terminal)
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

async fn run(
    mut terminal: DefaultTerminal,
    engine: &Engine,
    dir: &std::path::Path,
    initial: Vec<String>,
) -> Result<()> {
    let mut app = App::new(dir.display().to_string());
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    for input in initial {
        spawn_add(engine, &tx, &mut app, input);
    }
    app.update_torrents(engine.snapshot());

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            _ = tick.tick() => app.update_torrents(engine.snapshot()),

            maybe_event = events.next() => {
                // Stream end means stdin closed; nothing more can be done.
                let Some(event) = maybe_event else { return Ok(()) };
                match event? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = app.handle_key(key) {
                            match action {
                                Action::Quit => return Ok(()),
                                Action::Add(input) => spawn_add(engine, &tx, &mut app, input),
                                Action::TogglePause(id) => {
                                    let (engine, tx) = (engine.clone(), tx.clone());
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Toggled(engine.toggle_pause(id).await));
                                    });
                                }
                                Action::Remove { id, delete_files } => {
                                    let (engine, tx) = (engine.clone(), tx.clone());
                                    tokio::spawn(async move {
                                        let _ = tx.send(Msg::Removed(engine.remove(id, delete_files).await));
                                    });
                                }
                            }
                        }
                    }
                    Event::Paste(text) => app.handle_paste(&text),
                    // Resize and everything else just triggers the redraw at the loop top.
                    _ => {}
                }
            }

            Some(msg) = rx.recv() => {
                match msg {
                    Msg::Added(res) => {
                        app.pending_adds = app.pending_adds.saturating_sub(1);
                        match res {
                            Ok(name) => app.set_status(format!("Added: {name}"), false),
                            Err(e) => app.set_status(format!("Add failed: {e:#}"), true),
                        }
                    }
                    Msg::Toggled(Err(e)) => app.set_status(format!("Pause/resume failed: {e:#}"), true),
                    Msg::Removed(Ok(())) => app.set_status("Removed", false),
                    Msg::Removed(Err(e)) => app.set_status(format!("Remove failed: {e:#}"), true),
                    Msg::Toggled(Ok(())) => {}
                }
                app.update_torrents(engine.snapshot());
            }
        }
    }
}

/// Adding a magnet link waits for metadata from the swarm, so it must never
/// block the UI: run it in a task and report back through the channel.
fn spawn_add(engine: &Engine, tx: &mpsc::UnboundedSender<Msg>, app: &mut App, input: String) {
    app.pending_adds += 1;
    app.clear_status();
    let (engine, tx) = (engine.clone(), tx.clone());
    tokio::spawn(async move {
        let _ = tx.send(Msg::Added(engine.add(&input).await));
    });
}
