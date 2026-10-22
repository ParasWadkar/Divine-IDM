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
        let transfer = file_transfer
            && (lower.starts_with("ftp://")
                || lower.starts_with("ftps://")
                || lower.starts_with("sftp://"));
        if !(web || transfer) {
            return Err(if file_transfer {
                "only http, https, ftp, ftps and sftp urls are accepted".into()
            } else {
                "only http and https urls are accepted".to_owned()
            });
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

/// A browser extension the user has approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedClient {
    /// Random per-installation id the extension generates once and keeps.
    pub client_id: String,
    /// Something the user will recognise, e.g. "Chrome on this PC".
    pub label: String,
    pub paired_at_unix: u64,
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

/// Settings a client may read, and the desktop app may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Start Divine IDM when this user signs in.
    pub start_with_windows: bool,
    /// How many downloads may run at once. The rest wait their turn.
    ///
    /// This is about the origin and the link, not the disk: every running
    /// download opens its own fan of connections, so ten at once is not ten
    /// times faster, it is the same bandwidth cut ten ways with ten times
    /// the chance of tripping a rate limit. The scheduler already decides
    /// how many *connections* a single download deserves; this decides how
    /// many downloads get to make that decision simultaneously.
    pub max_concurrent_downloads: u32,
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
    GetSettings,
    SetSettings(Settings),
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
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: JobId,
    pub url: String,
    pub filename: String,
    pub state: JobState,
    pub downloaded: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
    pub connections: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
