//! The download engine as a library.
//!
//! Separated from the CLI because it has two front ends. The command line
//! drives it, and so does the desktop app, which hosts the engine in its own
//! process rather than talking to a daemon — closing the window hides to the
//! tray and downloads carry on. A binary crate cannot be depended on, so the
//! engine could not live in one.

pub mod archive;
pub mod batch;
pub mod browsers;
pub mod category;
pub mod checksum;
pub mod credentials;
pub mod failure;
pub mod finish;
pub mod folder;
pub mod grab;
pub mod hook;
pub mod list;
pub mod pac;
pub mod pairing;
pub mod proxy;
pub mod queues;
pub mod rpc;
pub mod scan;
pub mod schedule;
pub mod serve;
pub mod sidecars;
pub mod unpack;
pub mod video;
pub mod web;

pub use serve::Engine;

/// Opens the durable store, or explains why there is none.
///
/// Returns `None` rather than failing. Everything the store holds is a
/// convenience — what was learned about a host, what is in the history — and
/// none of it is needed to fetch bytes correctly. A database that will not
/// open should cost a re-climbed ramp, never a download.
pub fn open_store() -> Option<std::sync::Arc<idm_store::Store>> {
    match open_store_or_reason() {
        Ok(store) => Some(store),
        Err(reason) => {
            eprintln!("note: continuing without the store ({reason})");
            None
        }
    }
}

/// Like [`open_store`], but hands the failure to the caller instead of a
/// line on stderr — which, under `windows_subsystem = "windows"`, goes
/// nowhere. The window uses this to say *why* History is empty rather than
/// claiming nothing was ever downloaded.
pub fn open_store_or_reason() -> Result<std::sync::Arc<idm_store::Store>, String> {
    open_store_at(&store_path())
}

fn open_store_at(path: &std::path::Path) -> Result<std::sync::Arc<idm_store::Store>, String> {
    idm_store::Store::open(path).map(std::sync::Arc::new).map_err(|error| error.to_string())
}

/// `%LOCALAPPDATA%\DivineIDM\state.sqlite3`, beside the other engine state.
pub fn store_path() -> std::path::PathBuf {
    idm_ipc::state_dir().join("state.sqlite3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_that_will_not_open_reports_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        std::fs::write(&path, b"not a database at all").unwrap();
        let reason = open_store_at(&path).expect_err("junk must not open as a store");
        assert!(!reason.is_empty(), "the refusal should say what went wrong");
    }
}
