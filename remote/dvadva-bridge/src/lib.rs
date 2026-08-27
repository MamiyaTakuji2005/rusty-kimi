//! `dvadva-bridge` — the relay daemon pair that carries a `dvadva-agent` wire
//! connection across the network.
//!
//! Two halves, one binary (see `remote/PLAN.md` for the design contract):
//!
//! - [`remote_daemon`] runs on the machine that hosts the agent. Per
//!   connection it spawns `dvadva-agent` with caller-chosen arguments and
//!   relays bytes both ways.
//! - [`local_daemon`] runs on the frontend machine. It accepts the same
//!   connections and forwards them to the remote daemon.
//!
//! Both halves are **dumb byte relays**: the only thing either ever parses
//! is [`proto`]'s one header line per connection. Everything after it is
//! the dvadva-agent wire protocol — opaque newline-delimited JSON that flows
//! untouched between frontend and agent. There is deliberately no
//! authentication or encryption: bind both daemons to loopback and cross
//! the network through an ssh tunnel (`ssh -L`), never expose them
//! directly.
//!
//! One agent per connection; the connection's lifetime *is* the agent's
//! lifetime (close in either direction propagates to the other side).

pub mod config;
pub mod local_daemon;
pub mod proto;
pub mod remote_daemon;
