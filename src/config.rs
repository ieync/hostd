//! Configuration: TOML parsing, normalisation and validation.
//!
//! The on-disk format is deliberately tiny (see `config.example.toml`). Parsing goes
//! through `Raw*` structs that mirror the file; [`Config`] is the validated form the
//! rest of the program uses, so no other module has to re-check names, ports or
//! addresses.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Address hostd binds when `[server].listen` is not set.
///
/// Port 80 is what makes `http://app.localhost` work without a port in the URL.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:80";

/// Domain suffix used when `[server].suffix` is not set.
///
/// Browsers resolve `*.localhost` to loopback on their own (RFC 6761), so no DNS or
/// `/etc/hosts` setup is needed. `.local` is avoided on purpose: it is reserved for
/// mDNS and resolvers may stall or hijack it.
pub const DEFAULT_SUFFIX: &str = "localhost";

/// The `[server]` table as written in the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawServer {
    listen: String,
    suffix: String,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_owned(),
            suffix: DEFAULT_SUFFIX.to_owned(),
        }
    }
}

/// One entry of `[routes]`: either `name = 8808` or `name = { port = 8808, ... }`.
#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "a port number, or a table like { port = 5173, rewrite_host = true }"
)]
enum RawRoute {
    Port(u16),
    Detailed(RawDetailed),
}

/// The table form of a route.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDetailed {
    port: u16,
    #[serde(default)]
    rewrite_host: bool,
}

/// The whole file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    routes: BTreeMap<String, RawRoute>,
}

/// A validated route: where requests for one name are sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Backend port on loopback.
    pub port: u16,
    /// Send `Host: 127.0.0.1:<port>` to the backend instead of the original host.
    ///
    /// Some dev servers (Vite, webpack) reject Host values they do not know.
    pub rewrite_host: bool,
}

/// A validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Loopback address the proxy listens on.
    pub listen: SocketAddr,
    /// Domain suffix, lowercase, without leading or trailing dots.
    pub suffix: String,
    /// Routes keyed by lowercase route name (one or more dot-separated DNS labels).
    pub routes: BTreeMap<String, Route>,
}

impl Config {
    /// Full host name for a route, e.g. `health.localhost`.
    pub fn host_for(&self, name: &str) -> String {
        format!("{name}.{}", self.suffix)
    }

    /// Browser URL for a route; the port is only shown when it is not 80.
    pub fn url_for(&self, name: &str) -> String {
        let host = self.host_for(name);
        match self.listen.port() {
            80 => format!("http://{host}"),
            port => format!("http://{host}:{port}"),
        }
    }
}

/// Everything that can go wrong while loading a config.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read(io::Error),
    /// The file is not valid TOML or does not match the expected shape.
    Parse(String),
    /// The file parsed, but a value is not acceptable.
    Invalid(String),
}

impl ConfigError {
    /// True when the config file simply does not exist (worth a friendlier hint).
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Read(e) if e.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "{e}"),
            Self::Parse(msg) => write!(f, "invalid TOML: {}", msg.trim_end()),
            Self::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            _ => None,
        }
    }
}

/// Shorthand for building an [`ConfigError::Invalid`].
fn invalid(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

/// Default config location: `$XDG_CONFIG_HOME/hostd/config.toml`, else
/// `~/.config/hostd/config.toml`.
pub fn default_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(dir).join("hostd/config.toml")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/hostd/config.toml")
    } else {
        PathBuf::from("hostd.toml")
    }
}

/// Read, parse and validate the config file at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
    parse(&text)
}

/// Parse and validate config text.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    validate(raw)
}

/// True for a single DNS label: 1-63 chars of `[a-z0-9-]`, no leading/trailing hyphen.
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// True for one or more valid labels joined by dots.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(valid_label)
}

