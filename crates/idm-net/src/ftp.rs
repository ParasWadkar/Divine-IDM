//! FTP and FTPS downloads.
//!
//! Run on a blocking thread rather than through an async client. FTP is a
//! sequence of round trips on a control connection — log in, set the type,
//! ask the size, seek, then open a second connection for the bytes — and
//! that reads far more clearly as straight-line code than as a state
//! machine. One download occupies one thread for its duration, which is the
//! same bargain the writer and the journal already make.
//!
//! Resume is `REST`: an offset sent before `RETR`, after which the server
//! starts there. Servers that refuse it are common enough that a refusal is
//! handled rather than treated as failure — the download starts again from
//! the beginning, having lost nothing but time.
//!
//! What this does not do is split one file across several connections.
//! Opening four control connections to an FTP server to quarter a file is a
//! good way to be disconnected: unlike an HTTP origin, an FTP server
//! usually counts logins per user and means it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use suppaftp::native_tls::TlsConnector;
use suppaftp::types::FileType;
use suppaftp::{FtpStream, NativeTlsConnector, NativeTlsFtpStream};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum FtpError {
    #[error("ftp: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Refused(String),
}

type Result<T> = std::result::Result<T, FtpError>;

#[derive(Debug, Clone)]
pub struct FtpOptions {
    pub output_dir: PathBuf,
    pub filename: Option<String>,
    /// Credentials, when the URL does not carry them.
    ///
    /// Preferred over putting them in the address: a URL is logged, shown in
    /// a job list and kept in history, and a password in one ends up in all
    /// three.
    pub username: Option<String>,
    pub password: Option<String>,
    /// Pick up where a previous attempt stopped, if the server allows it.
    pub resume: bool,
    /// The engine-wide speed limit, if there is one.
    pub limiter: Option<std::sync::Arc<crate::limiter::RateLimiter>>,
}

impl FtpOptions {
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            filename: None,
            username: None,
            password: None,
            resume: true,
            limiter: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FtpReport {
    pub path: PathBuf,
    pub bytes: u64,
    /// How much was already on disk and not fetched again.
    pub resumed_bytes: u64,
    pub secure: bool,
}

/// What an FTP address resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Absolute path on the server.
    pub path: String,
    pub filename: String,
    pub username: String,
    pub password: String,
    /// `ftps://`, meaning TLS is required rather than merely offered.
    pub secure: bool,
}

/// Whether this is an address this module handles.
pub fn handles(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("ftp://") || lower.starts_with("ftps://")
}

/// Picks apart an FTP address.
///
/// Anonymous by default, which is what a bare `ftp://host/file` means and
/// what every public archive expects.
pub fn parse(url: &str, options: &FtpOptions) -> Result<Target> {
    let parsed = Url::parse(url.trim())
        .map_err(|_| FtpError::Refused(format!("{url} is not a usable address")))?;

    let secure = match parsed.scheme() {
        "ftp" => false,
        "ftps" => true,
        other => return Err(FtpError::Refused(format!("{other} is not an FTP address"))),
    };

    let host = parsed
        .host_str()
        .ok_or_else(|| FtpError::Refused("that address names no host".to_owned()))?
        .to_owned();
    let port = parsed.port().unwrap_or(21);

    let path = parsed.path().to_owned();
    if path.is_empty() || path.ends_with('/') {
        return Err(FtpError::Refused("that address names a directory, not a file".to_owned()));
    }

    // Percent-decoded, because a filename with a space arrives as %20 and the
    // server expects the real character.
    let decoded = percent_decode(&path);
    let leaf = decoded.rsplit('/').next().unwrap_or("download.bin");
    let filename = options
        .filename
        .clone()
        .or_else(|| sanitise(leaf))
        .unwrap_or_else(|| "download.bin".to_owned());

    // Credentials from the options win over the address: putting them in a
    // URL means they reach the job list, the history and any log line that
    // prints it.
    let username = options
        .username
        .clone()
        .or_else(|| (!parsed.username().is_empty()).then(|| percent_decode(parsed.username())))
        .unwrap_or_else(|| "anonymous".to_owned());
    let password = options
        .password
        .clone()
        .or_else(|| parsed.password().map(percent_decode))
        // What every anonymous FTP server has expected for forty years.
        .unwrap_or_else(|| "anonymous@".to_owned());

    Ok(Target { host, port, path: decoded, filename, username, password, secure })
}

/// Fetches a file over FTP or FTPS.
pub async fn download<F>(url: &str, options: &FtpOptions, mut progress: F) -> Result<FtpReport>
where
    F: FnMut(u64, Option<u64>) + Send + 'static,
{
    let target = parse(url, options)?;
    let options = options.clone();
    // The transfer runs on a blocking thread, which can still wait on the
    // async limiter through a handle to the runtime it came from.
    let runtime = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || fetch(&target, &options, &mut progress, &runtime))
        .await
        .map_err(|error| FtpError::Protocol(error.to_string()))?
}

