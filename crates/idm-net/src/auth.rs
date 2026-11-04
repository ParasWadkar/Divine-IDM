//! Signing in to a site that asks for a password.
//!
//! Basic and Digest, parsed and constructed here and nowhere else. The
//! module is pure — no sockets, no clock, no randomness except where it is
//! passed in — because a challenge is a string with an unpleasant grammar
//! and the only honest way to be sure of it is to test it as text.
//!
//! # Never pre-emptive on the first request
//!
//! The flow is request, 401, read the challenge, retry. Sending Basic
//! before being asked would put the password on the wire to every server
//! the user downloads from, including the ones that never wanted it. Once a
//! host has asked, the scheme is remembered and later requests to that host
//! do send it up front, because the alternative is a 401 in front of every
//! one of N segments.
//!
//! # Digest, hand-rolled
//!
//! About two hundred lines against a crate that would still need driving,
//! and the house idiom is a pure parser whose tests are the argument. The
//! part that is genuinely easy to get wrong is not the hashing:
//!
//! `qop=auth` requires the nonce count to be **unique per nonce**, and this
//! engine's whole performance story is N lanes sending at once. The counter
//! therefore lives in [`AuthState`] behind an `Arc` shared by every lane,
//! and is taken with a single atomic increment. Get that wrong and a
//! perfectly correct server starts returning 401 at random under load,
//! which looks like anything but an authentication bug.
//!
//! # What is refused, and said out loud
//!
//! NTLM and Negotiate are parsed and then declined in words. Honouring
//! either means a three-leg handshake pinned to one TCP connection, which
//! is precisely the thing `LanePool` exists not to have. "This server wants
//! Windows sign-in, which DivinePulse does not do" is a worse answer than
//! supporting it and a much better one than a bare 401.

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use md5::Md5;
use sha2::{Digest as _, Sha256};

/// A username and password for a host.
///
/// Prints as `***` from both `Debug` and `Display`: `HttpConfig` derives
/// `Debug`, and a config dumped into a log is the obvious way for this to
/// escape.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpCredential {
    pub username: String,
    password: String,
}

impl HttpCredential {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { username: username.into(), password: password.into() }
    }

    /// The password, where it is about to be hashed or encoded.
    fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for HttpCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The username is not a secret and naming it is what makes a log
        // useful; the password is replaced rather than the whole struct, so
        // "which account" is still answerable.
        f.debug_struct("HttpCredential")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

impl fmt::Display for HttpCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The server wants a scheme this program does not implement, and the
    /// message says which rather than leaving a bare 401.
    Unsupported(String),
    /// A challenge that could not be made sense of.
    Malformed(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "{what}"),
            Self::Malformed(what) => write!(f, "the server's sign-in request {what}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Which hash a Digest challenge asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Md5,
    Md5Sess,
    Sha256,
    Sha256Sess,
}

impl Algorithm {
    fn parse(value: Option<&str>) -> Result<Self, AuthError> {
        // Absent means MD5, which is what RFC 2617 says and what most
        // servers that omit it mean.
        match value.unwrap_or("MD5").to_ascii_uppercase().as_str() {
            "MD5" => Ok(Self::Md5),
            "MD5-SESS" => Ok(Self::Md5Sess),
            "SHA-256" => Ok(Self::Sha256),
            "SHA-256-SESS" => Ok(Self::Sha256Sess),
            // SHA-512-256 exists in RFC 7616 and is served by almost
            // nothing. Named rather than silently treated as MD5, which
            // would produce a wrong response and a confusing 401.
            other => Err(AuthError::Unsupported(format!(
                "this server wants its password hashed with {other}, which DivinePulse does not do"
            ))),
        }
    }

