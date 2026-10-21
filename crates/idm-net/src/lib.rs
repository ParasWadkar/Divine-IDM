//! Transport adapters: real sockets wired to the core scheduler.
//!
//! `idm-core` holds the decisions — which bytes, how many connections, where to
//! cut a claim — and keeps them pure so they can be tested without a network.
//! This crate supplies the sockets: tuned clients, the lane topology that makes
//! concurrency real, header extraction, and the loop that turns a response body
//! into write jobs.

pub mod budget;
pub mod client;
pub mod facts;
pub mod fetch;
