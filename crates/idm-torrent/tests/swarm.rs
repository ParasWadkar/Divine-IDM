//! An end-to-end check against a real swarm.
//!
//! Ignored by default, and it has to be: it joins a DHT, talks to strangers
//! and uses whatever connection the machine has. `cargo test` must never do
//! any of that on its own.
//!
//! The unit tests cover the parts that can be reasoned about -- which
//! addresses are torrents, what each librqbit state means, how a limit maps.
//! None of them prove the pieces are wired together, because that needs a
//! swarm. This is the check that does, and the one thing it looks for is the
//! step the unit tests cannot reach: a magnet arrives knowing nothing about
//! itself, and the name and size have to appear once the metadata does.
//!
//! Run it with a magnet of your own -- a Linux ISO is the usual thing:
//!
//! ```text
//! set IDM_TORRENT_MAGNET=magnet:?xt=urn:btih:...
//! cargo test -p idm-torrent --test swarm -- --ignored --nocapture
//! ```
//!
//! It stops as soon as the metadata resolves and a peer connects. It does not
//! download the files.

use std::time::{Duration, Instant};

use idm_torrent::{Phase, Source, Torrents};

/// Long enough for a tracker announce and a DHT lookup on a slow link; short
/// enough that a magnet nobody is seeding fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(90);

#[tokio::test]
#[ignore = "joins a real BitTorrent swarm; set IDM_TORRENT_MAGNET and run with --ignored"]
async fn a_magnet_resolves_its_own_name_and_size_from_the_swarm() {
    let Ok(magnet) = std::env::var("IDM_TORRENT_MAGNET") else {
        panic!("set IDM_TORRENT_MAGNET to a magnet link to run this");
    };
    let source = Source::parse(&magnet).expect("that is not a torrent address");

    let dir = tempfile::tempdir().unwrap();
    let session = Torrents::start(dir.path(), dir.path().join("files"), 0)
        .await
        .expect("the session should start");

    let torrent = session.add(&source, &dir.path().join("files")).await.expect("should be added");

    // What a magnet does not know about itself when it starts, and the whole
    // reason the engine has to be able to rename a job already on screen.
    println!("placeholder name: {}", source.provisional_name());

    let started = Instant::now();
    let mut resolved = None;
    while started.elapsed() < PATIENCE {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let progress = torrent.progress();

        if let Phase::Failed(reason) = &progress.phase {
            panic!("the torrent failed: {reason}");
        }

        if let (Some(name), Some(total)) = (torrent.name(), progress.total) {
            resolved = Some((name, total, progress.peers));
            // Keep going only until somebody is actually talking to us: the
            // metadata can arrive from the DHT without a single peer, and a
            // run that stops there would not have proved the peer wire works.
            if progress.peers > 0 {
                break;
            }
        }
    }

    session.stop().await;

    let Some((name, total, peers)) = resolved else {
        panic!("no metadata arrived within {PATIENCE:?} -- is anything seeding this?");
    };
    println!("resolved to {name}, {total} bytes, {peers} peers");

    assert!(!name.trim().is_empty(), "the swarm should have supplied a name");
    assert!(total > 0, "the swarm should have supplied a size");
}
