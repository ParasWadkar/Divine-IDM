//! SFTP downloads, over SSH.
//!
//! # Host keys
//!
//! The decision that matters here. An SFTP client that accepts whatever key
//! a server presents has no security at all: anything able to answer on that
//! address reads the password and the file. So keys are checked against
//! `~/.ssh/known_hosts`, the same file `ssh` uses, and:
//!
//! * a key that matches is accepted;
//! * a key that **differs** from the one on record is refused outright, and
//!   says so, because that is either a reinstalled server or someone in the
//!   middle and only the person can tell which;
//! * a host that is **not** on record is refused, with its fingerprint
//!   printed so it can be checked and added.
//!
//! Refusing an unknown host is stricter than `ssh`, which asks. There is
//! nobody to ask: a download may be started by a browser extension while the
//! window is closed, and a prompt nobody sees would either hang for ever or
//! be answered by whoever is at the keyboard, which is the same as not
//! asking. `accept_new` exists for the person who decides otherwise, and
//! writes the key to `known_hosts` as `ssh` would.

use std::path::PathBuf;
use std::sync::Arc;

use russh::keys::PublicKeyOrCertificate;
use russh::keys::ssh_key::PublicKey;
use russh::{client, keys};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error("ssh: {0}")]
    Ssh(String),
    #[error("sftp: {0}")]
    Sftp(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Refused(String),
    /// Kept separate so a caller can tell a key problem from a key mismatch.
    #[error("{0}")]
    HostKey(String),
}

type Result<T> = std::result::Result<T, SftpError>;

/// What to do about a host that is not in `known_hosts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownHost {
    /// Refuse, and print the fingerprint. The default.
    #[default]
    Refuse,
    /// Accept and record it, as `ssh -o StrictHostKeyChecking=accept-new`.
    AcceptNew,
}

#[derive(Debug, Clone)]
pub struct SftpOptions {
    pub output_dir: PathBuf,
    pub filename: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// A private key to authenticate with, in preference to a password.
    pub identity: Option<PathBuf>,
    /// Passphrase for that key, if it has one.
    pub identity_passphrase: Option<String>,
    pub unknown_host: UnknownHost,
    pub resume: bool,
    /// With no key and no password given, try the keys `ssh` would:
    /// ~/.ssh/id_ed25519, id_ecdsa and id_rsa.
    ///
    /// Only unencrypted ones. An encrypted key needs its passphrase typed,
    /// and a download started from the app or a browser has nobody there to
    /// type it — so those are skipped rather than left to hang.
    pub use_default_identities: bool,
    /// The engine-wide speed limit, if there is one.
    pub limiter: Option<Arc<crate::limiter::RateLimiter>>,
}

impl SftpOptions {
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            filename: None,
            username: None,
            password: None,
            identity: None,
            identity_passphrase: None,
            unknown_host: UnknownHost::default(),
            resume: true,
            use_default_identities: true,
            limiter: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SftpReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub resumed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub filename: String,
    pub username: String,
}

pub fn handles(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("sftp://")
}

/// Picks apart an `sftp://` address.
pub fn parse(url: &str, options: &SftpOptions) -> Result<Target> {
    let parsed = Url::parse(url.trim())
        .map_err(|_| SftpError::Refused(format!("{url} is not a usable address")))?;
    if parsed.scheme() != "sftp" {
        return Err(SftpError::Refused("that is not an sftp address".to_owned()));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| SftpError::Refused("that address names no host".to_owned()))?
        .to_owned();
    let port = parsed.port().unwrap_or(22);

    let path = percent_decode(parsed.path());
    if path.is_empty() || path.ends_with('/') {
        return Err(SftpError::Refused("that address names a directory, not a file".to_owned()));
    }

    let leaf = path.rsplit('/').next().unwrap_or("download.bin");
    let filename = options
        .filename
        .clone()
        .or_else(|| sanitise(leaf))
        .unwrap_or_else(|| "download.bin".to_owned());

    // A username in the address is normal for SSH; a password in one is not,
    // and is ignored rather than honoured — it would reach the job list and
    // the history, and ssh itself does not accept one either.
    let username = options
        .username
        .clone()
        .or_else(|| (!parsed.username().is_empty()).then(|| percent_decode(parsed.username())))
        .ok_or_else(|| {
            SftpError::Refused(
                "sftp needs a username: put it in the address as sftp://user@host/path, \
                 or pass --user"
                    .to_owned(),
            )
        })?;

    Ok(Target { host, port, path, filename, username })
}

