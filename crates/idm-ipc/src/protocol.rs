//! The wire protocol between the engine and its clients.
//!
//! Three things speak this: the CLI, the Tauri app, and the browser's native
//! messaging host. The host is the one that matters for security, because
//! everything it forwards originated in a web page.
//!
//! # Untrusted by construction
//!
//! A download request names a *file*, never a *directory*. The engine decides
//! where downloads go, from its own configuration; the request may only
//! suggest a leaf name, and even that is sanitised. This is deliberate: if the
//! protocol carried a destination path, then a compromised or malicious page
//! that reached the extension could write anywhere the user can — and
//! "validate the path carefully" is a weaker guarantee than "there is no path
//! to validate".
//!
//! # Framing
//!
//! Length-prefixed JSON: a little-endian `u32` byte count, then that many
//! bytes of UTF-8 JSON. The same framing Chrome uses for native messaging, so
//! the host can shuttle between the two without reframing.

use serde::{Deserialize, Serialize};

/// Refuse anything larger than this before allocating for it, so a bad length
/// prefix cannot be turned into a memory exhaustion.
pub const MAX_FRAME_BYTES: u32 = 1 << 20;

/// Chrome's own cap on a message from an extension.
pub const MAX_NATIVE_MESSAGE_BYTES: u32 = 64 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("frame of {size} bytes exceeds the {limit} byte limit")]
    FrameTooLarge { size: u32, limit: u32 },
    #[error("connection closed before a complete frame arrived")]
    Truncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job{}", self.0)
    }
}

/// What a client asks a download to be.
///
/// Note the absence of any destination directory: see the module docs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Other addresses serving the same file, fetched from at the same time.
    ///
    /// Each is checked against the first before it is allowed to contribute
    /// a byte, and one that disagrees is dropped rather than failing the
    /// download. Only the app and the command line ever set these: a browser
    /// extension has no way to know that two addresses are one file, and a
    /// page that could claim it could aim half a download anywhere it liked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    /// A *suggested* leaf name. Sanitised by the engine, and overridden by the
    /// user's own rules; never a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    /// Forwarded from the browser so a cookie-gated download works. Treated as
    /// a secret: never logged, never echoed back in a status response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// What the browser believed the size to be, used only to decide whether
    /// the download is worth taking over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_hint: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// The named queue this download joins, if any.
    ///
    /// Set only by the app. A browser extension has no business naming one:
    /// the ids are opaque row numbers it could only guess at, and a page that
    /// could pick a queue could park a download in a switched-off one so that
    /// it never ran and nobody noticed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<i64>,
}

/// Most addresses one download may be told to fetch from at once.
pub const MAX_MIRRORS: usize = 8;

/// One named queue, as the window sees it.
///
/// Carries the live counts the store cannot know: the store can see what its
/// rows say, but which downloads are actually in flight is the engine's own
/// knowledge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueView {
    pub id: i64,
    pub name: String,
    pub max_concurrent: u32,
    pub enabled: bool,
    /// Downloads of this queue that hold a slot right now.
    pub running: u32,
    /// Downloads of this queue waiting for one.
    pub waiting: u32,
}

