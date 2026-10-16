//! Shared routing table and `Host` header resolution.
//!
//! [`Router`] holds the current [`Config`] behind a lock so the reload thread can swap
//! it while connection threads keep serving. Connections take a cheap snapshot
//! (an `Arc` clone) once per request and never hold the lock while doing I/O.

use crate::config::{Config, Route};
use std::sync::{Arc, PoisonError, RwLock};

/// The live, hot-swappable configuration.
pub struct Router {
    current: RwLock<Arc<Config>>,
}

impl Router {
    /// Create a router serving `config`.
    pub fn new(config: Config) -> Self {
        Self {
            current: RwLock::new(Arc::new(config)),
        }
    }

    /// Cheap handle to the configuration as it is right now.
    pub fn snapshot(&self) -> Arc<Config> {
        // A poisoned lock only means another thread panicked mid-swap; the Arc inside
        // is still a complete, valid config, so keep serving it.
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Atomically replace the configuration; in-flight connections keep their snapshot.
    pub fn replace(&self, config: Config) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(config);
    }
}

/// What a request's `Host` header maps to.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// A configured route: forward the connection.
    Route {
        /// The route's name as written in the config.
        name: &'a str,
        /// Where it goes.
        route: &'a Route,
    },
    /// The bare suffix (e.g. `localhost`): show the index of routes.
    Index,
    /// Inside the suffix but no such route: 404 that lists the routes.
    ///
    /// Safe to list them because a `*.localhost` Host header cannot be forged by a
    /// web page, unlike an arbitrary hostname pointed at 127.0.0.1.
    Missing(String),
    /// Not our domain (or no Host at all): bare 404 that reveals nothing.
    ///
    /// This is also what stops DNS-rebinding attacks: `evil.example` resolving to
    /// 127.0.0.1 gets nothing from any backend.
    Foreign,
}

/// Lowercase the header, drop an optional `:port` and one trailing dot.
fn normalise_host(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    let no_port = match lowered.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => lowered.as_str(),
    };
    no_port.strip_suffix('.').unwrap_or(no_port).to_owned()
}

/// `app.localhost` with suffix `localhost` gives `app`; anything else gives `None`.
fn strip_domain<'h>(host: &'h str, suffix: &str) -> Option<&'h str> {
    let name = host.strip_suffix(suffix)?.strip_suffix('.')?;
    (!name.is_empty()).then_some(name)
}

/// Map a `Host` header value to a [`Resolution`] under `config`.
pub fn resolve<'a>(config: &'a Config, host: Option<&str>) -> Resolution<'a> {
    let Some(host) = host else {
        return Resolution::Foreign;
    };
    let host = normalise_host(host);
    if host == config.suffix {
        return Resolution::Index;
    }
    match strip_domain(&host, &config.suffix) {
        None => Resolution::Foreign,
        Some(name) => match config.routes.get_key_value(name) {
            Some((name, route)) => Resolution::Route { name, route },
            None => Resolution::Missing(name.to_owned()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    fn cfg() -> Config {
        // Dotted names must be quoted, or TOML reads them as a nested table.
        parse("[routes]\nhealth = 8808\n\"api.health\" = 9000\n").unwrap()
    }

    fn route_name<'a>(r: &Resolution<'a>) -> Option<&'a str> {
        match r {
            Resolution::Route { name, .. } => Some(name),
            _ => None,
        }
    }

    #[test]
    fn plain_and_decorated_hosts_resolve() {
        let c = cfg();
        for host in [
            "health.localhost",
            "HEALTH.LocalHost",
            "health.localhost:80",
            "health.localhost:8080",
            "health.localhost.",
            "  health.localhost  ",
        ] {
            assert_eq!(
                route_name(&resolve(&c, Some(host))),
                Some("health"),
                "{host}"
            );
        }
    }

    #[test]
    fn nested_names_resolve() {
        let c = cfg();
        assert_eq!(
            route_name(&resolve(&c, Some("api.health.localhost"))),
            Some("api.health")
        );
    }

    #[test]
    fn bare_suffix_is_index() {
        let c = cfg();
        assert_eq!(resolve(&c, Some("localhost")), Resolution::Index);
        assert_eq!(resolve(&c, Some("localhost:80")), Resolution::Index);
    }

    #[test]
    fn unknown_name_inside_suffix_is_missing() {
        let c = cfg();
        assert_eq!(
            resolve(&c, Some("nope.localhost")),
            Resolution::Missing("nope".to_owned())
        );
        // `health` is a route but `x.health` is not.
        assert_eq!(
            resolve(&c, Some("x.health.localhost")),
            Resolution::Missing("x.health".to_owned())
        );
    }

    #[test]
    fn foreign_hosts_reveal_nothing() {
        let c = cfg();
        for host in [
            "evil.example",
            "127.0.0.1",
            "127.0.0.1:80",
            "[::1]:80",
            "health.localhost.evil.example",
            "xhealth.localhost",
            ".localhost",
            "healthlocalhost",
        ] {
            let got = resolve(&c, Some(host));
            assert!(
                matches!(got, Resolution::Foreign | Resolution::Missing(_)),
                "{host}: {got:?}"
            );
            assert_ne!(route_name(&got), Some("health"), "{host}");
        }
        assert_eq!(resolve(&c, Some("evil.example")), Resolution::Foreign);
        assert_eq!(resolve(&c, None), Resolution::Foreign);
    }

    #[test]
    fn replace_swaps_table_but_old_snapshots_survive() {
        let router = Router::new(cfg());
        let before = router.snapshot();
        router.replace(parse("[routes]\nother = 1234\n").unwrap());
        let after = router.snapshot();
        assert!(before.routes.contains_key("health"));
        assert!(!after.routes.contains_key("health"));
        assert!(after.routes.contains_key("other"));
    }
}
