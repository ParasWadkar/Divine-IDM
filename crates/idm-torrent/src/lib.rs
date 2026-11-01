//! BitTorrent downloads, on librqbit's engine.
//!
//! Everything else this workspace fetches goes through `idm-core`: a plan, a
//! set of byte extents, claims handed to workers, one coalescing writer, one
//! journal. A torrent goes through none of it. There is no address to ask for
//! byte ranges, no server to ask; there is a swarm, a piece picker, a peer
//! wire protocol, a tracker and a DHT, and librqbit already has all of them
//! along with its own storage and resume. Reimplementing that against our
//! scheduler would be a second product's worth of work for a worse result.
//!
//! So this crate is a *translation layer*, not an engine. It turns the two
//! things a user hands over -- a magnet link or a `.torrent` -- into one
//! running torrent, and turns librqbit's statistics into the same progress
//! shape every other download in this app reports. Where the two models
//! genuinely differ, the difference is noted rather than hidden:
//!
//! * A torrent is usually *many* files under one folder, so what the rest of
//!   the app calls a filename is the torrent's folder name here.
//! * A magnet does not know its own name. It arrives with the metadata, from
//!   the swarm, seconds or minutes after the job starts. Until then there is
//!   only whatever `dn=` the link carried, or the info hash.
//! * `connections` is a count of connected peers, which is not something this
//!   side chooses. Nothing here climbs a connection count the way the HTTP
//!   scheduler does; the swarm decides.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState,
};

mod source;

pub use source::Source;

#[derive(Debug, thiserror::Error)]
pub enum TorrentError {
    #[error("could not start the torrent engine: {0}")]
    Session(String),
    #[error("could not read {path}: {message}")]
    Unreadable { path: String, message: String },
    #[error("{0}")]
    Refused(String),
}

type Result<T> = std::result::Result<T, TorrentError>;

/// How far along a torrent is, in the terms the rest of the app uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub phase: Phase,
    /// Verified bytes on disk. Not bytes received: a piece that failed its
    /// hash is thrown away, and counting it would show progress that later
    /// went backwards.
    pub downloaded: u64,
    /// `None` until the metadata arrives, which for a magnet means after the
    /// job has already started.
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
    /// Bytes handed to other peers. Shown because a torrent is the one kind
    /// of download here that also sends.
    pub uploaded: u64,
    /// Peers actually connected, as opposed to known of.
    pub peers: u32,
}

/// What a torrent is doing, in the smallest set of states that are actually
/// distinguishable to someone looking at the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Fetching the metadata, or checking what is already on disk. Both can
    /// take a while and neither has byte progress to show, so they are one
    /// state: something is happening and no bytes are arriving yet.
    Preparing,
    Running,
    Paused,
    Finished,
    Failed(String),
}

/// What a torrent turns out to be, read before it starts.
///
/// Carries the resolved metadata so starting it needs no second lookup --
/// see [`Torrents::start_listed`].
pub struct Listing {
    /// The torrent's own name: one file's name, or the folder a set of files
    /// goes in.
    pub name: String,
    pub total: u64,
    pub file_count: usize,
    /// The largest file in the torrent, which is what it is really *for*.
    ///
    /// The name to judge a torrent's kind by. A release is a film of two
    /// gigabytes next to subtitles of forty kilobytes, and the torrent's own
    /// name is a folder name with no extension at all, so neither of those
    /// answers the question.
    pub principal_file: Option<String>,
    /// The `.torrent`, as the swarm or the file supplied it.
    bytes: bytes::Bytes,
    /// Peers met while looking the metadata up, so the real start is warm.
    peers: Vec<std::net::SocketAddr>,
}

impl std::fmt::Debug for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listing")
            .field("name", &self.name)
            .field("total", &self.total)
            .field("file_count", &self.file_count)
            .field("principal_file", &self.principal_file)
            .finish_non_exhaustive()
    }
}

/// The app's single BitTorrent session.
///
/// One per process, not one per torrent. A session owns a listening socket, a
/// DHT node and a peer id; starting one per download would open a port per
/// download, run that many DHT nodes, and introduce itself to one swarm as
/// several unrelated clients.
pub struct Torrents {
    session: Arc<Session>,
}