/// Checks the server's key against `known_hosts`.
struct HostKeyCheck {
    host: String,
    port: u16,
    policy: UnknownHost,
    /// Filled in when the key was refused, so the caller can say why.
    outcome: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // Only a plain key is checked. A certificate is signed by an
        // authority this has no way to evaluate, and treating one as
        // trustworthy because it parsed would be worse than refusing it.
        let PublicKeyOrCertificate::PublicKey { key, .. } = offered else {
            *self.outcome.lock().unwrap() = Some(
                "the server offered a host certificate, which Divine IDM cannot check".to_owned(),
            );
            return Ok(false);
        };

        match keys::check_known_hosts(&self.host, self.port, key) {
            // On record, and it matches.
            Ok(true) => Ok(true),
            // Not on record at all.
            Ok(false) => match self.policy {
                UnknownHost::AcceptNew => {
                    if let Err(error) = learn(&self.host, self.port, key) {
                        tracing::warn!(%error, "could not record the host key");
                    }
                    Ok(true)
                }
                UnknownHost::Refuse => {
                    *self.outcome.lock().unwrap() = Some(format!(
                        "{} is not in ~/.ssh/known_hosts. Its key is {}. Check that against \
                         the server, then add it with `ssh-keyscan -p {} {} >> \
                         ~/.ssh/known_hosts`, or pass --accept-new-host-key to trust it now.",
                        self.host,
                        fingerprint(key),
                        self.port,
                        self.host
                    ));
                    Ok(false)
                }
            },
            // On record, and this is a different key.
            Err(keys::Error::KeyChanged { line }) => {
                *self.outcome.lock().unwrap() = Some(format!(
                    "the host key for {} has changed. Either the server was rebuilt, or \
                     something is answering in its place, and only you can tell which. The \
                     key now offered is {}. If the change is expected, remove line {line} \
                     from ~/.ssh/known_hosts.",
                    self.host,
                    fingerprint(key)
                ));
                Ok(false)
            }
            Err(error) => {
                *self.outcome.lock().unwrap() =
                    Some(format!("could not check the host key: {error}"));
                Ok(false)
            }
        }
    }
}

/// Appends a host key to `~/.ssh/known_hosts`, as `ssh` does.
///
/// Written here rather than taken from russh, which exposes the check but
/// not the record. The format is one line: `[host]:port keytype base64`,
/// with the brackets and port only when it is not 22 — matching what `ssh`
/// writes, so the two agree about what is known.
fn learn(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let Some(home) = home_dir() else {
        return Err(std::io::Error::other("no home directory"));
    };
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir)?;
    let path = ssh_dir.join("known_hosts");

    let name = if port == 22 { host.to_owned() } else { format!("[{host}]:{port}") };
    let line = format!("{name} {}\n", key.to_openssh().map_err(std::io::Error::other)?);

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(Default::default()).to_string()
}