impl DownloadRequest {
    /// Rejects anything the engine should not act on.
    ///
    /// Only `http` and `https` are accepted. `file:` would turn the extension
    /// into a local file reader, and `data:`/`blob:` carry their payload in
    /// the URL itself, which has no business going through a download engine.
    /// Checks a request from a browser extension.
    ///
    /// The strict form: http and https only. A page cannot be allowed to
    /// hand over an `ftp://` address — it would carry credentials in the
    /// URL, and it would let a web page reach file servers inside a network
    /// the browser itself could not reach.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with(false)
    }

    /// Checks a request, optionally allowing file-transfer protocols.
    ///
    /// `file_transfer` is true only for the application and the command
    /// line, where the address came from the person using them.
    pub fn validate_with(&self, file_transfer: bool) -> Result<(), String> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err("empty url".into());
        }
        if url.len() > 8192 {
            return Err("url is implausibly long".into());
        }
        let lower = url.to_ascii_lowercase();
        let web = lower.starts_with("http://") || lower.starts_with("https://");
        // A magnet link is how anyone starts a torrent, including by clicking
        // one on a page, so this is not gated the way `ftp` is: it names a
        // swarm by hash, carries no credentials, and reaches nothing a
        // browser could not reach for itself.
        let magnet = lower.starts_with("magnet:");
        // A path to a `.torrent` on this machine, which only the app and the
        // command line may name. A page that could hand one over would have
        // this process open an arbitrary local file of its choosing.
        let torrent_file = file_transfer && is_local_torrent_path(url);
        let transfer = file_transfer
            && (lower.starts_with("ftp://")
                || lower.starts_with("ftps://")
                || lower.starts_with("sftp://"));
        if !(web || magnet || torrent_file || transfer) {
            return Err(if file_transfer {
                "only http, https, magnet, ftp, ftps and sftp addresses, or a .torrent file, are accepted".into()
            } else {
                "only http, https and magnet addresses are accepted".to_owned()
            });
        }
        // Mirrors are http/https only, whatever `file_transfer` says. A
        // mirror contributes byte ranges over HTTP, so there is no sense in
        // which a magnet or an sftp address could be one, and accepting one
        // here would only produce a refusal later with further to fall.
        if !self.mirrors.is_empty() {
            // Capped because every mirror costs a probe and two sampled
            // windows before the first byte of the download is fetched, so
            // an unbounded list is an unbounded wait with nothing on screen.
            if self.mirrors.len() > MAX_MIRRORS {
                return Err(format!("at most {MAX_MIRRORS} mirrors"));
            }
            for mirror in &self.mirrors {
                let mirror = mirror.trim();
                if mirror.is_empty() || mirror.len() > 8192 {
                    return Err("a mirror address is empty or implausibly long".into());
                }
                let lower = mirror.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    return Err("a mirror must be an http or https address".into());
                }
            }
        }

        if let Some(queue) = self.queue {
            // `file_transfer` is the app-or-browser distinction this function
            // already turns on twice above, used here for the third time
            // rather than taking a second flag that would always agree with
            // it. A browser naming a queue is refused rather than ignored:
            // ignoring it would let a page believe it had parked a download
            // somewhere it had not.
            if !file_transfer {
                return Err("a browser cannot choose a queue".into());
            }
            if queue <= 0 {
                return Err("that is not a queue".into());
            }
        }

        if let Some(name) = &self.filename {
            if name.len() > 512 {
                return Err("filename is implausibly long".into());
            }
            // The engine sanitises too; rejecting here makes a hostile name
            // visible as an error rather than silently rewritten.
            if name.contains(['/', '\\', '\0']) {
                return Err("filename must be a leaf name, not a path".into());
            }
        }
        Ok(())
    }
}

/// Whether this is a path to a `.torrent` on this machine.
///
/// Checked as a path rather than by extension alone, because an extension
/// test on its own lets a scheme through: `file:///C:/x.torrent` and even
/// `javascript:x.torrent` end in `.torrent` and are emphatically not local
/// paths. So anything carrying a scheme is refused here and left to the
/// scheme checks, and a lone `X:` is read as a drive letter.
fn is_local_torrent_path(url: &str) -> bool {
    if url.contains("://") {
        return false;
    }
    // A scheme is a name followed by a colon. One letter before it is a
    // Windows drive, which is the one case that is a path and not a scheme.
    if let Some((head, _)) = url.split_once(':')
        && head.chars().count() != 1
    {
        return false;
    }
    std::path::Path::new(url)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

/// A browser extension the user has approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedClient {
    /// Random per-installation id the extension generates once and keeps.
    pub client_id: String,
    /// Something the user will recognise, e.g. "Chrome on this PC".
    pub label: String,
    pub paired_at_unix: u64,
    /// Whether this browser may hand downloads over right now.
    ///
    /// Switching a browser off keeps its pairing, so switching it back on
    /// needs no new approval. That is the point of having it instead of
    /// revoking: revoking is a decision about trust, and "not today" is not
    /// that. Defaults to on so a pairing written before this existed keeps
    /// working.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// A pairing request waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub request_id: String,
    pub client_id: String,
    pub label: String,
    pub requested_at_unix: u64,
}