impl Torrents {
    /// Starts the session.
    ///
    /// Deliberately not called until the first torrent is added: someone who
    /// never uses one should not have this app listening on a port, joining a
    /// DHT or announcing itself to anybody.
    pub async fn start(state_dir: &Path, download_dir: PathBuf, limit_bps: u64) -> Result<Self> {
        let options = SessionOptions {
            // librqbit's own resume data -- which pieces are verified, which
            // torrents were running -- kept beside this app's other state
            // rather than in librqbit's default per-OS folder, so everything
            // belonging to this app is in one place and uninstalling takes
            // it all.
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(state_dir.join("torrents")),
            }),
            // Resume without re-hashing every piece already on disk. A full
            // verify of a large torrent is minutes of disk for an answer
            // librqbit wrote down when it stopped.
            fastresume: true,
            ratelimits: limits(limit_bps),
            ..Default::default()
        };

        let session = Session::new_with_opts(download_dir, options)
            .await
            .map_err(|error| TorrentError::Session(error.to_string()))?;
        Ok(Self { session })
    }

    /// A session that touches no network at all.
    ///
    /// For testing `inspect`, which needs a session but not a swarm when the
    /// metadata is already in a local `.torrent`. No DHT, no trackers, no
    /// listener: a unit test must not announce this machine to anybody.
    #[cfg(test)]
    async fn offline(state_dir: &Path, download_dir: PathBuf) -> Result<Self> {
        let options = SessionOptions {
            dht: None,
            disable_trackers: true,
            listen: None,
            persistence: None,
            disable_local_service_discovery: true,
            ..Default::default()
        };
        let _ = state_dir;
        let session = Session::new_with_opts(download_dir, options)
            .await
            .map_err(|error| TorrentError::Session(error.to_string()))?;
        Ok(Self { session })
    }

    /// Reads a torrent's metadata without starting it.
    ///
    /// This exists so the destination can be settled before the first byte is
    /// written, which is the rule everywhere else in this app and the one
    /// thing a torrent makes awkward: a magnet knows neither its name nor
    /// what it contains until the swarm says, and by then a download that
    /// started straight away is already writing somewhere.
    ///
    /// For a `.torrent` or an http link this costs nothing -- the metadata is
    /// the file. For a magnet it is a swarm lookup, which is why the listing
    /// carries the resolved bytes and the peers it met: starting from those
    /// does not look anything up a second time.
    pub async fn inspect(&self, source: &Source) -> Result<Listing> {
        let options = AddTorrentOptions { list_only: true, ..Default::default() };
        let response = self
            .session
            .add_torrent(self.describe(source)?, Some(options))
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))?;

        let AddTorrentResponse::ListOnly(listed) = response else {
            // `list_only` was asked for, so anything else means librqbit
            // already had this torrent and handed back the running one.
            return Err(TorrentError::Refused("that torrent is already running".into()));
        };

        // The largest file decides what the torrent *is*. A single-file
        // torrent is its one file; a release is a video of two gigabytes
        // beside subtitles of forty kilobytes, and calling that collection
        // "subtitles" because of what sorts last would be absurd.
        let principal = listed
            .info
            .iter_file_details()
            .filter(|file| !file.attrs().padding)
            .max_by_key(|file| file.len)
            .and_then(|file| {
                file.filename
                    .to_pathbuf()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });

        Ok(Listing {
            name: listed
                .info
                .name()
                .map(|name| name.into_owned())
                .unwrap_or_else(|| source.provisional_name()),
            total: listed.info.lengths().total_length(),
            file_count: listed.info.iter_file_lengths().count(),
            principal_file: principal,
            bytes: listed.torrent_bytes,
            peers: listed.seen_peers,
        })
    }

    /// How librqbit should be handed this source.
    fn describe(&self, source: &Source) -> Result<AddTorrent<'static>> {
        Ok(match source {
            Source::Magnet(address) | Source::Metadata(address) => {
                AddTorrent::from_url(address.clone())
            }
            Source::File(path) => {
                let bytes = std::fs::read(path).map_err(|error| TorrentError::Unreadable {
                    path: path.display().to_string(),
                    message: error.to_string(),
                })?;
                AddTorrent::from_bytes(bytes)
            }
        })
    }

    /// Starts a torrent already read by [`Torrents::inspect`].
    ///
    /// From the listing's own bytes and peers, so nothing is looked up twice
    /// and the swarm is not asked for metadata it has already given.
    pub async fn start_listed(&self, listing: &Listing, output_dir: &Path) -> Result<Torrent> {
        self.add_bytes(
            AddTorrent::from_bytes(listing.bytes.clone()),
            output_dir,
            Some(listing.peers.clone()),
        )
        .await
    }

    /// Adds a torrent and starts it.
    pub async fn add(&self, source: &Source, output_dir: &Path) -> Result<Torrent> {
        let add = self.describe(source)?;
        self.add_bytes(add, output_dir, None).await
    }

    async fn add_bytes(
        &self,
        add: AddTorrent<'_>,
        output_dir: &Path,
        initial_peers: Option<Vec<std::net::SocketAddr>>,
    ) -> Result<Torrent> {
        let options = AddTorrentOptions {
            initial_peers,
            output_folder: Some(output_dir.display().to_string()),
            // Required to resume at all: librqbit will not write into files
            // that already exist without it, and a half-finished torrent is
            // nothing but files that already exist. The risk it names -- that
            // this writes on top of what is there -- is the same risk any
            // resume carries, and every piece is hash-checked before it
            // counts, so a file that does not belong to this torrent is
            // detected rather than quietly trusted.
            overwrite: true,
            ..Default::default()
        };

        let handle = self
            .session
            .add_torrent(add, Some(options))
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))?
            .into_handle()
            .ok_or_else(|| TorrentError::Refused("the torrent was listed, not started".into()))?;

        Ok(Torrent { session: Arc::clone(&self.session), handle })
    }

    /// Stops the session, giving librqbit the chance to write its resume data.
    ///
    /// Without this the next start re-checks every piece of every unfinished
    /// torrent, because `fastresume` has nothing to read.
    pub async fn stop(&self) {
        self.session.stop().await;
    }
}

