//! Which browser extensions the user has approved.
//!
//! # What this is, and what it is not
//!
//! It is **not** the security boundary. The endpoint is a named pipe whose
//! DACL admits only this user, and the token lives in a file only this user
//! can read. A process that can reach the endpoint at all is already running
//! as the user, and pairing cannot change that.
//!
//! What pairing adds is the part the operating system cannot express:
//!
//! * **Consent** — a new extension does not start driving the downloader
//!   until a human says so, in the app, out of band from the request.
//! * **Visibility** — the user can see what is connected.
//! * **Revocation** — one extension can be cut off without uninstalling
//!   anything or rotating the token for everything else.
//!
//! Approval never comes from the requester. A pairing request only ever gets
//! "pending" back; the answer arrives from a different client that the engine
//! does not consider a browser.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use idm_ipc::{PairedClient, PendingPairing};
use serde::{Deserialize, Serialize};

/// Requests older than this are forgotten, so an approval prompt cannot be
/// answered days later for a request the user has long forgotten making.
pub const PENDING_TTL_SECS: u64 = 300;

/// A cap on outstanding requests.
///
/// Without one, anything able to reach the endpoint could queue thousands of
/// prompts and wear the user down into approving one. Past the cap the oldest
/// request is dropped rather than the newest refused, so a genuine request
/// made after a flood still reaches the user.
pub const MAX_PENDING: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutcome {
    AlreadyPaired,
    Pending {
        request_id: String,
        /// False when this is an existing request being returned again, so
        /// callers can notify a human once rather than on every greeting.
        /// The extension greets on each service worker wake, which is often.
        fresh: bool,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    paired: Vec<PairedClient>,
}

/// The set of approved clients, and the requests waiting on the user.
///
/// Pending requests are deliberately in memory only: a request that did not
/// get answered before a restart should not survive one.
#[derive(Debug)]
pub struct PairingStore {
    path: PathBuf,
    paired: HashMap<String, PairedClient>,
    pending: Vec<PendingPairing>,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

impl PairingStore {
    pub fn load(path: PathBuf) -> Self {
        let persisted: Persisted = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let paired =
            persisted.paired.into_iter().map(|client| (client.client_id.clone(), client)).collect();
        Self { path, paired, pending: Vec::new() }
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let persisted = Persisted { paired: self.paired.values().cloned().collect() };
        if let Ok(json) = serde_json::to_vec_pretty(&persisted) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    pub fn is_paired(&self, client_id: &str) -> bool {
        self.paired.contains_key(client_id)
    }

    fn expire(&mut self) {
        let cutoff = now_unix().saturating_sub(PENDING_TTL_SECS);
        self.pending.retain(|request| request.requested_at_unix >= cutoff);
    }

    /// Records a request for the user to answer.
    ///
    /// Repeating a request for the same client replaces the previous one
    /// rather than stacking up, so a retrying extension cannot flood the
    /// prompt list on its own.
    pub fn request(&mut self, client_id: &str, label: &str) -> PairingOutcome {
        self.expire();
        if self.is_paired(client_id) {
            return PairingOutcome::AlreadyPaired;
        }
        if let Some(existing) = self.pending.iter().find(|p| p.client_id == client_id) {
            return PairingOutcome::Pending {
                request_id: existing.request_id.clone(),
                fresh: false,
            };
        }

        let request_id = idm_ipc::security::random_id();
        self.pending.push(PendingPairing {
            request_id: request_id.clone(),
            client_id: client_id.to_owned(),
            // The label is attacker-controlled text that will be shown to a
            // human, so it is truncated and stripped of control characters
            // that could garble the prompt it appears in.
            label: sanitise_label(label),
            requested_at_unix: now_unix(),
        });
        while self.pending.len() > MAX_PENDING {
            self.pending.remove(0);
        }
        PairingOutcome::Pending { request_id, fresh: true }
    }

    /// Approves a pending request. Returns the client that was paired.
    pub fn approve(&mut self, request_id: &str) -> Option<PairedClient> {
        self.expire();
        let index = self.pending.iter().position(|p| p.request_id == request_id)?;
        let request = self.pending.remove(index);
        let client = PairedClient {
            client_id: request.client_id.clone(),
            label: request.label,
            paired_at_unix: now_unix(),
        };
        self.paired.insert(request.client_id, client.clone());
        self.save();
        Some(client)
    }

    pub fn deny(&mut self, request_id: &str) -> bool {
        self.expire();
        match self.pending.iter().position(|p| p.request_id == request_id) {
            Some(index) => {
                self.pending.remove(index);
                true
            }
            None => false,
        }
    }

    /// Cuts off a paired client. Takes effect on its next request.
    pub fn revoke(&mut self, client_id: &str) -> bool {
        let removed = self.paired.remove(client_id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    pub fn paired(&self) -> Vec<PairedClient> {
        let mut listed: Vec<PairedClient> = self.paired.values().cloned().collect();
        listed.sort_by(|a, b| a.label.cmp(&b.label));
        listed
    }

    pub fn pending(&mut self) -> Vec<PendingPairing> {
        self.expire();
        self.pending.clone()
    }
}

/// Makes a client-supplied label safe to show a human.
///
/// The label arrives from the extension, so it is untrusted. Newlines and
/// control characters could be used to forge extra lines in an approval
/// prompt ("... Approved by Microsoft"), so they go, and the length is capped.
fn sanitise_label(label: &str) -> String {
    let cleaned: String =
        label.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_owned();
    if cleaned.is_empty() { "Unnamed extension".to_owned() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PairingStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::load(dir.path().join("pairings.json"));
        (dir, store)
    }

    #[test]
    fn a_new_client_is_not_paired_until_a_human_approves() {
        let (_dir, mut store) = store();
        assert!(!store.is_paired("abc"));

        let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
            panic!("a new client should be pending, never auto-approved");
        };
        // Still not paired: asking is not the same as being allowed.
        assert!(!store.is_paired("abc"));

        assert!(store.approve(&request_id).is_some());
        assert!(store.is_paired("abc"));
    }

    #[test]
    fn an_already_paired_client_does_not_prompt_again() {
        let (_dir, mut store) = store();
        let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
            panic!("expected pending")
        };
        store.approve(&request_id);
        assert_eq!(store.request("abc", "Chrome"), PairingOutcome::AlreadyPaired);
        assert!(store.pending().is_empty());
    }

    #[test]
    fn repeating_a_request_reuses_the_same_prompt() {
        // A retrying extension must not be able to stack up prompts.
        let (_dir, mut store) = store();
        let first = store.request("abc", "Chrome");
        let second = store.request("abc", "Chrome");

        let (
            PairingOutcome::Pending { request_id: first_id, fresh: first_fresh },
            PairingOutcome::Pending { request_id: second_id, fresh: second_fresh },
        ) = (first, second)
        else {
            panic!("both requests should be pending");
        };

        assert_eq!(first_id, second_id, "a repeat must reuse the same prompt");
        assert!(first_fresh, "the first request is new");
        assert!(!second_fresh, "a repeat is not new, so it must not notify again");
        assert_eq!(store.pending().len(), 1);
    }

    #[test]
    fn a_flood_of_requests_is_capped() {
        let (_dir, mut store) = store();
        for index in 0..50 {
            store.request(&format!("client-{index}"), "Spam");
        }
        assert_eq!(store.pending().len(), MAX_PENDING, "prompts should be capped");
        // The newest survive, so a genuine request after a flood still shows.
        let pending = store.pending();
        assert!(pending.iter().any(|p| p.client_id == "client-49"));
    }

    #[test]
    fn denying_removes_the_request_without_pairing() {
        let (_dir, mut store) = store();
        let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
            panic!("expected pending")
        };
        assert!(store.deny(&request_id));
        assert!(!store.is_paired("abc"));
        assert!(store.pending().is_empty());
        // Denying twice is simply false, not an error.
        assert!(!store.deny(&request_id));
    }

    #[test]
    fn approving_an_unknown_request_does_nothing() {
        let (_dir, mut store) = store();
        assert!(store.approve("not-a-real-request").is_none());
        assert!(store.paired().is_empty());
    }

    #[test]
    fn revoking_takes_effect_immediately() {
        let (_dir, mut store) = store();
        let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
            panic!("expected pending")
        };
        store.approve(&request_id);
        assert!(store.is_paired("abc"));

        assert!(store.revoke("abc"));
        assert!(!store.is_paired("abc"), "revocation must be instant");
        assert!(!store.revoke("abc"), "revoking twice is just false");
    }

    #[test]
    fn pairings_survive_a_restart_but_pending_requests_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairings.json");
        {
            let mut store = PairingStore::load(path.clone());
            let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
                panic!("expected pending")
            };
            store.approve(&request_id);
            // An unanswered request, which should not outlive the process.
            store.request("xyz", "Firefox");
        }
        let mut reopened = PairingStore::load(path);
        assert!(reopened.is_paired("abc"), "an approval should persist");
        assert!(!reopened.is_paired("xyz"));
        assert!(
            reopened.pending().is_empty(),
            "an unanswered request should not survive a restart"
        );
    }

    #[test]
    fn an_expired_request_can_no_longer_be_approved() {
        let (_dir, mut store) = store();
        let PairingOutcome::Pending { request_id, .. } = store.request("abc", "Chrome") else {
            panic!("expected pending")
        };
        // Backdate it past the time-to-live.
        store.pending[0].requested_at_unix = now_unix() - PENDING_TTL_SECS - 1;

        assert!(store.pending().is_empty(), "an expired request should be gone");
        assert!(store.approve(&request_id).is_none(), "and must not be approvable");
        assert!(!store.is_paired("abc"));
    }

    #[test]
    fn a_hostile_label_cannot_forge_lines_in_the_prompt() {
        // The label is attacker-controlled and gets shown to a human.
        let (_dir, mut store) = store();
        store.request("abc", "Chrome\n\nApproved by Microsoft\r\nTrusted: yes");
        let shown = &store.pending()[0].label;
        assert!(!shown.contains('\n'), "newlines could forge extra lines: {shown:?}");
        assert!(!shown.contains('\r'), "{shown:?}");
    }

    #[test]
    fn labels_are_capped_and_never_empty() {
        let (_dir, mut store) = store();
        store.request("long", &"A".repeat(500));
        assert!(store.pending()[0].label.len() <= 64);

        store.request("blank", "   ");
        let blank = store.pending().into_iter().find(|p| p.client_id == "blank").unwrap();
        assert_eq!(blank.label, "Unnamed extension");
    }
}
