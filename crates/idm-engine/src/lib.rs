//! The download engine as a library.
//!
//! Separated from the CLI because it has two front ends. The command line
//! drives it, and so does the desktop app, which hosts the engine in its own
//! process rather than talking to a daemon — closing the window hides to the
//! tray and downloads carry on. A binary crate cannot be depended on, so the
//! engine could not live in one.

pub mod pairing;
pub mod serve;

pub use serve::Engine;

/// Opens the durable store, or explains why there is none.
///
/// Returns `None` rather than failing. Everything the store holds is a
/// convenience — what was learned about a host, what is in the history — and
/// none of it is needed to fetch bytes correctly. A database that will not
/// open should cost a re-climbed ramp, never a download.
pub fn open_store() -> Option<std::sync::Arc<idm_store::Store>> {
    let path = store_path();
    match idm_store::Store::open(&path) {
        Ok(store) => Some(std::sync::Arc::new(store)),
        Err(error) => {
            eprintln!("note: continuing without the store ({error})");
            None
        }
    }
}

/// `%LOCALAPPDATA%\DivineIDM\state.sqlite3`, beside the other engine state.
pub fn store_path() -> std::path::PathBuf {
    idm_ipc::state_dir().join("state.sqlite3")
}
