//! Fetching the external programs this project runs, on request.
//!
//! `tools/fetch-sidecars.ps1` does this at build time for what ships. This
//! is the same job done for somebody who already installed the app and then
//! hit a video that needed ffmpeg — and it is the same chain of trust, not a
//! looser one, because the chain is the only reason downloading an
//! executable and running it is defensible at all:
//!
//! 1. The address and the expected hash come from `sidecars/manifest.json`,
//!    compiled in. Nothing on disk and nothing on the wire can change which
//!    build is being asked for.
//! 2. The bytes that arrive are hashed and compared to that before anything
//!    is unpacked. A mismatch deletes the download.
//! 3. What comes out of the archive is compared to `sidecars/sidecars.lock`
//!    — also compiled in, and the same list [`idm_media::sidecar`] checks
//!    against immediately before running the program. So a fetch cannot
//!    install something the runner would then refuse, and an unpacking bug
//!    cannot produce a working executable nobody pinned.
//! 4. Only then is it moved into place.
//!
//! Three things this deliberately does not do. It is never automatic: a
//! hundred megabytes is the user's bandwidth and the user's disk, and the
//! sidecar module's own rules say a missing tool is reported rather than
//! fixed behind somebody's back. It does not update anything — a newer
//! build is an edit to the manifest, which is a change somebody reviews.
//! And it refuses browser clients, because "download and install an
//! executable" is not something a web page gets to ask for.

use std::path::{Path, PathBuf};

use idm_ipc::protocol::SidecarView;
use idm_media::sidecar::{self, Tool};

/// The pinned manifest, compiled in rather than read from disk.
///
/// On disk it would be a file in the user's profile saying which address to
/// download an executable from and which hash to accept — which is a file
/// worth editing, and therefore one worth not having.
const MANIFEST: &str = include_str!("../../../sidecars/manifest.json");

#[derive(Debug, Clone, serde::Deserialize)]
struct Manifest {
    tools: Vec<Spec>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Spec {
    name: String,
    version: String,
    why: String,
    url: String,
    sha256: String,
    bytes: u64,
    source: String,
    /// `zip`, or `none` when the download is the program itself.
    archive: String,
    /// Paths inside the archive to take, matched by their tail.
    take: Vec<String>,
}

/// The manifest, parsed once.
///
/// A parse failure is this project's own build being wrong rather than
/// anything the user did, so it is a panic in a test and an empty list in
/// production: an app that will not start because a side feature's manifest
/// is malformed is worse than one where that feature says it has nothing.
fn manifest() -> &'static Manifest {
    static PARSED: std::sync::OnceLock<Manifest> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| match serde_json::from_str::<Manifest>(MANIFEST) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::error!(%error, "sidecars/manifest.json is not readable");
            Manifest { tools: Vec::new() }
        }
    })
}

/// Where a fetched sidecar goes: `%LOCALAPPDATA%\DivineIDM\sidecars`.
///
/// Not beside the executable, which is the natural place and the wrong one:
/// an installed app lives under Program Files, and writing there needs
/// administrator rights the app does not have and should not ask for. The
/// state folder is this user's, this app's, and — the part that matters —
/// not where downloads land. See `StreamOptions::sidecar_dir`.
pub fn dir() -> PathBuf {
    idm_ipc::state_dir().join("sidecars")
}

/// Just the version out of what a tool says when asked for its version.
///
/// ffmpeg answers with a banner — "ffmpeg version 9.0.2-essentials_build-
/// www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers" — and putting
/// that whole sentence on a row meant for a version wrapped it onto three
/// lines and pushed the button beside it onto two. yt-dlp answers with the
/// version alone, so this has to leave that case untouched.
fn short_version(said: &str) -> String {
    let said = said.trim();
    // The word after `version`. Falling back to the whole line rather than
    // to nothing: a tool that phrases it some third way is better shown
    // verbatim than shown blank.
    let token = said
        .split_whitespace()
        .skip_while(|word| !word.eq_ignore_ascii_case("version"))
        .nth(1)
        .unwrap_or(said);
    // A build string can still be long. Enough to tell two builds apart,
    // short enough to sit on one line beside a name and a button.
    if token.chars().count() > 40 {
        token.chars().take(39).chain(std::iter::once('…')).collect()
    } else {
        token.to_owned()
    }
}

