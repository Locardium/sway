//! Sway's archive and sync server.
//!
//! The binary is a thin wrapper around this (read the config, open the
//! database, listen). It's split this way so tests can spin up a real
//! server, with its socket and its database, and talk to it over the real
//! protocol instead of asserting things about loose functions.

pub mod config;
pub mod host;
pub mod serve;
