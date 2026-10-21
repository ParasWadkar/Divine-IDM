//! Windows-specific file primitives the engine depends on.
//!
//! Kept in one module so the `unsafe` surface is small, auditable, and covered
//! by tests, rather than scattered through the writer and the journal.

mod identity;

pub use identity::FileIdentity;