/// Fetches a file over SFTP.
pub async fn download<F>(url: &str, options: &SftpOptions, mut progress: F) -> Result<SftpReport>
where
    F: FnMut(u64, Option<u64>) + Send,
{
    let target = parse(url, options)?;

    let outcome = Arc::new(std::sync::Mutex::new(None));
    let handler = HostKeyCheck {
        host: target.host.clone(),
        port: target.port,
        policy: options.unknown_host,
        outcome: Arc::clone(&outcome),
    };

    let config = Arc::new(client::Config::default());
    let mut session =
        match client::connect(config, (target.host.as_str(), target.port), handler).await {
            Ok(session) => session,
            Err(error) => {
                // A rejected key surfaces as a connection error, so the
                // reason recorded by the check is what the user should see.
                if let Some(reason) = outcome.lock().unwrap().take() {
                    return Err(SftpError::HostKey(reason));
                }
                return Err(SftpError::Ssh(error.to_string()));
            }
        };

    authenticate(&mut session, &target, options).await?;

    let channel =
        session.channel_open_session().await.map_err(|error| SftpError::Ssh(error.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| SftpError::Ssh(error.to_string()))?;

    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| SftpError::Sftp(error.to_string()))?;

    let total = sftp.metadata(target.path.clone()).await.ok().and_then(|meta| meta.size);

    std::fs::create_dir_all(&options.output_dir)?;
    let final_path = options.output_dir.join(&target.filename);
    let part_path = options.output_dir.join(format!("{}.idmpart", target.filename));

    let already = if options.resume {
        std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    // Never resume past the end: a part file larger than the remote one
    // means they are not the same file.
    let resume_from = match (already, total) {
        (0, _) => 0,
        (have, Some(size)) if have >= size => 0,
        (have, _) => have,
    };

    let mut remote = sftp
        .open(target.path.clone())
        .await
        .map_err(|error| SftpError::Sftp(format!("{}: {error}", target.path)))?;

    if resume_from > 0 {
        // Unlike FTP's REST, seeking is part of the protocol's read: every
        // read names its own offset, so there is no server to refuse it.
        remote
            .seek(std::io::SeekFrom::Start(resume_from))
            .await
            .map_err(|error| SftpError::Sftp(error.to_string()))?;
    }

    let mut sink = if resume_from > 0 {
        tokio::fs::OpenOptions::new().append(true).open(&part_path).await?
    } else {
        tokio::fs::File::create(&part_path).await?
    };

    let mut written = resume_from;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = remote.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        sink.write_all(&buffer[..read]).await?;
        written += read as u64;
        progress(written, total);

        if let Some(limiter) = &options.limiter {
            limiter.take(read).await;
        }
    }
    sink.flush().await?;
    drop(sink);

    let _ = sftp.close().await;

    if let Some(size) = total
        && written != size
    {
        return Err(SftpError::Sftp(format!("the server said {size} bytes and sent {written}")));
    }

    std::fs::rename(&part_path, &final_path)?;
    Ok(SftpReport { path: final_path, bytes: written, resumed_bytes: resume_from })
}

