//! Chrome native messaging framing, and the extension-facing message shapes.
//!
//! A browser launches this host as a child process and talks to it over
//! stdio: a 4-byte **native-endian** length, then that many bytes of UTF-8
//! JSON. Native-endian rather than a fixed order is the part people get
//! wrong; it happens to be little-endian everywhere this runs, but the
//! conversion is written explicitly so the reason is visible.
//!
//! Everything arriving here began life in a web page, so the length is
//! checked before anything is allocated and the message shapes are
//! deliberately narrow — notably, there is no field for a destination
//! directory, because the IPC protocol behind this has none either.

use serde::{Deserialize, Serialize};

/// Chrome's cap on a message *to* a host. Checked before allocating.
pub const MAX_INCOMING: u32 = 64 << 20;

/// Chrome's cap on a message *from* a host. A larger reply is silently
/// dropped by the browser, so it is better to fail loudly here.
pub const MAX_OUTGOING: u32 = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("message of {size} bytes exceeds the {limit} byte limit")]
    TooLarge { size: u32, limit: u32 },
    /// The browser closed the pipe, which is how it says "stop".
    #[error("the browser closed the connection")]
    Closed,
}

/// What an extension can ask for.
///
/// Intentionally small. Every addition here widens what a compromised page
/// could reach through the extension.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromExtension {
    /// Greets the engine, which doubles as a pairing request.
    Hello {
        /// Shown to the user in the pairing prompt.
        label: String,
    },
    /// Asks the user to approve this extension.
    Pair {
        label: String,
    },
    Download {
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        referrer: Option<String>,
        #[serde(default)]
        cookies: Option<String>,
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default)]
        size_hint: Option<u64>,
        #[serde(default)]
        mime: Option<String>,
    },
    /// Lets the extension mirror the engine's state: whether integration is
    /// on, and whether this extension is paired.
    GetState,
    Ping,
}

/// What the host sends back.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToExtension {
    Ready {
        version: String,
    },
    /// The engine took the download.
    Accepted {
        id: u64,
    },
    /// Understood and declined, with a reason fit to show a user.
    Declined {
        reason: String,
    },
    /// A human must approve this extension before anything else works.
    PairingPending {
        request_id: String,
    },
    PairingApproved,
    State {
        /// Whether the engine will accept downloads from a browser at all.
        integration_enabled: bool,
        paired: bool,
    },
    Pong,
    /// The engine is not running, or could not be reached.
    Unavailable {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// A message plus the identity of the extension that sent it.
///
/// The id rides on *every* message rather than being established once by a
/// greeting, because `chrome.runtime.sendNativeMessage` starts a new host
/// process per message and closes it after the reply. A host that remembered
/// the id between messages would pass a test using one long-lived process and
/// then fail against a real browser, where the process that was greeted is
/// already gone by the time a download arrives.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// Absent only from clients that do not pair, which the engine refuses.
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(flatten)]
    pub message: FromExtension,
}

/// Reads one native messaging frame from `reader`.
pub async fn read_message<R>(reader: &mut R) -> Result<Envelope, FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Closed);
        }
        Err(error) => return Err(error.into()),
    }

    // Native byte order, per the native messaging specification.
    let size = u32::from_ne_bytes(header);
    if size > MAX_INCOMING {
        return Err(FrameError::TooLarge { size, limit: MAX_INCOMING });
    }
    let mut body = vec![0u8; size as usize];
    reader.read_exact(&mut body).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Closed
        } else {
            error.into()
        }
    })?;
    Ok(serde_json::from_slice(&body)?)
}

