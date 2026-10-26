mod actions;
mod app;
mod client;
mod config;
mod daemon;
mod desktop;
mod engine;
mod fmt;
mod forms;
mod macos;
mod proto;
mod tags;
mod theme;
mod tui;
mod ui;

use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};

use proto::Request;

/// A terminal BitTorrent client. Torrents live in a background daemon that
/// starts on demand, so downloads continue when the window is closed.
#[derive(Parser, Debug)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    tui: TuiArgs,
}

/// Open the terminal UI (the default).
#[derive(Args, Debug)]
struct TuiArgs {
    /// Magnet links, .torrent URLs or .torrent file paths to add on startup.
    inputs: Vec<String>,

    /// Directory to download into [default: `download_dir` from the config,
    /// else your Downloads folder]. Applies when this starts the daemon.
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// Config file to use [default: ~/.config/torrentui/config.yml].
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Use this theme instead of the one in the config for this run.
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,

    /// Print the available themes and exit.
    #[arg(long)]
    list_themes: bool,

    /// Download every file without offering a file picker. Applies when this
    /// starts the daemon.
    #[arg(long)]
    all_files: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Add torrents through the daemon without opening a window. This is
    /// what the browser's magnet-link handler runs.
    Open(OpenArgs),
    /// Make torrentui the handler for magnet links and .torrent files.
    Register(RegisterArgs),
    /// Remove the magnet-link handler.
    Unregister,
}

#[derive(Args, Debug)]
struct DaemonArgs {
    /// Directory to download into.
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// Config file to use.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Download every file without a file picker.
    #[arg(long)]
    all_files: bool,
}

#[derive(Subcommand, Debug)]
enum DaemonCommand {
    /// Start the daemon in the background.
    Start(DaemonArgs),
    /// Stop the daemon after it has saved its state.
    Stop,
    /// Stop the daemon (if running) and start it again, e.g. after editing the config.
    Restart(DaemonArgs),
    /// Show whether the daemon is running.
    Status,
    /// Run the daemon in the foreground (what systemd and auto-start use).
    Run(DaemonArgs),
    /// Install a systemd user service for the daemon.
    InstallService,
    /// Remove the systemd user service.
    UninstallService,
}

#[derive(Args, Debug)]
struct OpenArgs {
    /// Magnet links, .torrent URLs or paths (file:// URLs work too).
    #[arg(required = true)]
    inputs: Vec<String>,

    /// Config file to use if the daemon has to be started.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// If no window is attached, open one in a terminal.
    #[arg(long)]
    ui: bool,

    /// Terminal command for --ui, e.g. "ghostty -e" [default: detected].
    #[arg(long, value_name = "CMD")]
    terminal: Option<String>,
}

#[derive(Args, Debug)]
struct RegisterArgs {
    /// Links also open a terminal window with torrentui if none is attached,
    /// instead of only adding in the background.
    #[arg(long)]
    ui: bool,

    /// Terminal command for --ui and the launcher entry, e.g. "ghostty -e"
    /// [default: detected].
    #[arg(long, value_name = "CMD")]
    terminal: Option<String>,