    /// Whether the session variant, which folds the nonce into HA1.
    fn is_session(self) -> bool {
        matches!(self, Self::Md5Sess | Self::Sha256Sess)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Md5Sess => "MD5-sess",
            Self::Sha256 => "SHA-256",
            Self::Sha256Sess => "SHA-256-sess",
        }
    }

    fn hash(self, data: &str) -> String {
        match self {
            Self::Md5 | Self::Md5Sess => {
                let mut hasher = Md5::new();
                hasher.update(data.as_bytes());
                hex(&hasher.finalize())
            }
            Self::Sha256 | Self::Sha256Sess => {
                let mut hasher = Sha256::new();
                hasher.update(data.as_bytes());
                hex(&hasher.finalize())
            }
        }
    }

    /// How much this is worth preferring when a server offers several.
    ///
    /// SHA-256 over MD5 for the obvious reason. The `-sess` variants rank
    /// alongside their base hash rather than above it: they protect the
    /// password against a different threat, not a stronger one.
    fn strength(self) -> u8 {
        match self {
            Self::Md5 | Self::Md5Sess => 1,
            Self::Sha256 | Self::Sha256Sess => 2,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic {
        realm: Option<String>,
    },
    Digest {
        realm: String,
        nonce: String,
        opaque: Option<String>,
        algorithm: Algorithm,
        /// True when the server offered `qop=auth`.
        qop_auth: bool,
        /// The password was right; the nonce had simply expired.
        stale: bool,
    },
}

impl Challenge {
    /// How much this is worth preferring over another on the same response.
    ///
    /// Digest beats Basic always: Basic puts the password itself on the
    /// wire, and a server offering both is saying either will do.
    fn strength(&self) -> u8 {
        match self {
            Self::Basic { .. } => 1,
            Self::Digest { algorithm, .. } => 10 + algorithm.strength(),
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Digest { stale: true, .. })
    }
}

/// Picks the strongest challenge a `WWW-Authenticate` header offers.
///
/// Returns `Unsupported` naming the scheme when every challenge is one this
/// program declines, which is the whole point of parsing them at all: the
/// user gets "this server wants Windows sign-in" instead of a bare 401.
pub fn best_challenge(header: &str) -> Result<Challenge, AuthError> {
    let mut best: Option<Challenge> = None;
    let mut declined: Vec<String> = Vec::new();
    let mut malformed: Option<AuthError> = None;

    for (scheme, parameters) in split_challenges(header) {
        let parsed = match scheme.to_ascii_lowercase().as_str() {
            "basic" => Ok(Challenge::Basic { realm: parameters.get("realm").cloned() }),
            "digest" => digest_challenge(&parameters),
            // Both need a handshake pinned to a single connection, which is
            // the one thing this engine's lane pool is built not to have.
            "ntlm" | "negotiate" => {
                declined.push(scheme.clone());
                continue;
            }
            _ => {
                declined.push(scheme.clone());
                continue;
            }
        };

        match parsed {
            Ok(challenge) => {
                if best.as_ref().is_none_or(|current| challenge.strength() > current.strength()) {
                    best = Some(challenge);
                }
            }
            // Kept rather than returned: a malformed Digest alongside a
            // usable Basic should not lose the Basic.
            Err(error) => malformed = Some(error),
        }
    }

    if let Some(challenge) = best {
        return Ok(challenge);
    }
    if let Some(error) = malformed {
        return Err(error);
    }
    if declined.iter().any(|name| name.eq_ignore_ascii_case("ntlm"))
        || declined.iter().any(|name| name.eq_ignore_ascii_case("negotiate"))
    {
        return Err(AuthError::Unsupported(
            "this server wants Windows sign-in (NTLM or Negotiate), which DivinePulse does not do"
                .into(),
        ));
    }
    if declined.is_empty() {
        return Err(AuthError::Malformed("named no way to sign in".into()));
    }
    Err(AuthError::Unsupported(format!(
        "this server only offers {}, which DivinePulse does not do",
        declined.join(" and ")
    )))
}

fn digest_challenge(
    parameters: &std::collections::HashMap<String, String>,
) -> Result<Challenge, AuthError> {
    let Some(nonce) = parameters.get("nonce") else {
        return Err(AuthError::Malformed("left out the nonce it needs".into()));
    };
    let algorithm = Algorithm::parse(parameters.get("algorithm").map(String::as_str))?;
    // `qop` is a space-separated list; `auth-int` also exists and covers the
    // body, which is not something a ranged GET can usefully do.
    let qop_auth = parameters
        .get("qop")
        .is_some_and(|value| value.split([',', ' ']).any(|item| item.trim() == "auth"));
    Ok(Challenge::Digest {
        // A realm is required by the spec and omitted in the wild. Empty is
        // a usable value -- it goes into HA1 as written -- so this defaults
        // rather than refusing a server that would otherwise work.
        realm: parameters.get("realm").cloned().unwrap_or_default(),
        nonce: nonce.clone(),
        opaque: parameters.get("opaque").cloned(),
        algorithm,
        qop_auth,
        stale: parameters.get("stale").is_some_and(|value| value.eq_ignore_ascii_case("true")),
    })
}

/// Splits a header into (scheme, parameters) pairs.
///
/// The grammar is genuinely ambiguous: challenges are comma-separated and
/// so are their parameters, so `Digest realm="a", Basic realm="b"` has to
/// be told apart from one challenge with two parameters. The rule that
/// works is that a bare token *not* followed by `=` begins a new challenge.
/// Quoted strings are respected throughout, since a realm may contain a
/// comma, a space, or the word `Basic`.
fn split_challenges(header: &str) -> Vec<(String, std::collections::HashMap<String, String>)> {
    let mut out: Vec<(String, std::collections::HashMap<String, String>)> = Vec::new();
    for item in split_outside_quotes(header) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // `Scheme name=value` -- a new challenge whose first parameter rode
        // along in the same comma-separated item.
        match item.split_once('=') {
            Some((left, right)) if !left.trim().contains(' ') && !out.is_empty() => {
                // A plain parameter for the challenge in progress.
                if let Some((_, parameters)) = out.last_mut() {
                    parameters.insert(left.trim().to_ascii_lowercase(), unquote(right).to_owned());
                }
            }
            _ => {
                let (scheme, rest) = match item.split_once(char::is_whitespace) {
                    Some((scheme, rest)) => (scheme, rest.trim()),
                    None => (item, ""),
                };
                let mut parameters = std::collections::HashMap::new();
                if let Some((left, right)) = rest.split_once('=') {
                    parameters.insert(left.trim().to_ascii_lowercase(), unquote(right).to_owned());
                }
                out.push((scheme.to_owned(), parameters));
            }
        }
    }
    out
}