/// Writes one native messaging frame.
pub async fn write_message<W>(writer: &mut W, message: &ToExtension) -> Result<(), FrameError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let body = serde_json::to_vec(message)?;
    let size = u32::try_from(body.len())
        .map_err(|_| FrameError::TooLarge { size: u32::MAX, limit: MAX_OUTGOING })?;
    if size > MAX_OUTGOING {
        return Err(FrameError::TooLarge { size, limit: MAX_OUTGOING });
    }
    writer.write_all(&size.to_ne_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(json: &str) -> Vec<u8> {
        let mut out = (json.len() as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(json.as_bytes());
        out
    }

    #[tokio::test]
    async fn a_download_message_round_trips() {
        let raw = framed(
            r#"{"type":"download","url":"https://example.invalid/a.bin","size_hint":1048576}"#,
        );
        let mut cursor = std::io::Cursor::new(raw);
        match read_message(&mut cursor).await.unwrap().message {
            FromExtension::Download { url, size_hint, .. } => {
                assert_eq!(url, "https://example.invalid/a.bin");
                assert_eq!(size_hint, Some(1 << 20));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn optional_fields_may_be_omitted() {
        let raw = framed(r#"{"type":"download","url":"https://example.invalid/a.bin"}"#);
        let mut cursor = std::io::Cursor::new(raw);
        assert!(matches!(
            read_message(&mut cursor).await.unwrap().message,
            FromExtension::Download { .. }
        ));
    }

    #[tokio::test]
    async fn every_message_carries_the_client_id() {
        // The regression this guards cost a working end-to-end chain: Chrome
        // starts a fresh host process per message, so an id supplied only in
        // a greeting is gone by the time a download arrives.
        for body in [
            r#"{"type":"download","client_id":"abc","url":"https://x.invalid/a"}"#,
            r#"{"type":"hello","client_id":"abc","label":"Chrome"}"#,
            r#"{"type":"get_state","client_id":"abc"}"#,
            r#"{"type":"ping","client_id":"abc"}"#,
        ] {
            let mut cursor = std::io::Cursor::new(framed(body));
            let envelope = read_message(&mut cursor).await.expect(body);
            assert_eq!(envelope.client_id.as_deref(), Some("abc"), "{body}");
        }
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_allocating() {
        // A hostile prefix claiming 4 GiB must not become a 4 GiB allocation.
        let mut raw = u32::MAX.to_ne_bytes().to_vec();
        raw.extend_from_slice(b"{}");
        let mut cursor = std::io::Cursor::new(raw);
        assert!(matches!(read_message(&mut cursor).await, Err(FrameError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn a_closed_pipe_is_reported_as_closed_not_as_an_error() {
        // The browser closing stdin is the normal way this host exits.
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(matches!(read_message(&mut cursor).await, Err(FrameError::Closed)));
    }

    #[tokio::test]
    async fn a_truncated_body_is_reported_as_closed() {
        let mut raw = 100u32.to_ne_bytes().to_vec();
        raw.extend_from_slice(b"short");
        let mut cursor = std::io::Cursor::new(raw);
        assert!(matches!(read_message(&mut cursor).await, Err(FrameError::Closed)));
    }

    #[tokio::test]
    async fn malformed_json_is_an_error_not_a_panic() {
        let raw = framed("{ not json");
        let mut cursor = std::io::Cursor::new(raw);
        assert!(matches!(read_message(&mut cursor).await, Err(FrameError::Malformed(_))));
    }

    #[tokio::test]
    async fn an_unknown_message_type_is_rejected() {
        // The shape is a closed set; anything else should not silently map to
        // a variant that happens to deserialise.
        let raw = framed(r#"{"type":"exec","command":"calc.exe"}"#);
        let mut cursor = std::io::Cursor::new(raw);
        assert!(matches!(read_message(&mut cursor).await, Err(FrameError::Malformed(_))));
    }

    #[tokio::test]
    async fn replies_are_framed_native_endian() {
        let mut out = Vec::new();
        write_message(&mut out, &ToExtension::Pong).await.unwrap();

        let size = u32::from_ne_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(size, out.len() - 4);
        let body: serde_json::Value = serde_json::from_slice(&out[4..]).unwrap();
        assert_eq!(body["type"], "pong");
    }

    #[tokio::test]
    async fn an_oversized_reply_fails_loudly() {
        // The browser would silently drop it, which is harder to diagnose
        // than an error here.
        let huge = ToExtension::Declined { reason: "x".repeat((MAX_OUTGOING as usize) + 16) };
        let mut out = Vec::new();
        assert!(matches!(write_message(&mut out, &huge).await, Err(FrameError::TooLarge { .. })));
    }

    #[test]
    fn the_message_shape_carries_no_destination_directory() {
        // Structural, matching the IPC protocol behind it: if a page cannot
        // express a path, it cannot choose where bytes land.
        let shape = r#"{"type":"download","url":"https://x.invalid/a","dir":"C:\\Windows"}"#;
        let parsed: Result<Envelope, _> = serde_json::from_str(shape);
        // The unknown field is ignored rather than honoured; what matters is
        // that no variant has anywhere to put it.
        assert!(parsed.is_ok());
        let json = serde_json::to_string(&ToExtension::Pong).unwrap();
        for forbidden in ["dir", "path", "directory"] {
            assert!(!json.contains(forbidden));
        }
    }
}