/// The tool a spec names, if the runner knows how to run it.
fn tool_for(name: &str) -> Option<Tool> {
    match name {
        "ffmpeg" => Some(Tool::Ffmpeg),
        "ffprobe" => Some(Tool::Ffprobe),
        "yt-dlp" => Some(Tool::YtDlp),
        _ => None,
    }
}

/// What the app can fetch, and what it already has.
///
/// `where_from` is the honest answer rather than a yes or no, because the
/// three cases want different things said: a copy we fetched can be checked
/// against the lock, a copy on the user's `PATH` is theirs and is used as
/// found, and nothing at all is the only case where offering to fetch makes
/// sense.
pub fn views() -> Vec<SidecarView> {
    let managed = dir();
    manifest()
        .tools
        .iter()
        .map(|spec| {
            // `probe` rather than `probe_managed`, deliberately: this is a
            // listing, not permission to run anything. The difference is
            // the pinned checksum, which means reading a hundred megabytes
            // and hashing it -- affordable once before a mux, but this runs
            // every time the Settings panel is opened, and it showed: the
            // card sat empty for seconds while it hashed.
            //
            // Nothing is lost by leaving it out. `probe_managed` still runs
            // immediately before the program is executed, which is the
            // check that decides anything; a copy replaced between this
            // listing and that moment would defeat an eager check here
            // anyway. The version call stays, because a file of the right
            // name that cannot execute is not a tool that is present.
            let found = tool_for(&spec.name)
                .map(|tool| sidecar::probe(tool, Some(&managed)).map_err(|e| e.to_string()));

            let (present, detail, path) = match found {
                Some(Ok(sidecar)) => {
                    let ours = sidecar.path.parent() == Some(managed.as_path());
                    (
                        true,
                        Some(if ours {
                            format!("{} — fetched by this app", short_version(&sidecar.version))
                        } else {
                            format!("{} — already on this machine", short_version(&sidecar.version))
                        }),
                        Some(sidecar.path.display().to_string()),
                    )
                }
                Some(Err(why)) => (false, Some(why), None),
                None => (false, Some("this app has no use for it".into()), None),
            };

            SidecarView {
                name: spec.name.clone(),
                version: spec.version.clone(),
                why: spec.why.clone(),
                bytes: spec.bytes,
                present,
                detail,
                path,
            }
        })
        .collect()
}