/// Maps the app's one speed limit onto librqbit's.
///
/// Download only. The upload side is left alone on purpose: BitTorrent's
/// choking algorithm is reciprocal, so throttling what this hands out
/// throttles what the swarm hands back, and a limit meant to keep a
/// connection usable would instead make the download slow.
fn limits(limit_bps: u64) -> librqbit::limits::LimitsConfig {
    librqbit::limits::LimitsConfig {
        download_bps: NonZeroU32::new(limit_bps.try_into().unwrap_or(u32::MAX)),
        upload_bps: None,
    }
}

/// One running torrent.
pub struct Torrent {
    session: Arc<Session>,
    handle: Arc<ManagedTorrent>,
}

impl Torrent {
    /// The torrent's own name, once it is known.
    ///
    /// For a `.torrent` that is immediate. For a magnet it is `None` until
    /// the metadata arrives from the swarm, which is why the caller has to be
    /// able to rename a job that is already on screen.
    pub fn name(&self) -> Option<String> {
        self.handle.name()
    }

    /// Where the files are going. Known from the start, unlike the name.
    pub fn output_folder(&self) -> PathBuf {
        self.handle.output_folder().to_owned()
    }

    pub fn progress(&self) -> Progress {
        translate(self.handle.stats())
    }

    pub async fn pause(&self) -> Result<()> {
        self.session
            .pause(&self.handle)
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))
    }

    pub async fn resume(&self) -> Result<()> {
        self.session
            .unpause(&self.handle)
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))
    }

    /// Removes the torrent from the session, optionally with its files.
    pub async fn remove(&self, delete_files: bool) -> Result<()> {
        self.session
            .delete(self.handle.id().into(), delete_files)
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))
    }

    /// Resolves once every piece is on disk and verified.
    pub async fn wait_until_done(&self) -> Result<()> {
        self.handle
            .wait_until_completed()
            .await
            .map_err(|error| TorrentError::Refused(error.to_string()))
    }

    /// Stops taking part in the swarm, keeping the files.
    ///
    /// Called at completion. This is a download manager: a finished download
    /// is finished, and seeding indefinitely is not something to start doing
    /// on someone's connection without being asked. Uploading *during* the
    /// download is another matter and stays on -- see `limits`.
    pub async fn stop_seeding(&self) {
        if let Err(error) = self.pause().await {
            tracing::warn!(%error, "could not stop seeding a finished torrent");
        }
    }
}