/// Turn the raw file contents into a [`Config`], rejecting anything unsafe or ambiguous.
fn validate(raw: RawConfig) -> Result<Config, ConfigError> {
    let listen: SocketAddr = raw.server.listen.parse().map_err(|_| {
        invalid(format!(
            "server.listen {:?} is not an IP:PORT address",
            raw.server.listen
        ))
    })?;
    // Loopback only: hostd forwards to local services that were never meant to be
    // reachable from the network.
    if !listen.ip().is_loopback() {
        return Err(invalid(format!(
            "server.listen {listen} is not a loopback address; hostd only listens on 127.0.0.0/8 or ::1"
        )));
    }

    let suffix = raw.server.suffix.trim().to_ascii_lowercase();
    if !valid_name(&suffix) {
        return Err(invalid(format!(
            "server.suffix {:?} must be dot-separated lowercase DNS labels, e.g. \"localhost\"",
            raw.server.suffix
        )));
    }

    let mut routes = BTreeMap::new();
    for (name, route) in raw.routes {
        let lowered = name.to_ascii_lowercase();
        if !valid_name(&lowered) {
            return Err(invalid(format!(
                "route name {name:?} must be dot-separated DNS labels (letters, digits, hyphens)"
            )));
        }
        let (port, rewrite_host) = match route {
            RawRoute::Port(port) => (port, false),
            RawRoute::Detailed(d) => (d.port, d.rewrite_host),
        };
        if port == 0 {
            return Err(invalid(format!("route {name:?} has port 0")));
        }
        // A route to the listener itself would make every request loop forever.
        if port == listen.port() {
            return Err(invalid(format!(
                "route {name:?} points at port {port}, which is where hostd itself listens"
            )));
        }
        if routes
            .insert(lowered, Route { port, rewrite_host })
            .is_some()
        {
            return Err(invalid(format!(
                "route {name:?} is defined twice (names are case-insensitive)"
            )));
        }
    }

    Ok(Config {
        listen,
        suffix,
        routes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let cfg = parse("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:80".parse().unwrap());
        assert_eq!(cfg.suffix, "localhost");
        assert!(cfg.routes.is_empty());
    }

    #[test]
    fn shorthand_and_table_routes() {
        let cfg = parse(
            r#"
            [routes]
            health = 8808
            vite = { port = 5173, rewrite_host = true }
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.routes["health"],
            Route {
                port: 8808,
                rewrite_host: false
            }
        );
        assert_eq!(
            cfg.routes["vite"],
            Route {
                port: 5173,
                rewrite_host: true
            }
        );
    }

    #[test]
    fn names_are_lowercased_and_may_be_nested() {
        let cfg = parse("[routes]\n\"API.Health\" = 9000\n").unwrap();
        assert!(cfg.routes.contains_key("api.health"));
    }

    #[test]
    fn duplicate_after_lowercasing_is_rejected() {
        let err = parse("[routes]\nApp = 3000\napp = 3001\n").unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    #[test]
    fn bad_names_are_rejected() {
        for name in ["\"-x\"", "\"x-\"", "\"a b\"", "\"a..b\"", "\"\"", "\"é\""] {
            let err = parse(&format!("[routes]\n{name} = 3000\n")).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid(_)), "{name}: {err}");
        }
    }

    #[test]
    fn port_zero_and_out_of_range_are_rejected() {
        assert!(matches!(
            parse("[routes]\na = 0\n").unwrap_err(),
            ConfigError::Invalid(_)
        ));
        assert!(matches!(
            parse("[routes]\na = 70000\n").unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn non_loopback_listen_is_rejected() {
        for addr in ["0.0.0.0:80", "192.168.1.5:80", "[::]:80"] {
            let err = parse(&format!("[server]\nlisten = \"{addr}\"\n")).unwrap_err();
            assert!(err.to_string().contains("loopback"), "{addr}: {err}");
        }
        assert!(parse("[server]\nlisten = \"127.0.0.2:8080\"\n").is_ok());
        assert!(parse("[server]\nlisten = \"[::1]:8080\"\n").is_ok());
    }

    #[test]
    fn route_to_own_port_is_rejected() {
        let err = parse("[server]\nlisten = \"127.0.0.1:8080\"\n[routes]\nx = 8080\n").unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");
    }

    #[test]
    fn bad_suffix_is_rejected() {
        for suffix in ["", ".localhost", "local host", "-x"] {
            let err = parse(&format!("[server]\nsuffix = \"{suffix}\"\n")).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid(_)), "{suffix}: {err}");
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse("[server]\nlisten_on = \"127.0.0.1:80\"\n").is_err());
        assert!(parse("[routes]\na = { port = 1, rewrite = true }\n").is_err());
        assert!(parse("[extra]\nx = 1\n").is_err());
    }

    #[test]
    fn route_value_of_wrong_type_has_helpful_error() {
        let err = parse("[routes]\na = \"3000\"\n").unwrap_err();
        assert!(err.to_string().contains("port number"), "{err}");
    }

    #[test]
    fn url_hides_port_80_only() {
        let cfg = parse("[routes]\na = 3000\n").unwrap();
        assert_eq!(cfg.url_for("a"), "http://a.localhost");
        let cfg = parse("[server]\nlisten = \"127.0.0.1:8080\"\n[routes]\na = 3000\n").unwrap();
        assert_eq!(cfg.url_for("a"), "http://a.localhost:8080");
    }
}