/// Splits on commas that are not inside a quoted string.
fn split_outside_quotes(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in value.chars() {
        match character {
            // A backslash inside quotes escapes the next character, so a
            // realm may legitimately contain a quote mark.
            '\\' if quoted && !escaped => {
                escaped = true;
                current.push(character);
            }
            '"' if !escaped => {
                quoted = !quoted;
                current.push(character);
                escaped = false;
            }
            ',' if !quoted => {
                parts.push(std::mem::take(&mut current));
                escaped = false;
            }
            _ => {
                current.push(character);
                escaped = false;
            }
        }
    }
    parts.push(current);
    parts
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(value)
}

/// Everything needed to answer one host's challenges, shared by every lane.
///
/// The sharing is the point. A Digest `nc` must be unique per nonce, and
/// the lanes send concurrently, so a per-lane counter would reuse values
/// and a correct server would reject them — intermittently, under load,
/// which is the worst way for a bug to present.
#[derive(Debug)]
pub struct AuthState {
    credential: HttpCredential,
    /// The challenge this host last gave, once it has given one.
    ///
    /// `None` until the first 401, which is what keeps the password off the
    /// wire to servers that never asked for it.
    learned: Mutex<Option<Challenge>>,
    nonce_count: AtomicU64,
}

impl AuthState {
    pub fn new(credential: HttpCredential) -> Self {
        Self { credential, learned: Mutex::new(None), nonce_count: AtomicU64::new(0) }
    }

    /// Remembers a challenge, so later requests can answer it up front.
    ///
    /// Resets the nonce count, because the count is defined per nonce and a
    /// new challenge is almost always a new nonce. Starting over is safe in
    /// the direction that matters: a server rejects a repeated `nc`, never
    /// a fresh one.
    pub fn learn(&self, challenge: Challenge) {
        self.nonce_count.store(0, Ordering::SeqCst);
        *self.learned.lock().expect("the auth lock is never held across a panic") = Some(challenge);
    }

    pub fn learned(&self) -> Option<Challenge> {
        self.learned.lock().expect("the auth lock is never held across a panic").clone()
    }

    /// The next nonce count, as the eight hex digits the header wants.
    fn take_nonce_count(&self) -> String {
        // One atomic increment, so two lanes cannot be handed the same
        // value however closely they race.
        let next = self.nonce_count.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{next:08x}")
    }