fn fetch<F>(
    target: &Target,
    options: &FtpOptions,
    progress: &mut F,
    runtime: &tokio::runtime::Handle,
) -> Result<FtpReport>
where
    F: FnMut(u64, Option<u64>),
{
    std::fs::create_dir_all(&options.output_dir)?;
    let final_path = options.output_dir.join(&target.filename);
    let part_path = options.output_dir.join(format!("{}.idmpart", target.filename));

    let address = format!("{}:{}", target.host, target.port);

    if target.secure {
        let stream = NativeTlsFtpStream::connect(&address).map_err(protocol)?;
        let connector =
            TlsConnector::new().map_err(|error| FtpError::Protocol(error.to_string()))?;
        let mut stream = stream
            .into_secure(NativeTlsConnector::from(connector), &target.host)
            .map_err(protocol)?;
        transfer(&mut stream, target, options, &part_path, &final_path, progress, runtime)
    } else {
        let mut stream = FtpStream::connect(&address).map_err(protocol)?;
        transfer(&mut stream, target, options, &part_path, &final_path, progress, runtime)
    }
}

/// The part that is the same whether or not TLS is in the way.
fn transfer<T, F>(
    stream: &mut suppaftp::ImplFtpStream<T>,
    target: &Target,
    options: &FtpOptions,
    part_path: &Path,
    final_path: &Path,
    progress: &mut F,
    runtime: &tokio::runtime::Handle,
) -> Result<FtpReport>
where
    T: suppaftp::TlsStream,
    F: FnMut(u64, Option<u64>),
{
    stream.login(&target.username, &target.password).map_err(protocol)?;
    // Binary, always. The default is ASCII, which rewrites line endings in
    // transit and quietly corrupts every file that is not text.
    stream.transfer_type(FileType::Binary).map_err(protocol)?;

    // SIZE is optional and some servers refuse it; an unknown total is
    // workable, it only means no percentage.
    let total = stream.size(&target.path).ok().map(|size| size as u64);

    let already =
        if options.resume { std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0) } else { 0 };

    // Never resume past the end: a part file larger than the remote file
    // means the two are not the same file.
    let resume_from = match (already, total) {
        (0, _) => 0,
        (have, Some(size)) if have >= size => 0,
        (have, _) => have,
    };

    let mut resumed = resume_from;
    if resume_from > 0 {
        // This only records the offset. `REST` is sent as part of the
        // retrieve below, so a server that refuses it fails there and not
        // here — which is why the fallback is around the retrieve.
        let _ = stream.resume_transfer(resume_from as usize);
    }

    let mut reader = match stream.retr_as_stream(&target.path) {
        Ok(reader) => reader,
        Err(error) if resumed > 0 => {
            // Plenty of servers refuse `REST`. Starting from the beginning
            // costs time and nothing else, which is a far better answer
            // than refusing to download at all.
            tracing::info!(
                host = %target.host,
                %error,
                "the server would not resume; fetching the whole file"
            );
            let _ = stream.resume_transfer(0);
            resumed = 0;
            stream.retr_as_stream(&target.path).map_err(protocol)?
        }
        Err(error) => return Err(protocol(error)),
    };

    // Opened after the retrieve, because whether this appends or truncates
    // depends on whether the server agreed to resume.
    let mut sink = if resumed > 0 {
        std::fs::OpenOptions::new().append(true).open(part_path)?
    } else {
        std::fs::File::create(part_path)?
    };
    let mut written = resumed;
    let mut buffer = vec![0u8; 256 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sink.write_all(&buffer[..read])?;
        written += read as u64;
        progress(written, total);

        if let Some(limiter) = &options.limiter
            && limiter.rate() > 0
        {
            runtime.block_on(limiter.take(read));
        }
    }
    sink.flush()?;
    drop(sink);

    // The data connection has to be closed and acknowledged on the control
    // connection, or the next command reads the transfer's completion reply
    // and everything after is one response out of step.
    reader.finish().map_err(protocol)?;
    let _ = stream.quit();

    if let Some(size) = total
        && written != size
    {
        return Err(FtpError::Protocol(format!("the server said {size} bytes and sent {written}")));
    }

    std::fs::rename(part_path, final_path)?;
    Ok(FtpReport {
        path: final_path.to_path_buf(),
        bytes: written,
        resumed_bytes: resumed,
        // Read from where it was decided rather than passed alongside it.
        secure: target.secure,
    })
}

