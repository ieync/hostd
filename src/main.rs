//! `hostd`: the daemon. Listens on loopback and routes `NAME.localhost` to local ports.
//!
//! Runs entirely as the invoking user. The only privileged step is binding port 80, and
//! the README covers three ways to allow that without running as root.

use clap::Parser;
use hostd::config;
use hostd::proxy::{self, Options};
use hostd::reload;
use hostd::router::Router;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Command-line arguments.
#[derive(Parser)]
#[command(version, about = "Route NAME.localhost to local TCP ports")]
struct Args {
    /// Config file [default: $XDG_CONFIG_HOME/hostd/config.toml]
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Log every routed connection to stderr
    #[arg(short, long)]
    verbose: bool,
}

/// Print a hint for the two bind failures people actually hit.
fn explain_bind_error(e: &io::Error, listen: std::net::SocketAddr) {
    match e.kind() {
        io::ErrorKind::PermissionDenied if listen.port() < 1024 => {
            eprintln!("hostd: ports below 1024 need permission. Pick one:");
            eprintln!(
                "  doas sh -c 'echo net.ipv4.ip_unprivileged_port_start=80 > /etc/sysctl.d/99-hostd.conf && sysctl --system'"
            );
            eprintln!("  doas setcap cap_net_bind_service=+ep $(command -v hostd)");
            eprintln!("  or set [server] listen = \"127.0.0.1:8080\" in the config");
        }
        io::ErrorKind::AddrInUse => {
            eprintln!("hostd: {listen} is already in use (another hostd, or a web server?)");
        }
        _ => {}
    }
}

/// Load the config, bind the listener, start the reload watcher and serve forever.
fn main() -> ExitCode {
    let args = Args::parse();
    let path = args.config.unwrap_or_else(config::default_path);

    let config = match config::load(&path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("hostd: {}: {e}", path.display());
            if e.is_not_found() {
                eprintln!("hostd: create a starter config with `hostctl init`");
            }
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(config.listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("hostd: cannot listen on {}: {e}", config.listen);
            explain_bind_error(&e, config.listen);
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "hostd: listening on {}, {} routes under .{}",
        config.listen,
        config.routes.len(),
        config.suffix
    );

    let router = Arc::new(Router::new(config));
    if let Err(e) = reload::spawn(path, Arc::clone(&router), reload::POLL_INTERVAL) {
        // Not fatal: routing works, edits just need a restart to take effect.
        eprintln!("hostd: hot reload unavailable: {e}");
    }

    proxy::serve(
        listener,
        router,
        Options {
            verbose: args.verbose,
            ..Options::default()
        },
    );
    ExitCode::SUCCESS
}