/// Fetches the named sidecar and installs it, or says why it did not.
///
/// Long-running on purpose — this is a hundred megabytes over somebody's
/// connection — and it holds no engine lock while it runs, so downloads and
/// the window's ticker carry on underneath it.
pub async fn fetch(name: &str) -> Result<String, String> {
    let Some(spec) = manifest().tools.iter().find(|spec| spec.name == name) else {
        // Named rather than echoed back: the list is fixed and compiled in,
        // so anything else is a client sending something it invented.
        return Err(format!("there is no sidecar called {name:?}"));
    };
    if tool_for(&spec.name).is_none() {
        return Err(format!("{} is not a program this app runs", spec.name));
    }

    let destination = dir();
    std::fs::create_dir_all(&destination)
        .map_err(|error| format!("could not make {}: {error}", destination.display()))?;

    // A scratch folder beside the destination rather than in the system
    // temp, for two reasons: the final move is a rename on the same volume
    // instead of a hundred-megabyte copy, and a half-finished fetch is
    // visible in the place it belongs to rather than left in a shared one.
    let scratch = destination.join(format!(".fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("could not make a scratch folder: {error}"))?;

    let outcome = fetch_into(spec, &scratch, &destination).await;
    // Whatever happened: an unverified executable left lying about is the
    // thing this whole module exists to avoid.
    let _ = std::fs::remove_dir_all(&scratch);
    outcome
}

async fn fetch_into(spec: &Spec, scratch: &Path, destination: &Path) -> Result<String, String> {
    let archive = scratch.join(format!("{}.download", spec.name));
    download(&spec.url, &archive, spec.bytes).await?;

    // Before unpacking, not after. The reason to check at all is that these
    // bytes came off the internet, and an archive reader is the first thing
    // they would reach.
    sidecar::verify_checksum(&archive, &spec.sha256).map_err(|_| {
        format!(
            "what arrived from {} is not the build this app pins. Nothing was installed. \
             Either the publisher replaced it at that address, or something replaced it in \
             transit; {} is where the expected hash came from.",
            spec.url, spec.source
        )
    })?;

    let mut installed = Vec::new();
    for want in &spec.take {
        let leaf = want.rsplit(['/', '\\']).next().unwrap_or(want);
        let taken = match spec.archive.as_str() {
            "none" => archive.clone(),
            "zip" => unzip_one(&archive, want, &scratch.join(leaf))?,
            other => {
                return Err(format!(
                    "{} is packed as {other}, which this app cannot open",
                    spec.name
                ));
            }
        };

        // The other end of the chain, and the one that decides what runs.
        // A sidecar with no entry here is refused rather than installed
        // unpinned: an unpinned copy in a directory the app itself searches
        // first is worse than no copy at all.
        let Some(expected) = sidecar::pinned_hash(leaf) else {
            return Err(format!(
                "{leaf} has no entry in sidecars.lock, so there is nothing to hold it to"
            ));
        };
        sidecar::verify_checksum(&taken, expected).map_err(|error| {
            format!("{leaf} came out of the archive wrong and was not installed: {error}")
        })?;

        let final_path = destination.join(leaf);
        // Replaced rather than refused if something is already there: the
        // bytes have just been checked against the lock, so what lands is
        // what this app would accept either way.
        let _ = std::fs::remove_file(&final_path);
        std::fs::rename(&taken, &final_path)
            .map_err(|error| format!("could not put {leaf} in place: {error}"))?;
        installed.push(leaf.to_owned());
    }

    Ok(format!(
        "{} {} is installed in {} ({})",
        spec.name,
        spec.version,
        destination.display(),
        installed.join(", ")
    ))
}

/// Streams a URL to a file, refusing one that is not the size it should be.
///
/// Written out as it arrives rather than collected first: a hundred
/// megabytes in memory to then write it to disk anyway is a waste the
/// fetch script already learned about the hard way.
async fn download(url: &str, to: &Path, expected_bytes: u64) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .user_agent(concat!("DivinePulse/", env!("CARGO_PKG_VERSION")))
        // Generous, because this is a large file on whatever connection the
        // user has, but not unbounded: the request is being waited on.
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| format!("could not start the fetch: {error}"))?;

    let response =
        client.get(url).send().await.map_err(|error| format!("could not reach {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }

    // Checked before a byte is written, so a wrong file is refused on the
    // strength of the publisher's own header rather than after the whole
    // download. The hash is still the check that matters; this one only
    // saves the wait.
    if let Some(stated) = response.content_length()
        && stated != expected_bytes
    {
        return Err(format!(
            "{url} offers {stated} bytes where this app pins {expected_bytes}, \
             so it is not the build that was checked"
        ));
    }

    let mut file = tokio::fs::File::create(to)
        .await
        .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    let mut response = response;
    let mut written: u64 = 0;
    // `chunk` rather than a stream adapter: it is the same loop without a
    // dependency whose only use here would be one call to `next`.
    while let Some(chunk) =
        response.chunk().await.map_err(|error| format!("the fetch stopped partway: {error}"))?
    {
        written += chunk.len() as u64;
        if written > expected_bytes {
            return Err(format!("{url} sent more than the {expected_bytes} bytes it stated"));
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    }
    file.flush().await.map_err(|error| format!("could not finish writing: {error}"))?;

    if written != expected_bytes {
        return Err(format!("the fetch ended after {written} of {expected_bytes} bytes"));
    }
    Ok(())
}

/// Takes one named file out of a zip, by the tail of its path.
///
/// Matched by tail because the archive's top-level folder carries the
/// version, which would otherwise have to be spelled out again here on
/// every bump. The name written is the one from the manifest, never the one
/// in the archive, so there is no path in this that an archive chose: an
/// entry called `..\..\something.exe` is looked for and not found, which is
/// the whole of the traversal story here.
fn unzip_one(archive: &Path, want: &str, to: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(archive)
        .map_err(|error| format!("could not open the archive: {error}"))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|error| format!("that is not a zip this app can read: {error}"))?;

    let tail = want.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase();
    let suffix = format!("/{tail}");
    // The names are read before anything is extracted, and the one taken is
    // matched by its tail: the archive's top-level folder carries the
    // version, which would otherwise be spelled out again here on every
    // bump.
    let found = zip
        .file_names()
        .flatten()
        .map(|name| name.into_owned())
        .find(|name| {
            let name = name.replace('\\', "/").to_ascii_lowercase();
            name == tail || name.ends_with(&suffix)
        })
        .ok_or_else(|| format!("{want} was not in the archive"))?;

    let mut entry = zip
        .by_name(&found)
        .map_err(|error| format!("could not read {want} out of the archive: {error}"))?;
    let mut out = std::fs::File::create(to)
        .map_err(|error| format!("could not write {}: {error}", to.display()))?;
    std::io::copy(&mut entry, &mut out)
        .map_err(|error| format!("could not unpack {want}: {error}"))?;
    Ok(to.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_manifest_parses() {
        // `manifest()` swallows a parse failure into an empty list in
        // production, which is right there and useless here: this is the
        // test that stops a malformed manifest shipping.
        let manifest: Manifest =
            serde_json::from_str(MANIFEST).expect("sidecars/manifest.json should parse");
        assert!(!manifest.tools.is_empty());
    }

    #[test]
    fn every_tool_in_the_manifest_is_one_the_runner_can_run() {
        // A name only this file and the fetch script agree on would install
        // something nothing ever looks for.
        for spec in &manifest().tools {
            assert!(tool_for(&spec.name).is_some(), "{} is not a Tool", spec.name);
        }
    }

    #[test]
    fn everything_the_manifest_would_install_is_pinned_in_the_lock() {
        // The join between the two files, and the one that decides whether
        // a fetch can succeed at all: `fetch_into` refuses anything with no
        // lock entry, so a manifest entry without one is a button that can
        // only ever fail.
        for spec in &manifest().tools {
            for want in &spec.take {
                let leaf = want.rsplit(['/', '\\']).next().unwrap();
                assert!(
                    sidecar::pinned_hash(leaf).is_some(),
                    "{leaf} is in the manifest but not in sidecars.lock",
                );
            }
        }
    }

    #[test]
    fn a_manifest_entry_states_a_plausible_size_and_hash() {
        for spec in &manifest().tools {
            assert_eq!(spec.sha256.len(), 64, "{}: {}", spec.name, spec.sha256);
            assert!(spec.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{}", spec.name);
            // A zero would make the pre-flight size check accept anything
            // and the "this is how big it is" line a lie.
            assert!(spec.bytes > 1 << 20, "{} claims {} bytes", spec.name, spec.bytes);
            assert!(spec.url.starts_with("https://"), "{}: {}", spec.name, spec.url);
            assert!(spec.source.starts_with("https://"), "{}: {}", spec.name, spec.source);
            assert!(!spec.why.is_empty(), "{} does not say what it is for", spec.name);
        }
    }

    #[tokio::test]
    async fn a_name_that_is_not_in_the_manifest_is_refused() {
        let error = fetch("curl").await.unwrap_err();
        assert!(error.contains("no sidecar"), "{error}");
        // And nothing was created on the way to refusing.
        assert!(!dir().join("curl.exe").exists());
    }

    #[test]
    fn a_version_banner_is_cut_down_to_the_version() {
        // What ffmpeg actually says, which took up three lines of a row
        // meant for one and wrapped the button beside it.
        assert_eq!(
            short_version(
                "ffmpeg version 9.0.2-essentials_build-www.gyan.dev Copyright (c) 2000-2026 \
                 the FFmpeg developers"
            ),
            "9.0.2-essentials_build-www.gyan.dev",
        );
        // yt-dlp says only the version, and must come through untouched.
        assert_eq!(short_version("2026.10.26"), "2026.10.26");
        // Something phrased a third way is shown as it came rather than
        // blanked, because a blank version tells nobody anything.
        assert_eq!(short_version("some other tool, v4"), "some other tool, v4");
        // And a build string long enough to wrap anyway is cut, not kept.
        let long = format!("x version {}", "a".repeat(80));
        assert_eq!(short_version(&long).chars().count(), 40);
    }

    #[test]
    fn the_fetch_directory_is_not_somewhere_downloads_land() {
        // The invariant that makes it safe to search this directory ahead
        // of PATH and then execute what is there.
        let fetched = dir();
        assert!(fetched.starts_with(idm_ipc::state_dir()), "{}", fetched.display());
        assert_ne!(fetched, idm_ipc::state_dir());
    }
}
