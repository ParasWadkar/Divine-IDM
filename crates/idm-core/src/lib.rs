//! Core of the download engine.
//!
//! The modules here are deliberately free of any transport, UI or storage
//! dependency so that the parts whose correctness matters most can be tested
//! headlessly.

pub mod extent;
pub mod journal;
pub mod pio;
pub mod sched;
pub mod win;
pub mod writer;

pub use extent::{ByteRange, ContentRange, ExtentError, RangeSet};
pub use journal::{Journal, JournalError, JournalMeta, ValidatorKind};
pub use sched::{Action, ClaimSnapshot, StealPolicy, WorkLedger};
pub use win::FileIdentity;
pub use writer::{ByteBudget, WriteAck, WriteJob, Writer, WriterConfig};