/// Turns an FTP error into a message, without the credentials.
///
/// suppaftp's errors quote the command that failed, and the command that
/// failed is sometimes `PASS`.
fn protocol(error: suppaftp::FtpError) -> FtpError {
    let text = error.to_string();
    let cleaned =
        if text.contains("PASS") { "the server rejected the credentials".to_owned() } else { text };
    FtpError::Protocol(cleaned)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A leaf name that cannot escape the output directory.
fn sanitise(name: &str) -> Option<String> {
    let trimmed = name.trim().trim_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    let cleaned: String = trimmed
        .chars()
        .map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> FtpOptions {
        FtpOptions::new("D:/downloads")
    }

    #[test]
    fn ftp_and_ftps_addresses_are_recognised() {
        assert!(handles("ftp://example.invalid/a.bin"));
        assert!(handles("FTPS://example.invalid/a.bin"));
        assert!(!handles("https://example.invalid/a.bin"));
        assert!(!handles("sftp://example.invalid/a.bin"), "sftp is a different protocol");
    }

    #[test]
    fn a_bare_address_logs_in_anonymously() {
        // What every public archive expects, and what a bare ftp:// means.
        let target = parse("ftp://ftp.example.invalid/pub/file.iso", &options()).unwrap();
        assert_eq!(target.username, "anonymous");
        assert_eq!(target.password, "anonymous@");
        assert_eq!(target.port, 21, "the default port");
        assert_eq!(target.path, "/pub/file.iso");
        assert_eq!(target.filename, "file.iso");
        assert!(!target.secure);
    }

    #[test]
    fn ftps_is_a_different_scheme_not_a_preference() {
        let target = parse("ftps://example.invalid/a.bin", &options()).unwrap();
        assert!(target.secure, "ftps:// means TLS is required");
    }

    #[test]
    fn credentials_in_the_address_are_read_but_options_win() {
        // A URL reaches the job list, the history and any log line that
        // prints it, so credentials passed separately must take precedence.
        let url = "ftp://someone:hunter2@example.invalid/a.bin";
        let target = parse(url, &options()).unwrap();
        assert_eq!(target.username, "someone");
        assert_eq!(target.password, "hunter2");

        let mut with_options = options();
        with_options.username = Some("other".into());
        with_options.password = Some("secret".into());
        let target = parse(url, &with_options).unwrap();
        assert_eq!(target.username, "other");
        assert_eq!(target.password, "secret");
    }

    #[test]
    fn percent_escapes_are_undone_for_the_server() {
        // The server expects the real character; %20 would name a different
        // file, or none.
        let target = parse("ftp://example.invalid/my%20folder/a%20file.bin", &options()).unwrap();
        assert_eq!(target.path, "/my folder/a file.bin");
        assert_eq!(target.filename, "a file.bin");
    }

    #[test]
    fn a_nonstandard_port_is_honoured() {
        let target = parse("ftp://example.invalid:2121/a.bin", &options()).unwrap();
        assert_eq!(target.port, 2121);
    }

    #[test]
    fn a_directory_address_is_refused_rather_than_guessed_at() {
        assert!(parse("ftp://example.invalid/pub/", &options()).is_err());
        assert!(parse("ftp://example.invalid/", &options()).is_err());
    }

    #[test]
    fn a_name_from_the_server_cannot_escape_the_output_directory() {
        // The path comes from whoever published the address.
        for hostile in [
            "ftp://x.invalid/a/..%2f..%2fwindows%2fsystem32%2fdrivers",
            "ftp://x.invalid/%2e%2e%2f%2e%2e%2fsecrets",
            "ftp://x.invalid/a/b/c:stream",
        ] {
            let target = parse(hostile, &options()).unwrap();
            assert!(!target.filename.contains(['/', '\\', ':']), "{}", target.filename);
            assert!(!target.filename.contains(".."), "{}", target.filename);
        }
    }

    #[test]
    fn a_suggested_name_overrides_the_one_on_the_server() {
        let mut chosen = options();
        chosen.filename = Some("renamed.iso".into());
        let target = parse("ftp://x.invalid/pub/original.iso", &chosen).unwrap();
        assert_eq!(target.filename, "renamed.iso");
        assert_eq!(target.path, "/pub/original.iso", "the remote path is unchanged");
    }

    #[test]
    fn a_failed_login_does_not_repeat_the_password_back() {
        // suppaftp quotes the command that failed, and that command is
        // sometimes PASS.
        let error =
            protocol(suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                suppaftp::Status::NotLoggedIn,
                b"PASS hunter2 rejected".to_vec(),
            )));
        let text = error.to_string();
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn nonsense_addresses_are_errors_not_panics() {
        for url in ["", "ftp://", "ftp:///a", "not a url", "ftp://[::1"] {
            let _ = parse(url, &options());
        }
    }
}