/// What to do once the queue drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostActionSetting {
    #[default]
    Nothing,
    ExitApp,
    Sleep,
    Hibernate,
    Shutdown,
}

/// Where the engine's own requests go on their way out.
///
/// Three choices rather than an optional address, because "no proxy" and
/// "whatever the machine is set up for" are genuinely different answers and
/// the second is what happens by default: the HTTP client reads `HTTP_PROXY`
/// and friends from the environment unless told not to. Without a `Direct`
/// to pick, a user on a machine with those set would have no way to ask for
/// a plain connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxySetting {
    /// Whatever the environment says.
    #[default]
    System,
    /// Straight out, whatever the environment says.
    Direct,
    /// Through [`Settings::proxy_address`].
    Manual,
}

/// Settings a client may read, and the desktop app may change.
///
/// `serde(default)` on the whole struct, so a field missing from the file is
/// filled from [`Settings::default`] rather than failing the parse. Without
/// it, every field added after a settings file was written made that file
/// unreadable — and the loader treats unreadable as "use the defaults", so
/// the user's own choices were silently thrown away on every start. That
/// included browser integration switching itself back on after being
/// turned off. Putting it on the struct rather than on each field means a
/// field added later cannot reintroduce the problem by being forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Master switch for browser integration.
    ///
    /// Turning this off makes the engine refuse downloads handed over by the
    /// browser, so the browser keeps them itself. It is a convenience control,
    /// not a security boundary: the boundary is the pipe's DACL. Its value is
    /// that it can be flipped from the app, without hunting through browser
    /// settings, and it is enforced by the engine rather than only by the
    /// extension -- an extension-only switch would be cosmetic.
    pub browser_integration: bool,
    pub post_action: PostActionSetting,
    pub post_action_countdown_secs: u64,
    /// Downloads smaller than this are left to the browser.
    pub min_takeover_bytes: u64,
    /// Pick up interrupted downloads when the engine starts.
    ///
    /// The bytes and the resume journal outlive any crash, so this costs
    /// nothing but the decision to act on them. A download that needed
    /// browser cookies is left alone whatever this says: the cookies were
    /// never stored, so restarting it would only produce a refusal the user
    /// has no way to account for.
    pub resume_on_start: bool,
    /// Start DivinePulse when this user signs in.
    pub start_with_windows: bool,
    /// Sort finished downloads into Video, Audio, Documents and so on.
    ///
    /// The folder names are a fixed list in the engine, never taken from a
    /// server or a URL, so they cannot be made to point outside the
    /// download folder.
    pub sort_into_folders: bool,
    /// A ceiling on the total download rate, in bytes per second; zero for
    /// none.
    ///
    /// Shared by every download rather than applied to each: a limit exists
    /// so that downloading does not take over the connection, and four
    /// downloads at a per-download 5 MB/s would be 20.
    pub speed_limit_bytes_per_sec: u64,
    /// How many downloads may run at once. The rest wait their turn.
    ///
    /// This is about the origin and the link, not the disk: every running
    /// download opens its own fan of connections, so ten at once is not ten
    /// times faster, it is the same bandwidth cut ten ways with ten times
    /// the chance of tripping a rate limit. The scheduler already decides
    /// how many *connections* a single download deserves; this decides how
    /// many downloads get to make that decision simultaneously.
    pub max_concurrent_downloads: u32,
    /// Where new downloads go; empty means the user's Downloads folder.
    ///
    /// Empty rather than a resolved path so the default keeps following the
    /// account: a settings file copied to another machine or profile should
    /// not carry a hardcoded `C:\Users\someone\Downloads` that does not
    /// exist there.
    ///
    /// Only the app and the CLI can set it -- the engine refuses settings
    /// changes from the browser side. That matters more here than for any
    /// other setting: a page that could choose the folder could choose the
    /// Startup folder.
    ///
    /// Applies to downloads started afterwards. One already queued or
    /// running keeps the folder it was given, because its part file and
    /// journal are already there and moving them mid-transfer would only
    /// lose them.
    pub download_dir: String,
    /// Browsers the user has removed from the list in the window.
    ///
    /// Only hidden from view: a hidden browser is still registered, still
    /// pairable, and listed again the moment it is restored. Stored by name
    /// rather than as a flag on each row because most of these browsers are
    /// not paired with anything, so there is no row of ours to hang it on.
    pub hidden_browsers: Vec<String>,
    pub proxy: ProxySetting,
    /// The proxy to use when [`Settings::proxy`] is `Manual`.
    ///
    /// A whole address rather than a host and a port, because the scheme is
    /// what picks between an HTTP proxy and a SOCKS one:
    /// `http://host:3128`, `socks5://host:1080`, or `socks5h://host:1080` to
    /// have the proxy resolve names too.
    ///
    /// Credentials are deliberately not accepted here. This string is kept
    /// in `settings.json` in the clear, and a proxy password belongs in
    /// Windows Credential Manager -- so an address carrying one is refused
    /// rather than quietly written to a file the user does not know is
    /// holding it. Authenticated proxies wait for that to be wired up.
    pub proxy_address: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            browser_integration: true,
            // Never shut a machine down unless asked.
            post_action: PostActionSetting::Nothing,
            post_action_countdown_secs: 60,
            min_takeover_bytes: 2 << 20,
            // A download manager that forgets what it was doing when the
            // machine rebooted is the thing people complain about, and the
            // machinery to avoid it is already paid for.
            resume_on_start: true,
            // Never without being asked: adding yourself to someone's
            // startup is theirs to decide.
            start_with_windows: false,
            // Enough that a handful of files from a browser all start, few
            // enough that dropping a hundred-link batch in does not open a
            // hundred fans of connections at once.
            max_concurrent_downloads: 4,
            // On, as the download managers people are used to do it.
            // A file whose type is not recognised stays where it would have
            // gone anyway, so nothing goes missing because of this.
            sort_into_folders: true,
            speed_limit_bytes_per_sec: 0,
            download_dir: String::new(),
            hidden_browsers: Vec::new(),
            // Not Direct: honouring the machine's own proxy configuration is
            // what every other program on it does, and a download manager
            // that quietly ignored it would look broken on exactly the
            // networks where a proxy is mandatory.
            proxy: ProxySetting::System,
            proxy_address: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// First message on every connection. The token proves the client could
    /// read a file only this user can read.
    ///
    /// `client_id` is a stable per-installation identifier used for pairing.
    /// It is not a credential: it says *which* extension is asking, so the
    /// user can approve or revoke it. The engine never treats it as proof of
    /// anything by itself.
    Hello {
        token: String,
        client: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    Enqueue(DownloadRequest),
    /// All jobs, or one.
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<JobId>,
    },
    Cancel {
        id: JobId,
    },
    /// Starts a paused or interrupted download again.
    Resume {
        id: JobId,
    },
    /// Starts a failed download again from its History row, after the
    /// engine itself no longer remembers it.
    ///
    /// `Resume` only ever finds a job in `Failed` while the process that
    /// failed it is still running: nothing failed survives a restart into
    /// the in-memory job list, because only unfinished jobs are restored.
    /// The bytes and journal on disk outlive that restart just as well as a
    /// paused download's do, so History needs its own way back to them,
    /// keyed by the store row rather than a session-local job id.
    ResumeFromHistory {
        row_id: i64,
    },
    /// Stops a running download, keeping its bytes so it can go on later.
    Pause {
        id: JobId,
    },
    /// Points an existing download at a new address, keeping what it has.
    ///
    /// For a link that has expired, which is the commonest reason a resume
    /// cannot proceed: the bytes on disk are still good, only the way back
    /// to the rest of them has gone stale.
    RefreshUrl {
        id: JobId,
        url: String,
    },
    /// Asks the user to approve this client. Never self-approving: the reply
    /// is only ever "pending" until a human says otherwise in the app.
    RequestPairing {
        label: String,
    },
    /// Listing, approving, denying and revoking are all refused for browser
    /// clients, so a browser can neither pair itself nor see what else is
    /// paired.
    ListPairings,
    ApprovePairing {
        request_id: String,
    },
    DenyPairing {
        request_id: String,
    },
    RevokePairing {
        client_id: String,
    },
    /// Switches a paired browser on or off without unpairing it.
    ///
    /// Refused for browser clients, like every other change to who may do
    /// what: an extension that could switch itself back on would make the
    /// switch decoration.
    SetBrowserEnabled {
        client_id: String,
        enabled: bool,
    },
    GetSettings,
    SetSettings(Settings),
    /// The named queues, with what each is running right now.
    ///
    /// Queue management is refused for browser clients throughout, for the
    /// same reason pairing is: a queue decides what runs and when, and an
    /// extension that could switch one off could stop every download in it
    /// without anything in the window saying so.
    ListQueues,
    /// Creates a queue, or changes the concurrency of one with this name.
    SetQueue {
        name: String,
        max_concurrent: u32,
    },
    /// Switches a queue on or off. Off stops it starting anything more; it
    /// does not stop what is already running.
    EnableQueue {
        id: i64,
        enabled: bool,
    },
    /// Removes a queue. Its downloads are kept and become unqueued.
    RemoveQueue {
        id: i64,
    },
    /// Moves a download into a queue, or out of one with `None`.
    AssignQueue {
        id: JobId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue: Option<i64>,
    },
    /// Cancels an armed post-download action during its countdown.
    CancelPostAction,
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    /// The connection went away and the engine is waiting to try again.
    ///
    /// Distinct from `Paused`, which the user chose, and from `Failed`,
    /// which is final. The bytes and the journal are intact and the engine
    /// will pick it up again by itself, so the window can say so instead of
    /// showing a failure the user is expected to act on.
    Reconnecting,
    Completed,
    Failed,
    /// Stopped because the user said so.
    ///
    /// Distinct from `Failed` because nothing went wrong, and the window
    /// should not offer to diagnose a decision. It was a `Failed` job whose
    /// error read "cancelled" until this existed, which meant every reader
    /// had to compare that word against a field the server also writes to.
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: JobId,
    pub url: String,
    pub filename: String,
    /// Where the file is, or is going -- so a client can reveal it once the
    /// job completes without a second round trip to ask.
    pub output_dir: String,
    pub state: JobState,
    pub downloaded: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
    pub connections: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Seconds until the next reconnection attempt, while `Reconnecting`.
    ///
    /// Shown so a wait looks like a wait rather than a stall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_secs: Option<u64>,
    /// The queue holding this download, if any.
    ///
    /// Both the id and the name, because the window needs each: the id to
    /// show which entry of the queue picker is the current one, the name to
    /// put on the job without a second round trip to look it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Welcome {
        version: String,
    },
    Accepted {
        id: JobId,
    },
    /// The request was understood and declined, with a reason fit to show a
    /// user. Distinct from `Error`, which means the request was malformed.
    Rejected {
        reason: String,
    },
    Status {
        jobs: Vec<JobStatus>,
    },
    /// Recorded; a human must now approve it in the app.
    PairingPending {
        request_id: String,
    },
    PairingApproved,
    Pairings {
        paired: Vec<PairedClient>,
        pending: Vec<PendingPairing>,
    },
    Settings(Settings),
    Queues {
        queues: Vec<QueueView>,
    },
    Pong,
    Error {
        message: String,
    },
}

