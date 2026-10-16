//! `hostctl`: validate the hostd config, list routes and create a starter config.
//!
//! It never talks to the daemon over a socket; it reads the same TOML file and probes
//! the backend ports directly, so it works whether or not hostd is running.

use clap::{Parser, Subcommand};
use hostd::config::{self, Config};
use hostd::proxy::connect_backend;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Starter config written by `hostctl init`.
const EXAMPLE_CONFIG: &str = include_str!("../../config.example.toml");

/// How long to wait when probing whether a port is up.
///
/// Loopback answers (or refuses) instantly; 200 ms only matters for a wedged listener
/// and keeps `list` snappy even with many routes.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Command-line arguments.
#[derive(Parser)]
#[command(version, about = "Inspect and validate the hostd config")]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/hostd/config.toml]
    #[arg(short, long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// Subcommands.
#[derive(Subcommand)]
enum Command {
    /// Validate the config file and exit non-zero if it is not usable
    Check,
    /// Show every route with its URL and whether its backend port is up
    List,
    /// Write a commented starter config
    Init {
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },
}

/// Load the config or print the error.
fn load(path: &Path) -> Option<Config> {
    match config::load(path) {
        Ok(config) => Some(config),
        Err(e) => {
            eprintln!("hostctl: {}: {e}", path.display());
            if e.is_not_found() {
                eprintln!("hostctl: create one with `hostctl init`");
            }
            None
        }
    }
}

/// True if something accepts TCP connections on loopback `port`.
fn port_is_up(port: u16) -> bool {
    connect_backend(port, PROBE_TIMEOUT).is_ok()
}

/// `hostctl check`.
fn check(path: &Path) -> ExitCode {
    let Some(config) = load(path) else {
        return ExitCode::FAILURE;
    };
    println!(
        "ok: {} routes, listening on {}, suffix .{}",
        config.routes.len(),
        config.listen,
        config.suffix
    );
    ExitCode::SUCCESS
}

/// `hostctl list`.
fn list(path: &Path) -> ExitCode {
    let Some(config) = load(path) else {
        return ExitCode::FAILURE;
    };
    let daemon = if TcpStream::connect_timeout(&config.listen, PROBE_TIMEOUT).is_ok() {
        "running"
    } else {
        "not running"
    };
    println!("hostd {} ({daemon})\n", config.listen);

    let mut rows = vec![vec![
        "NAME".to_owned(),
        "URL".to_owned(),
        "PORT".to_owned(),
        "STATUS".to_owned(),
    ]];
    for (name, route) in &config.routes {
        rows.push(vec![
            name.clone(),
            config.url_for(name),
            route.port.to_string(),
            if port_is_up(route.port) { "up" } else { "down" }.to_owned(),
        ]);
    }
    let widths: Vec<usize> = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    ExitCode::SUCCESS
}

/// `hostctl init`.
fn init(path: &Path, force: bool) -> ExitCode {
    if path.exists() && !force {
        eprintln!(
            "hostctl: {} already exists (use --force to overwrite)",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("hostctl: cannot create {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    }
    match fs::write(path, EXAMPLE_CONFIG) {
        Ok(()) => {
            println!("wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hostctl: cannot write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Dispatch the chosen subcommand.
fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    match cli.command {
        Command::Check => check(&path),
        Command::List => list(&path),
        Command::Init { force } => init(&path, force),
    }
}