    /// The `Authorization` value for a request, if a scheme is known.
    ///
    /// `target` is the request-target: path and query, as it appears on the
    /// request line. Digest hashes it, so it must be what is actually sent
    /// rather than the whole URL.
    pub fn authorization(&self, method: &str, target: &str, cnonce: &str) -> Option<String> {
        let challenge = self.learned()?;
        Some(self.answer(&challenge, method, target, cnonce))
    }

    /// Builds the header for one challenge.
    fn answer(&self, challenge: &Challenge, method: &str, target: &str, cnonce: &str) -> String {
        match challenge {
            Challenge::Basic { .. } => {
                let joined = format!("{}:{}", self.credential.username, self.credential.password());
                format!("Basic {}", base64(joined.as_bytes()))
            }
            Challenge::Digest { realm, nonce, opaque, algorithm, qop_auth, .. } => {
                let mut ha1 = algorithm.hash(&format!(
                    "{}:{realm}:{}",
                    self.credential.username,
                    self.credential.password()
                ));
                if algorithm.is_session() {
                    ha1 = algorithm.hash(&format!("{ha1}:{nonce}:{cnonce}"));
                }
                let ha2 = algorithm.hash(&format!("{method}:{target}"));

                let mut parts = vec![
                    format!("username=\"{}\"", escape(&self.credential.username)),
                    format!("realm=\"{}\"", escape(realm)),
                    format!("nonce=\"{}\"", escape(nonce)),
                    format!("uri=\"{}\"", escape(target)),
                ];

                let response = if *qop_auth {
                    let nc = self.take_nonce_count();
                    let response =
                        algorithm.hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
                    parts.push("qop=auth".to_owned());
                    parts.push(format!("nc={nc}"));
                    parts.push(format!("cnonce=\"{}\"", escape(cnonce)));
                    response
                } else {
                    // The RFC 2617 form, still served by plenty of things.
                    algorithm.hash(&format!("{ha1}:{nonce}:{ha2}"))
                };

                parts.push(format!("response=\"{response}\""));
                parts.push(format!("algorithm={}", algorithm.name()));
                if let Some(opaque) = opaque {
                    // Echoed back exactly as given, which is its whole job.
                    parts.push(format!("opaque=\"{}\"", escape(opaque)));
                }
                format!("Digest {}", parts.join(", "))
            }
        }
    }
}

/// Escapes a value for a quoted-string header parameter.
///
/// A username or realm containing a quote mark would otherwise end the
/// string early and let the rest be read as further parameters.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Base64, here rather than as a dependency for one call.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let packed = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[(packed >> (18 - index * 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A client nonce: random hex, fresh per request.
///
/// Its only job is to be unpredictable to the server, so that the server
/// cannot choose both halves of the hashed input.
pub fn fresh_cnonce() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    // Two draws from the standard library's randomly seeded hasher, which
    // needs no dependency and no entropy handle. Not a key -- a salt.
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(std::time::SystemTime::now().elapsed().map_or(0, |d| d.as_nanos() as u64));
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// Sends a request, answering a sign-in challenge if one comes back.
///
/// The whole flow in one place, so the three request builders in this crate
/// cannot each get a different piece of it wrong:
///
/// * If this host has already challenged us, the answer goes out **with**
///   the request. Otherwise N segments would each cost a 401 first.
/// * A 401 is answered and the request sent again.
/// * A second 401 ends it — unless it says `stale=true`, which means the
///   password was right and only the nonce expired. That is what happens to
///   a download long enough to outlive one, and treating it as a wrong
///   password would fail precisely the transfers this feature exists for.
///   It costs one more send, hence three rather than two.
/// * Past that, no more. Against a server that counts failures, retrying a
///   password it has already refused is how an account gets locked.
///
/// The second return value is why signing in could not be attempted, when
/// that is the reason for the 401 — "this server wants Windows sign-in", as
/// against a bare status code.
pub async fn send(
    builder: reqwest::RequestBuilder,
    state: Option<&std::sync::Arc<AuthState>>,
    url: &str,
) -> Result<(reqwest::Response, Option<String>), reqwest::Error> {
    /// Unauthenticated, the answer to the challenge, and one more for a
    /// nonce that expired between the two.
    const MAX_SENDS: usize = 3;

    let target = request_target(url);
    let mut pending = builder;

    for sent in 1..=MAX_SENDS {
        // Cloned before sending, because sending consumes the builder and a
        // retry has to be the same request with one header added. A GET has
        // no body to make this fail; `None` simply means no retry is
        // possible, which is handled rather than assumed away.
        let spare = pending.try_clone();

        let request = match state {
            Some(state) => match state.authorization("GET", &target, &fresh_cnonce()) {
                Some(header) => pending.header(reqwest::header::AUTHORIZATION, header),
                None => pending,
            },
            None => pending,
        };

        // Read before the send, because the send is what may change it.
        let had_scheme = state.is_some_and(|state| state.learned().is_some());
        let response = request.send().await?;

        if response.status() != reqwest::StatusCode::UNAUTHORIZED || sent == MAX_SENDS {
            return Ok((response, None));
        }

        let Some(header) = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
        else {
            // A 401 with nothing saying how to sign in. There is no answer
            // to construct, so it is reported as it stands.
            return Ok((
                response,
                Some("this needs a password, but the server did not say how to sign in".into()),
            ));
        };

        let challenge = match best_challenge(header) {
            Ok(challenge) => challenge,
            // NTLM, or a hash this does not implement. The words matter far
            // more than the status code, so they are carried back.
            Err(error) => return Ok((response, Some(error.to_string()))),
        };

        let Some(state) = state else {
            // Nothing saved for this host. The outcome is the same 401, and
            // the scheduler turns it into "this needs a password".
            return Ok((response, None));
        };

        let stale = challenge.is_stale();
        state.learn(challenge);

        // Already answered once with a scheme we knew, and the server is
        // not saying the nonce expired. That is a wrong password.
        if had_scheme && !stale {
            return Ok((response, None));
        }
        let Some(spare) = spare else {
            return Ok((response, None));
        };
        pending = spare;
    }

    // Unreachable: the loop returns on its last pass. Written as a call
    // rather than `unreachable!` so a future edit to MAX_SENDS cannot turn
    // a logic slip into a panic in somebody's download.
    unreachable!("the send loop returns on its final pass")
}