/// Encodes a value as a length-prefixed JSON frame.
pub fn encode<T: Serialize>(value: &T, limit: u32) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(value)?;
    let size = u32::try_from(body.len())
        .map_err(|_| ProtocolError::FrameTooLarge { size: u32::MAX, limit })?;
    if size > limit {
        return Err(ProtocolError::FrameTooLarge { size, limit });
    }
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&size.to_le_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

/// Reads one length-prefixed JSON frame.
///
/// The length is checked against `limit` *before* allocating, so a hostile or
/// corrupt prefix cannot be turned into a huge allocation.
pub async fn read_frame<R, T>(reader: &mut R, limit: u32) -> Result<T, ProtocolError>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    use tokio::io::AsyncReadExt;

    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ProtocolError::Truncated);
        }
        Err(error) => return Err(error.into()),
    }

    let size = u32::from_le_bytes(header);
    if size > limit {
        return Err(ProtocolError::FrameTooLarge { size, limit });
    }
    let mut body = vec![0u8; size as usize];
    reader.read_exact(&mut body).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            error.into()
        }
    })?;
    Ok(serde_json::from_slice(&body)?)
}

/// Writes one length-prefixed JSON frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T, limit: u32) -> Result<(), ProtocolError>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    use tokio::io::AsyncWriteExt;
    let framed = encode(value, limit)?;
    writer.write_all(&framed).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest { url: url.into(), ..Default::default() }
    }

    fn with_mirrors(mirrors: &[&str]) -> DownloadRequest {
        DownloadRequest {
            url: "https://example.invalid/a.bin".into(),
            mirrors: mirrors.iter().map(|m| (*m).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn http_mirrors_are_accepted() {
        assert!(
            with_mirrors(&["https://b.invalid/a.bin", "http://c.invalid/a.bin"]).validate().is_ok()
        );
    }

    #[test]
    fn a_mirror_must_be_an_http_address_even_from_the_command_line() {
        // Unlike the main address, which may be a magnet or an sftp URL.
        // A mirror contributes byte ranges over HTTP, so there is no sense
        // in which either of those could be one.
        for wrong in ["magnet:?xt=urn:btih:abc", "sftp://b.invalid/a.bin", "file:///C:/a.bin", ""] {
            let request = with_mirrors(&[wrong]);
            assert!(request.validate().is_err(), "{wrong} should be refused");
            assert!(
                request.validate_with(true).is_err(),
                "{wrong} should be refused from the command line too"
            );
        }
    }

    #[test]
    fn there_is_a_ceiling_on_how_many_mirrors_may_be_given() {
        // Each costs a probe and two sampled windows before the download
        // starts, so an unbounded list is an unbounded wait.
        let many: Vec<String> =
            (0..=MAX_MIRRORS).map(|n| format!("https://m{n}.invalid/a.bin")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(with_mirrors(&refs[..MAX_MIRRORS]).validate().is_ok());
        assert!(with_mirrors(&refs).validate().is_err());
    }

    #[test]
    fn only_http_urls_are_accepted() {
        assert!(request("https://example.invalid/a.bin").validate().is_ok());
        assert!(request("http://example.invalid/a.bin").validate().is_ok());

        // A download engine has no business reading local files or decoding
        // payloads carried in the URL.
        for hostile in [
            "file:///C:/Windows/System32/config/SAM",
            "data:text/plain;base64,QUJD",
            "blob:https://example.invalid/abc",
            "ftp://example.invalid/a.bin",
            "javascript:alert(1)",
            "",
        ] {
            assert!(request(hostile).validate().is_err(), "{hostile} should be refused");
        }
    }

    #[test]
    fn the_scheme_check_is_case_insensitive() {
        // Otherwise "FILE://" walks straight past it.
        assert!(request("HTTPS://example.invalid/a.bin").validate().is_ok());
    }

    #[test]
    fn settings_written_by_an_older_build_keep_their_values() {
        // Exactly the shape found on a real machine: four fields, written
        // before the rest existed. Values that differ from the defaults, so
        // the test can tell kept from reset — browser integration off is the
        // one that matters, since silently switching it back on undoes the
        // user's kill switch.
        let old = r#"{
            "browser_integration": false,
            "post_action": "shutdown",
            "post_action_countdown_secs": 30,
            "min_takeover_bytes": 1048576
        }"#;
        let settings: Settings = serde_json::from_str(old).expect("an older file must still load");

        assert!(!settings.browser_integration, "a switched-off integration must stay off");
        assert_eq!(settings.post_action, PostActionSetting::Shutdown);
        assert_eq!(settings.post_action_countdown_secs, 30);
        assert_eq!(settings.min_takeover_bytes, 1 << 20);

        // And the fields it never had take their defaults.
        let defaults = Settings::default();
        assert_eq!(settings.max_concurrent_downloads, defaults.max_concurrent_downloads);
        assert_eq!(settings.resume_on_start, defaults.resume_on_start);
        assert_eq!(settings.speed_limit_bytes_per_sec, 0);
    }

    #[test]
    fn an_empty_settings_object_is_the_defaults() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn a_field_from_a_newer_build_is_ignored_rather_than_fatal() {
        // A downgrade should keep what it understands.
        let newer = r#"{"browser_integration": false, "some_future_setting": 7}"#;
        let settings: Settings = serde_json::from_str(newer).unwrap();
        assert!(!settings.browser_integration);
    }

    #[test]
    fn a_browser_may_not_hand_over_a_file_transfer_address() {
        // It would carry credentials in the URL, and let a web page reach
        // servers inside a network the browser itself could not.
        for url in [
            "ftp://user:pass@fileserver.internal/secrets.zip",
            "ftps://fileserver.internal/a.bin",
            "sftp://root@10.0.0.1/etc/shadow",
        ] {
            assert!(request(url).validate().is_err(), "{url} should be refused from a browser");
            assert!(
                request(url).validate_with(true).is_ok(),
                "{url} should be allowed from the app"
            );
        }
    }

    #[test]
    fn a_magnet_link_is_accepted_from_anywhere() {
        // Clicking a magnet on a page is how torrents normally start. It
        // names a swarm by hash, carries no credentials, and reaches nothing
        // the browser could not reach itself, so it is not gated the way an
        // ftp address is.
        let link = "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056";
        assert!(request(link).validate().is_ok());
        assert!(request(link).validate_with(true).is_ok());
        assert!(request("MAGNET:?xt=urn:btih:abc").validate().is_ok());
    }

    #[test]
    fn only_the_app_may_name_a_torrent_file_on_this_machine() {
        // From a browser this would be a page choosing which local file this
        // process opens.
        for path in [r"D:\downloads\ubuntu.torrent", "ubuntu.torrent", r"\nas\share\a.torrent"] {
            assert!(request(path).validate().is_err(), "{path} should be refused from a browser");
            assert!(
                request(path).validate_with(true).is_ok(),
                "{path} should be allowed from the app"
            );
        }
    }

    #[test]
    fn a_torrent_extension_does_not_smuggle_a_scheme_past_the_check() {
        // The trap: testing the extension alone accepts anything at all that
        // happens to end in `.torrent`, scheme and all.
        for hostile in [
            "file:///C:/Windows/System32/config/SAM.torrent",
            "javascript:alert(1)/x.torrent",
            "data:application/x-bittorrent;base64,ZDg6.torrent",
            "blob:https://example.invalid/a.torrent",
        ] {
            assert!(
                request(hostile).validate_with(true).is_err(),
                "{hostile} should be refused even from the app"
            );
        }
    }

    #[test]
    fn allowing_file_transfer_does_not_allow_everything_else() {
        for url in [
            "file:///C:/Windows/System32/config/SAM",
            "data:text/plain;base64,AAAA",
            "javascript:alert(1)",
            "\\\\server\\share\\file",
        ] {
            assert!(request(url).validate_with(true).is_err(), "{url} should still be refused");
        }
        assert!(request("FILE:///C:/secret").validate().is_err());
        assert!(request("JavaScript:alert(1)").validate().is_err());
    }

    #[test]
    fn a_filename_must_be_a_leaf_not_a_path() {
        let mut req = request("https://example.invalid/a.bin");
        req.filename = Some("report.pdf".into());
        assert!(req.validate().is_ok());

        for hostile in ["../../evil.exe", r"..\..\evil.exe", "/etc/passwd", r"C:\Windows\x.dll"] {
            req.filename = Some(hostile.into());
            assert!(req.validate().is_err(), "{hostile} should be refused");
        }
    }

    #[test]
    fn the_protocol_carries_no_destination_directory() {
        // A structural guarantee rather than a validation rule: if the request
        // cannot express a directory, no amount of cunning in a web page can
        // make the engine write to one.
        let json = serde_json::to_string(&request("https://example.invalid/a.bin")).unwrap();
        for forbidden in ["dir", "path", "directory", "output", "dest"] {
            assert!(!json.contains(forbidden), "request should not carry {forbidden}: {json}");
        }
    }

    #[test]
    fn absurd_lengths_are_refused() {
        let mut req = request(&format!("https://example.invalid/{}", "a".repeat(9000)));
        assert!(req.validate().is_err());
        req = request("https://example.invalid/a.bin");
        req.filename = Some("x".repeat(600));
        assert!(req.validate().is_err());
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let original = Request::Enqueue(request("https://example.invalid/a.bin"));
        let framed = encode(&original, MAX_FRAME_BYTES).unwrap();

        let mut cursor = std::io::Cursor::new(framed);
        let decoded: Request = read_frame(&mut cursor, MAX_FRAME_BYTES).await.unwrap();
        match decoded {
            Request::Enqueue(req) => assert_eq!(req.url, "https://example.invalid/a.bin"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_refused_before_allocating() {
        // A hostile prefix claiming 4 GiB must not become a 4 GiB allocation.
        let mut framed = u32::MAX.to_le_bytes().to_vec();
        framed.extend_from_slice(b"{}");
        let mut cursor = std::io::Cursor::new(framed);

        let outcome: Result<Request, _> = read_frame(&mut cursor, MAX_FRAME_BYTES).await;
        assert!(matches!(outcome, Err(ProtocolError::FrameTooLarge { .. })), "got {outcome:?}");
    }

    #[tokio::test]
    async fn a_truncated_frame_is_reported_not_hung() {
        let mut framed = 64u32.to_le_bytes().to_vec();
        framed.extend_from_slice(b"only a few bytes");
        let mut cursor = std::io::Cursor::new(framed);

        let outcome: Result<Request, _> = read_frame(&mut cursor, MAX_FRAME_BYTES).await;
        assert!(matches!(outcome, Err(ProtocolError::Truncated)), "got {outcome:?}");
    }

    #[tokio::test]
    async fn malformed_json_is_an_error_not_a_panic() {
        let body = b"{ this is not json }";
        let mut framed = (body.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(framed);

        let outcome: Result<Request, _> = read_frame(&mut cursor, MAX_FRAME_BYTES).await;
        assert!(matches!(outcome, Err(ProtocolError::Malformed(_))), "got {outcome:?}");
    }

    #[test]
    fn encoding_refuses_to_build_an_oversized_frame() {
        let huge = Request::Enqueue(DownloadRequest {
            url: format!("https://example.invalid/{}", "a".repeat(2000)),
            ..Default::default()
        });
        assert!(matches!(encode(&huge, 256), Err(ProtocolError::FrameTooLarge { .. })));
    }

    #[test]
    fn cookies_are_omitted_from_the_wire_when_absent() {
        // They are a secret; the shape should not even hint at them unless set.
        let json = serde_json::to_string(&request("https://example.invalid/a.bin")).unwrap();
        assert!(!json.contains("cookies"), "{json}");
    }
}
