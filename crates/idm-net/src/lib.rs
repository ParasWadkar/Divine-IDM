//! Transport adapters: real sockets wired to the core scheduler.
//!
//! `idm-core` holds the decisions — which bytes, how many connections, where to
//! cut a claim — and keeps them pure so they can be tested without a network.
//! This crate supplies the sockets: tuned clients, the lane topology that makes
//! concurrency real, header extraction, and the loop that turns a response body
//! into write jobs.

pub mod auth;
pub mod budget;
pub mod checksum;
pub mod client;
pub mod download;
pub mod facts;
pub mod fetch;
pub mod ftp;
pub mod limiter;
pub mod mirror;
pub mod range;
pub mod sftp;
pub mod stream;
pub mod topology;

/// The URL type this crate speaks, so callers can parse one without taking
/// a direct dependency on the HTTP client.
pub use reqwest::Url;