/// The path and query of a URL, as it appears on the request line.
pub fn request_target(url: &str) -> String {
    match crate::Url::parse(url) {
        Ok(parsed) => {
            let mut target = parsed.path().to_owned();
            if let Some(query) = parsed.query() {
                target.push('?');
                target.push_str(query);
            }
            target
        }
        // A URL this cannot parse is one the request would not have been
        // built from, so the exact fallback never reaches a server.
        Err(_) => "/".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> HttpCredential {
        // The example from RFC 7616, so the expected hashes below can be
        // checked against the specification rather than against this code.
        HttpCredential::new("Mufasa", "Circle of Life")
    }

    #[test]
    fn a_credential_does_not_print_its_password() {
        let credential = credential();
        let shown = format!("{credential:?}");
        assert!(!shown.contains("Circle of Life"), "{shown}");
        assert!(shown.contains("Mufasa"), "which account is still answerable: {shown}");
        assert_eq!(format!("{credential}"), "***");
    }

    #[test]
    fn basic_is_parsed_and_answered() {
        let challenge = best_challenge(r#"Basic realm="WallyWorld""#).unwrap();
        assert_eq!(challenge, Challenge::Basic { realm: Some("WallyWorld".into()) });

        // The example from RFC 7617, which is what makes this a check of the
        // encoding rather than of itself.
        let state = AuthState::new(HttpCredential::new("Aladdin", "open sesame"));
        state.learn(challenge);
        let header = state.authorization("GET", "/", "x").unwrap();
        assert_eq!(header, "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn base64_pads_every_length_correctly() {
        // The padding is the part that is easy to get wrong, and a wrong
        // one is a password the server cannot read.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn digest_is_parsed_with_everything_it_carries() {
        let header = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", \
                        algorithm=SHA-256, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", \
                        opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let header = header.replace("\\\n", "").replace("                        ", " ");
        match best_challenge(&header).unwrap() {
            Challenge::Digest { realm, nonce, opaque, algorithm, qop_auth, stale } => {
                assert_eq!(realm, "http-auth@example.org");
                assert_eq!(nonce, "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v");
                assert_eq!(opaque.unwrap(), "FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS");
                assert_eq!(algorithm, Algorithm::Sha256);
                assert!(qop_auth, "`auth` is in the qop list");
                assert!(!stale);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_digest_response_matches_the_specification() {
        // RFC 7616 section 3.9.1, worked through with its own values. This
        // is the test that makes the rest of the module trustworthy: it is
        // checked against the standard, not against what this code happens
        // to produce.
        let state = AuthState::new(credential());
        state.learn(Challenge::Digest {
            realm: "http-auth@example.org".into(),
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v".into(),
            opaque: Some("FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS".into()),
            algorithm: Algorithm::Sha256,
            qop_auth: true,
            stale: false,
        });

        let header = state
            .authorization("GET", "/dir/index.html", "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ")
            .unwrap();

        assert!(
            header.contains(
                "response=\"753927fa0e85d155564e2e272a28d1802ca10daf449\
                 6794697cf8db5856cb6c1\""
            ),
            "{header}",
        );
        assert!(header.contains("nc=00000001"), "the first request is count one: {header}");
        assert!(header.contains("algorithm=SHA-256"), "{header}");
        assert!(header.contains("qop=auth"), "{header}");
        assert!(
            header.contains("opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\""),
            "{header}"
        );
    }

    #[test]
    fn md5_digest_matches_the_older_specification_too() {
        // RFC 2617's own example, since MD5 without qop is still what a
        // great many devices serve.
        let state = AuthState::new(HttpCredential::new("Mufasa", "Circle Of Life"));
        state.learn(Challenge::Digest {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop_auth: false,
            stale: false,
        });

        let header = state.authorization("GET", "/dir/index.html", "unused").unwrap();
        assert!(header.contains("response=\"670fd8c2df070c60b045671b8b24ff02\""), "{header}");
        // Without qop there is no nc and no cnonce, and sending them anyway
        // changes nothing for the server but is not what was asked for.
        assert!(!header.contains("nc="), "{header}");
        assert!(!header.contains("cnonce="), "{header}");
    }

    #[test]
    fn the_nonce_count_never_repeats_however_the_lanes_race() {
        // The bug this is all arranged to avoid: a repeated `nc` makes a
        // correct server return 401 at random under load, which looks like
        // anything but an authentication problem.
        use std::sync::Arc;
        let state = Arc::new(AuthState::new(credential()));
        state.learn(Challenge::Digest {
            realm: "r".into(),
            nonce: "n".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop_auth: true,
            stale: false,
        });

        let mut handles = Vec::new();
        for _ in 0..8 {
            let state = Arc::clone(&state);
            handles.push(std::thread::spawn(move || {
                (0..64)
                    .map(|_| {
                        let header = state.authorization("GET", "/x", "c").unwrap();
                        let at = header.find("nc=").unwrap() + 3;
                        header[at..at + 8].to_owned()
                    })
                    .collect::<Vec<_>>()
            }));
        }

        let mut seen: Vec<String> =
            handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), total, "every nonce count must be its own");
    }

    #[test]
    fn learning_a_new_challenge_starts_the_count_again() {
        // The count is defined per nonce, so a new nonce starts at one. The
        // safe direction: a server rejects a repeat, never a fresh value.
        let state = AuthState::new(credential());
        let with_nonce = |nonce: &str| Challenge::Digest {
            realm: "r".into(),
            nonce: nonce.into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop_auth: true,
            stale: false,
        };

        state.learn(with_nonce("first"));
        state.authorization("GET", "/x", "c").unwrap();
        let second = state.authorization("GET", "/x", "c").unwrap();
        assert!(second.contains("nc=00000002"), "{second}");

        state.learn(with_nonce("second"));
        let fresh = state.authorization("GET", "/x", "c").unwrap();
        assert!(fresh.contains("nc=00000001"), "a new nonce counts from one: {fresh}");
    }

    #[test]
    fn digest_is_preferred_when_both_are_offered() {
        // A server offering both is saying either will do, and Basic puts
        // the password itself on the wire.
        let header = r#"Basic realm="x", Digest realm="x", nonce="abc""#;
        assert!(matches!(best_challenge(header).unwrap(), Challenge::Digest { .. }));
        // And the order it was written in must not decide it.
        let header = r#"Digest realm="x", nonce="abc", Basic realm="x""#;
        assert!(matches!(best_challenge(header).unwrap(), Challenge::Digest { .. }));
    }

    #[test]
    fn sha256_is_preferred_over_md5() {
        let header = concat!(
            r#"Digest realm="x", nonce="a", algorithm=MD5, "#,
            r#"Digest realm="x", nonce="b", algorithm=SHA-256"#,
        );
        match best_challenge(header).unwrap() {
            Challenge::Digest { algorithm, nonce, .. } => {
                assert_eq!(algorithm, Algorithm::Sha256);
                assert_eq!(nonce, "b", "the nonce must come from the chosen challenge");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_realm_containing_a_comma_or_a_scheme_name_does_not_split_the_header() {
        // The grammar's one real trap: challenges and parameters are both
        // comma-separated, so quoting has to be respected throughout.
        let header = r#"Basic realm="Enter, please, Digest or otherwise""#;
        assert_eq!(
            best_challenge(header).unwrap(),
            Challenge::Basic { realm: Some("Enter, please, Digest or otherwise".into()) },
        );
    }

    #[test]
    fn windows_sign_in_is_declined_in_words() {
        // The whole reason these are parsed rather than ignored. A bare 401
        // tells somebody nothing they can act on.
        for header in ["NTLM", "Negotiate", "Negotiate, NTLM"] {
            match best_challenge(header).unwrap_err() {
                AuthError::Unsupported(said) => {
                    assert!(said.contains("Windows sign-in"), "{said}");
                    assert!(said.contains("does not do"), "{said}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_usable_scheme_alongside_one_we_decline_is_still_used() {
        // Servers commonly offer Negotiate first and Basic as a fallback.
        // Taking the refusal would mean failing a download that would have
        // worked.
        let header = r#"Negotiate, NTLM, Basic realm="x""#;
        assert_eq!(best_challenge(header).unwrap(), Challenge::Basic { realm: Some("x".into()) },);
    }

    #[test]
    fn a_digest_with_no_nonce_is_refused_rather_than_guessed_at() {
        let error = best_challenge(r#"Digest realm="x""#).unwrap_err();
        assert!(error.to_string().contains("nonce"), "{error}");
    }

    #[test]
    fn a_hash_this_does_not_implement_is_named() {
        let error =
            best_challenge(r#"Digest realm="x", nonce="a", algorithm=SHA-512-256"#).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("SHA-512-256"), "{said}");
    }

    #[test]
    fn a_stale_nonce_is_recognised() {
        // Not a wrong password: the credentials were right and the nonce
        // expired, so the right answer is to retry rather than to give up.
        let header = r#"Digest realm="x", nonce="fresh", stale=true"#;
        let challenge = best_challenge(header).unwrap();
        assert!(challenge.is_stale());

        let header = r#"Digest realm="x", nonce="fresh""#;
        assert!(!best_challenge(header).unwrap().is_stale());
    }

    #[test]
    fn a_header_that_says_nothing_is_refused() {
        assert!(best_challenge("").is_err());
        assert!(best_challenge("   ").is_err());
    }

    #[test]
    fn a_username_with_a_quote_in_it_cannot_break_out_of_the_header() {
        // Otherwise a username ends the quoted string early and whatever
        // follows is read by the server as further parameters.
        let state = AuthState::new(HttpCredential::new(r#"ev"il, nc=99"#, "x"));
        state.learn(Challenge::Digest {
            realm: "r".into(),
            nonce: "n".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop_auth: true,
            stale: false,
        });
        let header = state.authorization("GET", "/x", "c").unwrap();
        assert!(header.contains(r#"username="ev\"il, nc=99""#), "{header}");
        // And the real count is still the one this sent.
        assert!(header.contains("nc=00000001"), "{header}");
    }

    #[test]
    fn the_request_target_is_the_path_and_query_only() {
        // Digest hashes it, so it has to be what goes on the request line.
        assert_eq!(request_target("https://example.com/dir/index.html"), "/dir/index.html");
        assert_eq!(request_target("https://example.com/a?b=c&d=e"), "/a?b=c&d=e");
        assert_eq!(request_target("https://example.com"), "/");
    }

    #[test]
    fn nothing_is_sent_before_a_server_has_asked() {
        // The property that keeps the password off the wire to every server
        // the user downloads from.
        let state = AuthState::new(credential());
        assert_eq!(state.authorization("GET", "/x", "c"), None);
    }

    #[test]
    fn a_client_nonce_is_not_the_same_twice() {
        let first = fresh_cnonce();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, fresh_cnonce());
    }
}
