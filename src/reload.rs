//! Hot reload: pick up edits to the config file without restarting.
//!
//! A background thread stats the file on a timer. When it changes, the file is parsed
//! and validated in full; only a good config replaces the live routing table, so a typo
//! mid-edit never takes the routes down.

use crate::config::{self, Config};
use crate::router::Router;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

/// How often the config file is checked for changes.
///
/// One `stat` every 2 s is effectively free, and a two-second delay between saving the
/// file and the new name working is not noticeable. inotify would need an extra
/// dependency or unsafe FFI for no practical gain at this scale.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Identity of a config file's contents as far as `stat` can tell.
///
/// Includes the inode because editors that save by writing a temp file and renaming it
/// can leave mtime and size unchanged within one timestamp tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

/// Stat `path` (following symlinks, so dotfile-manager links work).
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        inode: meta.ino(),
    })
}

/// What a reload attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The file parses to the same routes that are already live.
    Unchanged,
    /// The new routing table is live.
    Reloaded {
        /// Route names that did not exist before.
        added: Vec<String>,
        /// Route names that no longer exist.
        removed: Vec<String>,
        /// Route names whose port or options changed.
        changed: Vec<String>,
    },
    /// The file was not applied; the previous routes stay live. Holds the reason.
    Rejected(String),
}

/// Load `path` and, if it is valid and different, swap it into `router`.
pub fn reload_once(path: &Path, router: &Router) -> Outcome {
    let mut new = match config::load(path) {
        Ok(config) => config,
        Err(e) => return Outcome::Rejected(e.to_string()),
    };
    let old = router.snapshot();

    // The socket is already bound, so a changed listen address cannot take effect
    // without a restart. Keep reporting the real one on the index page.
    let mut note = None;
    if new.listen != old.listen {
        note = Some(format!(
            "[server].listen changed to {} but needs a restart; still listening on {}",
            new.listen, old.listen
        ));
        new.listen = old.listen;
        if let Some((name, route)) = new
            .routes
            .iter()
            .find(|(_, route)| route.port == old.listen.port())
        {
            return Outcome::Rejected(format!(
                "route {name:?} points at port {}, which is where hostd itself listens",
                route.port
            ));
        }
    }
    if let Some(note) = note {
        eprintln!("hostd: {note}");
    }

    if *old == new {
        return Outcome::Unchanged;
    }
    let outcome = diff(&old, &new);
    router.replace(new);
    outcome
}

/// Describe how the route set differs between two configs.
fn diff(old: &Config, new: &Config) -> Outcome {
    let old_names: BTreeSet<&String> = old.routes.keys().collect();
    let new_names: BTreeSet<&String> = new.routes.keys().collect();
    let added = new_names
        .difference(&old_names)
        .map(|n| (*n).clone())
        .collect();
    let removed = old_names
        .difference(&new_names)
        .map(|n| (*n).clone())
        .collect();
    let changed = new
        .routes
        .iter()
        .filter(|(name, route)| old.routes.get(*name).is_some_and(|prev| prev != *route))
        .map(|(name, _)| name.clone())
        .collect();
    Outcome::Reloaded {
        added,
        removed,
        changed,
    }
}

/// Start the watcher thread. It runs for the life of the process.
pub fn spawn(path: PathBuf, router: Arc<Router>, interval: Duration) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("reload".into())
        .spawn(move || watch(&path, &router, interval))
}

/// Poll loop: reload whenever the file's stamp changes.
fn watch(path: &Path, router: &Router, interval: Duration) {
    let mut last = stamp(path);
    loop {
        thread::sleep(interval);
        // Stamp before reading: if the file changes between the two, the next tick sees
        // a different stamp and reloads again, so no edit is ever missed.
        let now = stamp(path);
        if now == last {
            continue;
        }
        last = now.clone();
        if now.is_none() {
            eprintln!(
                "hostd: config file {} is missing; keeping current routes",
                path.display()
            );
            continue;
        }
        match reload_once(path, router) {
            Outcome::Unchanged => {}
            Outcome::Reloaded {
                added,
                removed,
                changed,
            } => eprintln!(
                "hostd: reloaded {}: {} routes (+{} -{} ~{})",
                path.display(),
                router.snapshot().routes.len(),
                added.len(),
                removed.len(),
                changed.len()
            ),
            Outcome::Rejected(reason) => {
                eprintln!("hostd: reload rejected, keeping current routes: {reason}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Write `text` to a unique temp file and return its path.
    fn temp_config(text: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("hostd-test-{}-{n}.toml", std::process::id()));
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn valid_edit_is_applied_with_a_diff() {
        let path = temp_config("[routes]\na = 1001\nb = 1002\n");
        let router = Router::new(config::load(&path).unwrap());
        fs::write(&path, "[routes]\nb = 2002\nc = 1003\n").unwrap();
        let outcome = reload_once(&path, &router);
        assert_eq!(
            outcome,
            Outcome::Reloaded {
                added: vec!["c".into()],
                removed: vec!["a".into()],
                changed: vec!["b".into()],
            }
        );
        assert_eq!(router.snapshot().routes["b"].port, 2002);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn broken_edit_keeps_old_routes() {
        let path = temp_config("[routes]\na = 1001\n");
        let router = Router::new(config::load(&path).unwrap());
        fs::write(&path, "[routes]\na = \n").unwrap();
        assert!(matches!(reload_once(&path, &router), Outcome::Rejected(_)));
        assert_eq!(router.snapshot().routes["a"].port, 1001);
        fs::write(&path, "[routes]\na = 0\n").unwrap();
        assert!(matches!(reload_once(&path, &router), Outcome::Rejected(_)));
        assert_eq!(router.snapshot().routes["a"].port, 1001);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn identical_content_is_unchanged() {
        let path = temp_config("[routes]\na = 1001\n");
        let router = Router::new(config::load(&path).unwrap());
        fs::write(&path, "# comment only change\n[routes]\na = 1001\n").unwrap();
        assert_eq!(reload_once(&path, &router), Outcome::Unchanged);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn listen_change_is_not_applied() {
        let path = temp_config("[server]\nlisten = \"127.0.0.1:8080\"\n[routes]\na = 1001\n");
        let router = Router::new(config::load(&path).unwrap());
        fs::write(
            &path,
            "[server]\nlisten = \"127.0.0.1:9090\"\n[routes]\na = 1001\nb = 1002\n",
        )
        .unwrap();
        assert!(matches!(
            reload_once(&path, &router),
            Outcome::Reloaded { .. }
        ));
        let live = router.snapshot();
        assert_eq!(live.listen.port(), 8080);
        assert!(live.routes.contains_key("b"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn listen_change_cannot_smuggle_in_a_loop_route() {
        let path = temp_config("[server]\nlisten = \"127.0.0.1:8080\"\n[routes]\na = 1001\n");
        let router = Router::new(config::load(&path).unwrap());
        // Valid for the *new* listen address (9090), but 8080 is where we really listen.
        fs::write(
            &path,
            "[server]\nlisten = \"127.0.0.1:9090\"\n[routes]\na = 8080\n",
        )
        .unwrap();
        assert!(matches!(reload_once(&path, &router), Outcome::Rejected(_)));
        assert_eq!(router.snapshot().routes["a"].port, 1001);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn stamp_changes_when_file_is_replaced_by_rename() {
        let path = temp_config("[routes]\n");
        let before = stamp(&path).unwrap();
        let tmp = path.with_extension("new");
        fs::write(&tmp, "[routes]\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_ne!(stamp(&path).unwrap(), before);
        fs::remove_file(path).unwrap();
    }
}
