//! Scheduling: who fetches which bytes, over how many connections.
//!
//! The decisions live in pure functions over snapshots rather than inside a
//! running coordinator, so the policy that actually determines throughput can
//! be tested exhaustively without a network, a clock or a disk. Wiring those
//! decisions to real sockets is `idm-net`'s job.

pub mod conn;
pub mod ledger;
pub mod probe;
pub mod steal;

pub use conn::{ConnController, ConnDecision, ConnPolicy, HostProfile, Observation, SettleReason};
pub use ledger::{WorkLedger, claim_size, force_single_connection};
pub use probe::{ProbeOutcome, ResponseFacts, RestartReason, classify};
pub use steal::{Action, ClaimSnapshot, IdleReason, Protocol, StealPolicy, next_action};

/// Identifies one in-flight claim within a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimId(pub u32);

impl std::fmt::Display for ClaimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "claim{}", self.0)
    }
}
