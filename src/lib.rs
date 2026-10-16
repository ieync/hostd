//! hostd: route `NAME.localhost` to local TCP ports.
//!
//! The daemon listens on one loopback address, peeks at the `Host` header of each
//! incoming HTTP request to pick a backend port from a TOML routing table, and then
//! becomes a plain byte pipe between the client and that backend. Because nothing
//! after the request head is interpreted, WebSockets, SSE and streaming bodies work
//! unchanged.
//!
//! Module map:
//! - [`config`]: TOML parsing and validation
//! - [`router`]: shared routing table and `Host` header resolution
//! - [`http`]: reading and (optionally) rewriting the request head
//! - [`proxy`]: accept loop, connection handling and the byte pipe
//! - [`pages`]: the small HTML/plain-text responses hostd generates itself
//! - [`reload`]: hot reload of the config file

#![warn(missing_docs)]

pub mod config;
pub mod http;
pub mod pages;
pub mod proxy;
pub mod reload;
pub mod router;