    /// Replace another application as the default handler.
    #[arg(long)]
    force: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    // color-eyre still gives panics a readable report and restores the terminal.
    let _ = color_eyre::install();
    match start().await {
        Ok(code) => code,
        Err(e) => {
            // Config and startup problems are user errors: no backtrace noise.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        None => run_tui(cli.tui).await,
        Some(Command::Daemon { command }) => run_daemon_command(command).await,
        Some(Command::Open(args)) => {
            let result = open(&args).await;
            if let Err(e) = &result {
                // Launched from a browser there is no terminal to read this.
                desktop::notify("torrentui could not add the torrent", &format!("{e:#}"));
            }
            result.map(|()| ExitCode::SUCCESS)
        }
        Some(Command::Register(args)) => {
            let terminal = args
                .terminal
                .as_deref()
                .map(desktop::parse_terminal)
                .transpose()?;
            let lines = desktop::register(&desktop::RegisterOpts {
                ui: args.ui,
                terminal,
                force: args.force,
            })?;
            print_lines(&lines);
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Unregister) => {
            print_lines(&desktop::unregister()?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_lines(lines: &[String]) {
    for l in lines {
        println!("{l}");
    }
}

fn config_path(arg: &Option<PathBuf>) -> Result<PathBuf> {
    match arg {
        // The daemon is started from another directory; keep the path valid.
        Some(p) => Ok(std::path::absolute(p)?),
        None => config::default_path(),
    }
}

fn daemon_options(args: &DaemonArgs) -> Result<daemon::Options> {
    Ok(daemon::Options {
        config_path: config_path(&args.config)?,
        dir: args.dir.as_deref().map(std::path::absolute).transpose()?,
        all_files: args.all_files,
    })
}

async fn run_daemon_command(command: DaemonCommand) -> Result<ExitCode> {
    match command {
        DaemonCommand::Run(args) => {
            daemon::run(daemon_options(&args)?).await?;
        }
        DaemonCommand::Start(args) => {
            let opts = daemon_options(&args)?;
            daemon::validate(&opts)?;
            if let Some(conn) = client::connect(false).await? {
                let s = conn.client.status().await?;
                println!("The daemon is already running (pid {}).", s.pid);
                return Ok(ExitCode::SUCCESS);
            }
            let conn = client::connect_or_start(&opts, false).await?;
            println!("Daemon started (pid {}).", conn.welcome.pid);
        }
        DaemonCommand::Stop => {
            if client::stop().await? {
                println!("Daemon stopped.");
            } else {
                println!("The daemon is not running.");
            }
        }
        DaemonCommand::Restart(args) => {
            let opts = daemon_options(&args)?;
            // Check the config first so a typo doesn't leave nothing running.
            daemon::validate(&opts)?;
            client::stop().await?;
            let conn = client::connect_or_start(&opts, false).await?;
            println!("Daemon restarted (pid {}).", conn.welcome.pid);
        }
        DaemonCommand::Status => match client::connect(false).await? {
            Some(conn) => {
                let s = conn.client.status().await?;
                println!(
                    "running: pid {}, {} torrent(s), {} window(s) attached, up {}",
                    s.pid,
                    s.torrents,
                    s.uis,
                    fmt::duration(s.uptime_secs)
                );
            }
            None => {
                println!("not running");
                return Ok(ExitCode::from(3));
            }
        },
        DaemonCommand::InstallService => print_lines(&desktop::install_service()?),
        DaemonCommand::UninstallService => print_lines(&desktop::uninstall_service()?),
    }
    Ok(ExitCode::SUCCESS)
}

/// `torrentui open`: queue torrents with the daemon (starting it if needed).
async fn open(args: &OpenArgs) -> Result<()> {
    let opts = daemon::Options {
        config_path: config_path(&args.config)?,
        dir: None,
        all_files: false,
    };
    let conn = client::connect_or_start(&opts, false).await?;
    for input in &args.inputs {
        let input = engine::prepare_input(input);
        if input.is_empty() {
            bail!("nothing to add");
        }
        conn.client
            .request(Request::Add {
                input: input.clone(),
                tags: Vec::new(),
            })
            .await
            .wrap_err_with(|| format!("cannot add {input}"))?;
        println!("Queued: {input}");
    }
    if args.ui && conn.client.status().await?.uis == 0 {
        let terminal = match &args.terminal {
            Some(t) => desktop::parse_terminal(t)?,
            None => desktop::detect_terminal()
                .ok_or_else(|| eyre!("no terminal emulator found; pass --terminal"))?,
        };
        desktop::launch_terminal(&terminal)?;
    }
    Ok(())
}

async fn run_tui(args: TuiArgs) -> Result<ExitCode> {
    let config_path = config_path(&args.config)?;
    let cfg = config::load(&config_path)?;
    // Built-ins are written out as files on first run, then everything in the
    // themes folder next to the config is offered.
    let themes_dir = config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("themes");
    let themes = theme::load_themes(&themes_dir, &cfg.themes).wrap_err_with(|| {
        format!(
            "invalid themes (config {} or folder {})",
            config_path.display(),
            themes_dir.display()
        )
    })?;

    if args.list_themes {
        for t in &themes {
            println!("{}", t.name);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let theme_name = args.theme.clone().unwrap_or_else(|| cfg.theme.clone());
    if theme::find(&themes, &theme_name).is_none() {
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        bail!(
            "unknown theme '{theme_name}'; available themes: {}",
            names.join(", ")
        );
    }

    let opts = daemon::Options {
        config_path: config_path.clone(),
        dir: args.dir.as_deref().map(std::path::absolute).transpose()?,
        all_files: args.all_files,
    };
    // Report config mistakes here, with the terminal still usable, rather
    // than from a daemon that dies in the background.
    daemon::validate(&opts)?;
    let conn = client::connect_or_start(&opts, true).await?;

    // Startup-only settings can't be changed on a daemon that is already up.
    if let Some(dir) = &opts.dir
        && std::path::Path::new(&conn.welcome.download_dir)
            != config::expand_tilde(&dir.to_string_lossy())
    {
        bail!(
            "the daemon is already running and downloads to {}, not {}; \
             run `torrentui daemon restart --dir {}` first",
            conn.welcome.download_dir,
            dir.display(),
            dir.display()
        );
    }
    if opts.all_files && conn.welcome.ask_files {
        bail!(
            "the daemon is already running with the file picker enabled; \
             run `torrentui daemon restart --all-files` first"
        );
    }

    let terminal = tui::init_terminal()?;
    let result = tui::run(
        terminal,
        conn,
        tui::Settings {
            config_path,
            themes,
            theme_name,
            transparent: cfg.transparent_background,
            ask_tags: cfg.ask_tags,
            inputs: args.inputs,
        },
    )
    .await;
    tui::restore_terminal();

    match result? {
        tui::Exit::Quit => {}
        tui::Exit::StopDaemon => {
            println!("Stopping the daemon…");
            client::stop().await?;
            println!("Daemon stopped.");
        }
        tui::Exit::Closed(reason) => {
            eprintln!("{reason}");
            // Stopped from elsewhere is expected; anything else is a failure.
            if reason != "the daemon stopped" {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