/// librqbit's statistics in the shape the rest of the app speaks.
///
/// Split out and taking the stats by value so the mapping can be tested
/// without a swarm, a disk or a session: the states are where this gets
/// quietly wrong, not the arithmetic.
fn translate(stats: librqbit::TorrentStats) -> Progress {
    let live = stats.live.as_ref();
    let phase = if stats.finished {
        // Checked before the state, because librqbit reports a finished
        // torrent as `Live` while it seeds and as `Paused` once stopped --
        // neither of which is what someone wants to read about a download
        // that is done.
        Phase::Finished
    } else {
        match &stats.state {
            TorrentStatsState::Initializing { .. } => Phase::Preparing,
            TorrentStatsState::Live => Phase::Running,
            TorrentStatsState::Paused => Phase::Paused,
            TorrentStatsState::Error => {
                Phase::Failed(stats.error.clone().unwrap_or_else(|| "the torrent failed".into()))
            }
        }
    };

    Progress {
        phase,
        downloaded: stats.progress_bytes,
        // Zero means "not known yet" rather than "an empty torrent": a magnet
        // has no idea how large it is until the metadata lands, and a bar
        // drawn against zero would read as complete.
        total: (stats.total_bytes > 0).then_some(stats.total_bytes),
        bytes_per_sec: live.map_or(0.0, |live| live.download_speed.as_bytes() as f64),
        uploaded: stats.uploaded_bytes,
        peers: live.map_or(0, |live| live.snapshot.peer_stats.live),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::TorrentStats;

    fn stats(state: TorrentStatsState, finished: bool) -> TorrentStats {
        TorrentStats {
            state,
            file_progress: Vec::new(),
            error: None,
            progress_bytes: 512,
            uploaded_bytes: 64,
            total_bytes: 2048,
            finished,
            live: None,
        }
    }

    /// A real multi-file torrent, made here, read back offline.
    ///
    /// `inspect` cannot be tested by constructing a `Listing` -- the part
    /// worth testing is what it reads *out* of real metadata. librqbit can
    /// make a `.torrent` from a folder, so this needs no swarm.
    async fn listing_of(files: &[(&str, usize)]) -> (tempfile::TempDir, Listing) {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("Some.Release.2024");
        std::fs::create_dir_all(&content).unwrap();
        for (name, size) in files {
            std::fs::write(content.join(name), vec![b'x'; *size]).unwrap();
        }

        let made = librqbit::create_torrent(
            &content,
            librqbit::CreateTorrentOptions::default(),
            &librqbit::spawn_utils::BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let path = dir.path().join("made.torrent");
        std::fs::write(&path, made.as_bytes().unwrap()).unwrap();

        let session =
            Torrents::offline(dir.path(), dir.path().join("out")).await.unwrap();
        let source = Source::parse(path.to_str().unwrap()).unwrap();
        let listing = session.inspect(&source).await.unwrap();
        session.stop().await;
        (dir, listing)
    }

    #[tokio::test]
    async fn the_largest_file_is_what_a_torrent_is_judged_by() {
        // The question this answers: which folder does a release belong in?
        // Its own name is `Some.Release.2024`, a folder name with no
        // extension, so that says nothing. The smallest file is subtitles.
        // Sorting by either would be wrong; the film decides.
        let (_dir, listing) = listing_of(&[
            ("Some.Release.2024.srt", 2_000),
            ("Some.Release.2024.mkv", 400_000),
            ("readme.nfo", 500),
        ])
        .await;

        assert_eq!(listing.name, "Some.Release.2024");
        assert_eq!(listing.file_count, 3);
        assert_eq!(listing.principal_file.as_deref(), Some("Some.Release.2024.mkv"));
        assert_eq!(listing.total, 402_500);
    }

    #[tokio::test]
    async fn a_single_file_torrent_is_judged_by_that_file() {
        let (_dir, listing) = listing_of(&[("ubuntu-24.04.iso", 50_000)]).await;
        assert_eq!(listing.file_count, 1);
        assert_eq!(listing.principal_file.as_deref(), Some("ubuntu-24.04.iso"));
    }

    #[test]
    fn fetching_metadata_and_checking_files_are_one_waiting_state() {
        assert_eq!(
            translate(stats(TorrentStatsState::Initializing { paused: false }, false)).phase,
            Phase::Preparing
        );
    }

    #[test]
    fn a_finished_torrent_reads_as_finished_whatever_the_swarm_state_says() {
        // The trap this guards: librqbit keeps a completed torrent `Live`
        // while it seeds, and moves it to `Paused` once stopped. Reading the
        // state alone would show a finished download as running, and then as
        // paused, which invites someone to press resume on it.
        for state in [TorrentStatsState::Live, TorrentStatsState::Paused] {
            assert_eq!(translate(stats(state, true)).phase, Phase::Finished);
        }
    }

    #[test]
    fn a_torrent_that_is_merely_paused_is_not_mistaken_for_finished() {
        assert_eq!(translate(stats(TorrentStatsState::Paused, false)).phase, Phase::Paused);
    }

    #[test]
    fn a_failure_carries_the_reason_it_gave() {
        let mut failing = stats(TorrentStatsState::Error, false);
        failing.error = Some("no peers would talk to us".into());
        assert_eq!(
            translate(failing).phase,
            Phase::Failed("no peers would talk to us".into())
        );
    }

    #[test]
    fn a_failure_without_a_reason_still_says_something() {
        let Phase::Failed(reason) = translate(stats(TorrentStatsState::Error, false)).phase else {
            panic!("should have failed");
        };
        assert!(!reason.is_empty());
    }

    #[test]
    fn an_unknown_size_is_absent_rather_than_zero() {
        // A magnet knows nothing about its own size until the metadata
        // arrives. Reported as 0 it would divide into a full progress bar.
        let mut unknown = stats(TorrentStatsState::Initializing { paused: false }, false);
        unknown.total_bytes = 0;
        assert_eq!(translate(unknown).total, None);
    }

    #[test]
    fn with_no_live_peers_there_is_no_speed_and_no_peer_count() {
        let idle = translate(stats(TorrentStatsState::Paused, false));
        assert_eq!(idle.bytes_per_sec, 0.0);
        assert_eq!(idle.peers, 0);
    }

    #[test]
    fn progress_is_the_verified_bytes_and_the_upload_is_kept_separate() {
        let running = translate(stats(TorrentStatsState::Live, false));
        assert_eq!(running.downloaded, 512);
        assert_eq!(running.total, Some(2048));
        assert_eq!(running.uploaded, 64);
    }

    #[test]
    fn no_speed_limit_means_no_limit_rather_than_a_limit_of_zero() {
        // `NonZeroU32::new(0)` is `None`, which is librqbit's "unlimited" --
        // the right answer, but by coincidence, so it is worth pinning.
        assert_eq!(limits(0).download_bps, None);
        assert_eq!(limits(1 << 20).download_bps, NonZeroU32::new(1 << 20));
        // The app's limit is a u64 and librqbit's is a u32: a limit larger
        // than 4 GB/s saturates rather than wrapping to something tiny.
        assert_eq!(limits(u64::MAX).download_bps, NonZeroU32::new(u32::MAX));
    }

    #[test]
    fn the_upload_side_is_never_limited() {
        for limit in [0, 1 << 10, u64::MAX] {
            assert_eq!(limits(limit).upload_bps, None);
        }
    }
}