/// Authenticates, preferring a key over a password.
///
/// A key is both safer and what most servers are configured for; a password
/// is the fallback, and some servers accept neither, which is its own
/// message rather than a bare refusal.
async fn authenticate(
    session: &mut client::Handle<HostKeyCheck>,
    target: &Target,
    options: &SftpOptions,
) -> Result<()> {
    if let Some(identity) = &options.identity {
        let key = keys::load_secret_key(identity, options.identity_passphrase.as_deref())
            .map_err(|error| SftpError::Ssh(format!("{}: {error}", identity.display())))?;
        let hash = session
            .best_supported_rsa_hash()
            .await
            .map_err(|error| SftpError::Ssh(error.to_string()))?
            .flatten();

        let result = session
            .authenticate_publickey(
                &target.username,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .map_err(|error| SftpError::Ssh(error.to_string()))?;
        if result.success() {
            return Ok(());
        }
        return Err(SftpError::Refused(format!(
            "the server did not accept the key in {}",
            identity.display()
        )));
    }

    if let Some(password) = &options.password {
        let result = session
            .authenticate_password(&target.username, password)
            .await
            .map_err(|error| SftpError::Ssh(error.to_string()))?;
        if result.success() {
            return Ok(());
        }
        // Deliberately not echoing the password, or how long it was.
        return Err(SftpError::Refused("the server rejected the password".to_owned()));
    }

    if options.use_default_identities {
        let candidates = default_identities();
        for path in &candidates {
            // An encrypted key fails to load without its passphrase, which
            // is the cue to move on rather than to ask.
            let Ok(key) = keys::load_secret_key(path, None) else {
                continue;
            };
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|error| SftpError::Ssh(error.to_string()))?
                .flatten();
            let result = session
                .authenticate_publickey(
                    &target.username,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(|error| SftpError::Ssh(error.to_string()))?;
            if result.success() {
                return Ok(());
            }
        }

        return Err(SftpError::Refused(if candidates.is_empty() {
            "sftp needs a key or a password: there is no key in ~/.ssh, so add one, or \
             download from the command line with --password"
                .to_owned()
        } else {
            "none of the keys in ~/.ssh was accepted. Encrypted keys are skipped, because \
             there is nobody to type a passphrase; add an unencrypted key to the server, or \
             download from the command line with --identity or --password"
                .to_owned()
        }));
    }

    Err(SftpError::Refused(
        "sftp needs a password or a key: pass --password, or --identity with a private key"
            .to_owned(),
    ))
}

/// The private keys `ssh` tries by default, in its order, if they exist.
fn default_identities() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|path| path.is_file())
        .collect()
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

    fn options() -> SftpOptions {
        let mut options = SftpOptions::new("D:/downloads");
        options.username = Some("someone".into());
        options
    }

    #[test]
    fn sftp_addresses_are_recognised_and_ftp_ones_are_not() {
        assert!(handles("sftp://host/a.bin"));
        assert!(handles("SFTP://host/a.bin"));
        assert!(!handles("ftp://host/a.bin"), "a different protocol entirely");
        assert!(!handles("https://host/a.bin"));
    }

    #[test]
    fn the_default_port_is_ssh_not_ftp() {
        assert_eq!(parse("sftp://host/a.bin", &options()).unwrap().port, 22);
        assert_eq!(parse("sftp://host:2222/a.bin", &options()).unwrap().port, 2222);
    }

    #[test]
    fn a_username_may_come_from_the_address() {
        let mut bare = SftpOptions::new("D:/downloads");
        bare.username = None;
        let target = parse("sftp://alice@host/a.bin", &bare).unwrap();
        assert_eq!(target.username, "alice");
    }

    #[test]
    fn without_a_username_anywhere_the_error_says_how_to_give_one() {
        let mut bare = SftpOptions::new("D:/downloads");
        bare.username = None;
        let error = parse("sftp://host/a.bin", &bare).unwrap_err().to_string();
        assert!(error.contains("--user"), "{error}");
    }

    #[test]
    fn a_password_in_the_address_is_ignored_rather_than_used() {
        // ssh does not accept one either, and a URL reaches the job list,
        // the history and any log line that prints it.
        let mut bare = SftpOptions::new("D:/downloads");
        bare.username = None;
        let target = parse("sftp://alice:hunter2@host/a.bin", &bare).unwrap();
        assert_eq!(target.username, "alice");
        // There is nowhere on the target for it to have gone.
        let rendered = format!("{target:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn a_remote_path_keeps_its_shape_but_the_local_name_is_a_leaf() {
        let target = parse("sftp://host/srv/data/archive.tar.gz", &options()).unwrap();
        assert_eq!(target.path, "/srv/data/archive.tar.gz");
        assert_eq!(target.filename, "archive.tar.gz");
    }

    #[test]
    fn percent_escapes_are_undone_for_the_server() {
        let target = parse("sftp://host/my%20folder/a%20file.bin", &options()).unwrap();
        assert_eq!(target.path, "/my folder/a file.bin");
        assert_eq!(target.filename, "a file.bin");
    }

    #[test]
    fn a_name_from_the_server_cannot_escape_the_output_directory() {
        for hostile in [
            "sftp://host/a/..%2f..%2fwindows%2fsystem32",
            "sftp://host/%2e%2e%2fsecrets",
            "sftp://host/a/b:stream",
        ] {
            let target = parse(hostile, &options()).unwrap();
            assert!(!target.filename.contains(['/', '\\', ':']), "{}", target.filename);
            assert!(!target.filename.contains(".."), "{}", target.filename);
        }
    }

    #[test]
    fn a_directory_address_is_refused() {
        assert!(parse("sftp://host/srv/data/", &options()).is_err());
        assert!(parse("sftp://host/", &options()).is_err());
    }

    #[test]
    fn an_unknown_host_is_refused_by_default() {
        // The whole point: a client that trusts any key has no security,
        // and there is nobody to prompt when a browser started the download.
        assert_eq!(UnknownHost::default(), UnknownHost::Refuse);
    }

    #[test]
    fn nonsense_addresses_are_errors_not_panics() {
        for url in ["", "sftp://", "sftp:///a", "not a url", "sftp://[::1"] {
            let _ = parse(url, &options());
        }
    }
}
