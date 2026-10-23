//! Registering the native messaging host with whatever browsers are present.
//!
//! # Why this is a table and not four hardcoded paths
//!
//! There are only two extension codebases possible on Windows: one Chromium
//! build and one Gecko build. Every Chromium browser speaks the same
//! extension API and the same native messaging protocol, and so does every
//! Firefox fork. The *only* thing that differs per browser is the registry
//! key the host manifest is registered under.
//!
//! So supporting "any browser" is not N implementations, it is one table and
//! a loop. A browser missing from the table can still be added by hand with
//! `--extra-root`, and nothing here assumes a fixed set.
//!
//! Safari is the genuine exclusion: it is macOS-only and uses a different
//! extension model entirely.
//!
//! # The two manifest dialects
//!
//! Chromium lists permitted callers under `allowed_origins`, as
//! `chrome-extension://<id>/`. Gecko uses `allowed_extensions` with a plain
//! extension id. Same file otherwise, so both are generated from one place.
//! That list is the browser-enforced restriction on who may launch the host,
//! which is why it is never left empty.

use std::path::{Path, PathBuf};

/// The host name both the manifest and the registry key use.
pub const HOST_NAME: &str = "com.divine.idm";

pub use idm_engine::browsers::{BROWSERS, Browser, Family, browser_present};

/// A browser the table does not list, named on the command line.
///
/// The table cannot be complete -- forks appear faster than anyone updates
/// it -- so this is the escape hatch that keeps "any browser" true. Only two
/// things about a browser matter: which registry root it reads, and which
/// manifest dialect it speaks. Both are given here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraRoot {
    /// Registry path under `Software\`, excluding the
    /// `NativeMessagingHosts` leaf.
    pub root: String,
    pub family: Family,
}

impl std::str::FromStr for ExtraRoot {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // Split on the FIRST colon, family first. A family name never
        // contains one, while a registry root is whatever the browser chose
        // and may; putting the root second means it is never the thing that
        // has to be escaped.
        let (family, root) = text.split_once(':').ok_or_else(|| {
            format!("expected `family:root`, as in `chromium:Thorium`, not {text:?}")
        })?;

        let root = root.trim();
        if root.is_empty() {
            return Err(format!("no registry root in {text:?}"));
        }
        // A leading or trailing separator produces a key path with an empty
        // component, which silently does not match what the browser reads.
        if root.starts_with('\\') || root.ends_with('\\') {
            return Err(format!("the root must not start or end with a backslash: {root:?}"));
        }

        Ok(Self { root: root.to_owned(), family: family.trim().parse()? })
    }
}

impl std::fmt::Display for ExtraRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let family = match self.family {
            Family::Chromium => "chromium",
            Family::Gecko => "gecko",
        };
        write!(f, "{family}:{}", self.root)
    }
}

/// Where the roots registered by hand are remembered.
///
/// Uninstall walks the built-in table, which by definition does not contain
/// these -- so without a record a browser registered with `--extra-root`
/// keeps a key pointing at a host that uninstall has just deleted, and the
/// browser reports nothing when it fails to start it. The user cannot name
/// them again at uninstall either: the NSIS uninstaller runs
/// `--uninstall-host` with no arguments.
///
/// Plain text, one `family:root` per line, in the same syntax the flag
/// takes -- so the file is readable, and parsing it is parsing the flag.
fn extra_roots_path(dir: &Path) -> PathBuf {
    dir.join("extra-roots.txt")
}

fn read_extra_roots(dir: &Path) -> Vec<ExtraRoot> {
    let Ok(text) = std::fs::read_to_string(extra_roots_path(dir)) else {
        return Vec::new();
    };
    // A line that no longer parses is skipped rather than fatal: this is a
    // cleanup aid, and refusing to uninstall over a corrupt line in it
    // would be a worse failure than forgetting one root.
    text.lines().filter_map(|line| line.trim().parse().ok()).collect()
}

fn write_extra_roots(dir: &Path, roots: &[ExtraRoot]) -> std::io::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let body: String = roots
        .iter()
        .map(|root| {
            format!(
                "{root}
"
            )
        })
        .collect();
    std::fs::write(extra_roots_path(dir), body)
}

/// Who may launch the host. Enforced by the browser, not by us.
#[derive(Debug, Clone)]
pub struct AllowedCallers {
    /// Chromium extension ids (32 lowercase letters).
    pub chromium_ids: Vec<String>,
    /// Gecko extension ids, usually an email-like string.
    pub gecko_ids: Vec<String>,
}

impl Default for AllowedCallers {
    fn default() -> Self {
        Self { chromium_ids: Vec::new(), gecko_ids: vec!["idm@divine.local".to_owned()] }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry: {0}")]
    Registry(String),
    #[error("no extension ids were given, which would let any extension launch the host")]
    NoCallers,
    /// Only reachable from the non-Windows stub, which exists so the logic
    /// above stays compilable and testable off Windows.
    #[cfg_attr(windows, allow(dead_code))]
    #[error("this installer is Windows-only")]
    Unsupported,
}

/// Builds the manifest JSON for one family.
pub fn manifest_json(
    family: Family,
    host_path: &Path,
    callers: &AllowedCallers,
) -> Result<String, InstallError> {
    // An empty list would be a manifest that any extension could use, which
    // removes the browser-side restriction entirely.
    let entries: Vec<String> = match family {
        Family::Chromium => {
            callers.chromium_ids.iter().map(|id| format!("\"chrome-extension://{id}/\"")).collect()
        }
        Family::Gecko => callers.gecko_ids.iter().map(|id| format!("\"{id}\"")).collect(),
    };
    if entries.is_empty() {
        return Err(InstallError::NoCallers);
    }

    let key = match family {
        Family::Chromium => "allowed_origins",
        Family::Gecko => "allowed_extensions",
    };
    // JSON needs the backslashes in a Windows path escaped.
    let escaped = host_path.display().to_string().replace('\\', "\\\\");
    Ok(format!(
        "{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"Divine IDM native messaging host\",\n  \"path\": \"{escaped}\",\n  \"type\": \"stdio\",\n  \"{key}\": [{}]\n}}\n",
        entries.join(", ")
    ))
}

/// Where the generated manifests are written.
pub fn manifest_dir() -> PathBuf {
    idm_ipc::endpoint_path().with_file_name("hosts")
}

pub fn manifest_path(family: Family) -> PathBuf {
    let leaf = match family {
        Family::Chromium => "com.divine.idm.chromium.json",
        Family::Gecko => "com.divine.idm.gecko.json",
    };
    manifest_dir().join(leaf)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub label: String,
    pub detected: bool,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub registered: Vec<Registered>,
    pub skipped: Vec<String>,
    pub failures: Vec<(String, String)>,
}

/// Writes the manifests and registers them with every browser found.
///
/// `all` registers with every known browser rather than only the installed
/// ones, so a browser installed later still finds the host.
pub fn install(
    host_path: &Path,
    callers: &AllowedCallers,
    all: bool,
    extra_roots: &[ExtraRoot],
) -> Result<InstallReport, InstallError> {
    if !host_path.is_file() {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("host binary not found at {}", host_path.display()),
        )));
    }

    std::fs::create_dir_all(manifest_dir())?;
    for family in [Family::Chromium, Family::Gecko] {
        let json = manifest_json(family, host_path, callers)?;
        std::fs::write(manifest_path(family), json)?;
    }

    // Merged, not replaced: a second install naming different roots must
    // not orphan the ones the first registered.
    let mut remembered = read_extra_roots(&manifest_dir());
    for extra in extra_roots {
        if !remembered.contains(extra) {
            remembered.push(extra.clone());
        }
    }
    write_extra_roots(&manifest_dir(), &remembered)?;

    let mut report = InstallReport::default();
    let extras: Vec<Browser> = extra_roots
        .iter()
        .map(|extra| Browser {
            // Leaked deliberately: the table is static and these live for the
            // process, which ends moments later.
            label: Box::leak(extra.root.clone().into_boxed_str()),
            root: Box::leak(extra.root.clone().into_boxed_str()),
            family: extra.family,
        })
        .collect();

    let (targets, skipped) = registration_targets(all, &extras, &browser_present);
    report.skipped = skipped;

    for (browser, detected) in targets {
        match platform::register(browser.root, &manifest_path(browser.family)) {
            Ok(()) => {
                report.registered.push(Registered { label: browser.label.to_owned(), detected });
            }
            Err(error) => {
                report.failures.push((browser.label.to_owned(), error.to_string()));
            }
        }
    }
    Ok(report)
}

/// Which browsers a given set of options will be registered with.
///
/// Separated from [`install`] because it is the part worth testing and the
/// only part that can be. `install` writes to every browser on the machine,
/// so a test that called it would rewrite the registration of whoever ran
/// the suite; this takes detection as an argument instead and decides
/// nothing by itself.
///
/// Returns the browsers to register, each with whether it looked installed,
/// and the labels of those skipped.
fn registration_targets<'a>(
    all: bool,
    extras: &'a [Browser],
    present: &dyn Fn(&str) -> bool,
) -> (Vec<(&'a Browser, bool)>, Vec<String>) {
    let mut targets = Vec::new();
    let mut skipped = Vec::new();

    // `forced` is what makes --extra-root worth having. Detection asks
    // whether a browser wrote anything under its own key, and a browser the
    // table does not know is usually one sitting somewhere unusual -- so it
    // would be skipped as "not installed", and the flag the user reached for
    // precisely because the table was wrong would do nothing unless they
    // also passed --all-browsers. Naming a root IS the instruction.
    let listed = BROWSERS.iter().map(|browser| (browser, false));
    let named = extras.iter().map(|browser| (browser, true));

    for (browser, forced) in listed.chain(named) {
        // Someone naming a root the table already holds does not know it is
        // in the table -- that is why they named it. Without this the table
        // entry is skipped as absent while the named one is registered, and
        // the same browser is reported both ways in one run.
        if !forced && extras.iter().any(|extra| extra.root == browser.root) {
            continue;
        }

        let detected = present(browser.root);
        if !detected && !all && !forced {
            skipped.push(browser.label.to_owned());
            continue;
        }
        targets.push((browser, detected));
    }

    (targets, skipped)
}

/// Removes the host registration from every known browser.
pub fn uninstall() -> InstallReport {
    let mut report = InstallReport::default();

    // The table plus whatever was registered by hand. Anything missed here
    // is a key left pointing at a host that is about to be deleted.
    // Unregistering needs the root and a name to report it by; the family
    // decided which manifest a browser was given, and that manifest is
    // being deleted either way, so it plays no part here.
    let remembered = read_extra_roots(&manifest_dir());
    let listed = BROWSERS.iter().map(|browser| (browser.label, browser.root));
    let named = remembered.iter().map(|extra| (extra.root.as_str(), extra.root.as_str()));

    for (label, root) in listed.chain(named) {
        match platform::unregister(root) {
            Ok(true) => {
                report.registered.push(Registered { label: label.to_owned(), detected: true });
            }
            Ok(false) => report.skipped.push(label.to_owned()),
            Err(error) => report.failures.push((label.to_owned(), error.to_string())),
        }
    }
    let _ = std::fs::remove_file(manifest_path(Family::Chromium));
    let _ = std::fs::remove_file(manifest_path(Family::Gecko));
    let _ = std::fs::remove_file(extra_roots_path(&manifest_dir()));
    report
}

#[cfg(windows)]
mod platform {
    use super::InstallError;
    use std::path::Path;

    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Points the browser at our manifest. Always HKCU: registering per-user
    /// needs no elevation and is the right scope for a per-user tool.
    pub(super) fn register(root: &str, manifest: &Path) -> Result<(), InstallError> {
        let path = wide(&format!(r"Software\{root}\NativeMessagingHosts\{}", super::HOST_NAME));
        let mut key = HKEY::default();
        // SAFETY: NUL-terminated inputs; `key` is a valid out-pointer.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &raw mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(InstallError::Registry(format!("RegCreateKeyEx failed: {status:?}")));
        }

        // The default value holds the manifest path.
        let value = wide(&manifest.display().to_string());
        let bytes =
            unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) };
        // SAFETY: `bytes` borrows `value`, which outlives the call, and its
        // length matches exactly.
        let status = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };
        // SAFETY: created above, not used after this.
        unsafe {
            let _ = RegCloseKey(key);
        }
        if status != ERROR_SUCCESS {
            return Err(InstallError::Registry(format!("RegSetValueEx failed: {status:?}")));
        }
        Ok(())
    }

    /// Returns whether anything was actually removed.
    /// Removes a whole browser root. Tests only: a real uninstall leaves
    /// the browser's own keys alone and touches only our registration.
    #[cfg(test)]
    pub(super) fn delete_root(root: &str) {
        let path = wide(&format!(r"Software\{root}"));
        // SAFETY: `path` is a NUL-terminated wide string outliving the call.
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()));
        }
    }

    pub(super) fn unregister(root: &str) -> Result<bool, InstallError> {
        let path = wide(&format!(r"Software\{root}\NativeMessagingHosts\{}", super::HOST_NAME));
        // SAFETY: NUL-terminated wide string outliving the call.
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        Ok(status == ERROR_SUCCESS)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::InstallError;
    use std::path::Path;

    pub(super) fn register(_root: &str, _manifest: &Path) -> Result<(), InstallError> {
        Err(InstallError::Unsupported)
    }

    pub(super) fn unregister(_root: &str) -> Result<bool, InstallError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer must register the id the extension actually has.
    ///
    /// Two files have to agree: the keypair under `extension/keys/` fixes
    /// the id Chromium derives, and `installer-hooks.nsh` passes an id to
    /// `--install-host` during installation. Running `keygen.mjs --force`
    /// changes the first and not the second.
    ///
    /// That drift fails silently in the worst way. The install succeeds, the
    /// registry key is written, the manifest looks right -- and the browser
    /// refuses to launch the host, because the extension presenting itself
    /// is not the one `allowed_origins` names. Nothing logs it, and the only
    /// symptom is that downloads quietly keep going to the browser.
    ///
    /// `make-installer.ps1` checks this too, but only when an installer is
    /// being built. Here it is checked whenever anyone runs the tests.
    #[test]
    fn the_installer_registers_the_id_the_extension_actually_has() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        let pinned = std::fs::read_to_string(root.join("extension/keys/chromium.pub.txt"))
            .expect("extension/keys/chromium.pub.txt is missing; run node extension/keygen.mjs");
        let pinned = pinned.lines().next().unwrap_or_default().trim().to_owned();
        assert_eq!(pinned.len(), 32, "a Chromium id is 32 characters: {pinned:?}");

        let hooks = std::fs::read_to_string(root.join("app/src-tauri/installer-hooks.nsh"))
            .expect("app/src-tauri/installer-hooks.nsh is missing");
        let declared = hooks
            .split_once("!define IDM_EXTENSION_ID")
            .and_then(|(_, rest)| rest.split('"').nth(1))
            .expect("installer-hooks.nsh does not define IDM_EXTENSION_ID");

        assert_eq!(
            declared, pinned,
            "the installer would register {declared}, but the extension's id is {pinned}.              Update IDM_EXTENSION_ID in app/src-tauri/installer-hooks.nsh."
        );
    }

    fn callers() -> AllowedCallers {
        AllowedCallers {
            chromium_ids: vec!["abcdefghijklmnopabcdefghijklmnop".into()],
            gecko_ids: vec!["idm@divine.local".into()],
        }
    }

    #[test]
    fn the_two_families_use_their_own_manifest_keys() {
        let path = Path::new(r"C:\Program Files\DivineIDM\idm-host.exe");

        let chromium = manifest_json(Family::Chromium, path, &callers()).unwrap();
        assert!(chromium.contains("allowed_origins"), "{chromium}");
        assert!(chromium.contains("chrome-extension://abcdefghijklmnopabcdefghijklmnop/"));
        assert!(!chromium.contains("allowed_extensions"));

        let gecko = manifest_json(Family::Gecko, path, &callers()).unwrap();
        assert!(gecko.contains("allowed_extensions"), "{gecko}");
        assert!(gecko.contains("idm@divine.local"));
        assert!(!gecko.contains("allowed_origins"));
    }

    #[test]
    fn a_manifest_with_no_permitted_callers_is_refused() {
        // An empty list means any extension could launch the host, which
        // removes the browser-side restriction entirely.
        let empty = AllowedCallers { chromium_ids: Vec::new(), gecko_ids: Vec::new() };
        let path = Path::new(r"C:\x\idm-host.exe");
        assert!(matches!(
            manifest_json(Family::Chromium, path, &empty),
            Err(InstallError::NoCallers)
        ));
        assert!(matches!(manifest_json(Family::Gecko, path, &empty), Err(InstallError::NoCallers)));
    }

    #[test]
    fn windows_paths_are_escaped_for_json() {
        let path = Path::new(r"C:\Program Files\DivineIDM\idm-host.exe");
        let json = manifest_json(Family::Chromium, path, &callers()).unwrap();
        assert!(json.contains(r"C:\\Program Files\\DivineIDM\\idm-host.exe"), "{json}");
        // And the result must actually parse.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["path"], r"C:\Program Files\DivineIDM\idm-host.exe");
        assert_eq!(parsed["type"], "stdio");
        assert_eq!(parsed["name"], HOST_NAME);
    }

    #[test]
    fn every_known_browser_has_a_distinct_registry_root() {
        // A duplicate root would silently mean one browser overwrites
        // another's registration.
        let mut roots: Vec<&str> = BROWSERS.iter().map(|b| b.root).collect();
        let before = roots.len();
        roots.sort_unstable();
        roots.dedup();
        assert_eq!(before, roots.len(), "duplicate registry roots in the browser table");
    }

    #[test]
    fn the_table_covers_both_families_broadly() {
        let chromium = BROWSERS.iter().filter(|b| b.family == Family::Chromium).count();
        let gecko = BROWSERS.iter().filter(|b| b.family == Family::Gecko).count();
        // The point of the table is breadth; a handful of each is the minimum
        // that makes "any browser" a fair claim.
        assert!(chromium >= 10, "only {chromium} Chromium browsers listed");
        assert!(gecko >= 3, "only {gecko} Gecko browsers listed");
    }

    #[test]
    fn manifests_for_the_two_families_do_not_collide() {
        assert_ne!(manifest_path(Family::Chromium), manifest_path(Family::Gecko));
    }

    fn phantom(root: &str, family: Family) -> Browser {
        Browser {
            label: Box::leak(root.to_owned().into_boxed_str()),
            root: Box::leak(root.to_owned().into_boxed_str()),
            family,
        }
    }

    /// Nothing is installed, so only what the user named is registered.
    fn nothing_present(_root: &str) -> bool {
        false
    }

    #[test]
    fn a_named_root_is_registered_even_though_it_looks_absent() {
        // The whole point of --extra-root. A browser the table does not
        // know is usually one in an unusual place, so detection will say it
        // is not there -- and if that skipped it, the flag someone reached
        // for because the table was wrong would do nothing at all.
        let extras = [phantom(r"Thorium", Family::Chromium)];
        let (targets, skipped) = registration_targets(false, &extras, &nothing_present);

        let registered: Vec<&str> = targets.iter().map(|(b, _)| b.root).collect();
        assert_eq!(registered, vec!["Thorium"], "only the named root should be registered");
        assert!(!skipped.contains(&"Thorium".to_owned()), "a named root must never be skipped");
        // Everything in the table was absent, so all of it was skipped.
        // Thorium is itself in the table, so the named entry supersedes it
        // rather than the two contradicting each other.
        assert_eq!(skipped.len(), BROWSERS.len() - 1);
    }

    #[test]
    fn the_record_of_named_roots_survives_a_round_trip() {
        // Uninstall reads this to find the roots the table does not list.
        // A round trip that loses the family would unregister nothing,
        // because the root would no longer parse.
        let dir = tempfile::tempdir().expect("temp dir");
        let roots = vec![
            ExtraRoot { root: "Thorium".into(), family: Family::Chromium },
            ExtraRoot { root: r"Some Vendor\Fork".into(), family: Family::Gecko },
        ];

        write_extra_roots(dir.path(), &roots).expect("write");
        assert_eq!(read_extra_roots(dir.path()), roots);
    }

    #[test]
    fn no_record_at_all_reads_as_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(read_extra_roots(dir.path()).is_empty());
    }

    #[test]
    fn a_damaged_line_in_the_record_does_not_block_the_rest() {
        // This file only exists to help uninstall clean up. Refusing to
        // uninstall over one unreadable line would be a worse failure than
        // forgetting the root it named.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            extra_roots_path(dir.path()),
            "chromium:Thorium\nnonsense\n\ngecko:Waterfox\n",
        )
        .expect("write");

        let read = read_extra_roots(dir.path());
        assert_eq!(read.len(), 2, "{read:?}");
        assert_eq!(read[0].root, "Thorium");
        assert_eq!(read[1].family, Family::Gecko);
    }

    #[test]
    fn naming_a_root_the_table_already_holds_is_not_reported_both_ways() {
        // Someone naming Thorium does not know it is already listed; that
        // is why they named it. Reporting it as both skipped and registered
        // in one run is the kind of output that makes people re-run things.
        let extras = [phantom("Thorium", Family::Chromium)];
        let (targets, skipped) = registration_targets(false, &extras, &nothing_present);

        let named: Vec<&str> =
            targets.iter().map(|(b, _)| b.root).filter(|r| *r == "Thorium").collect();
        assert_eq!(named, vec!["Thorium"], "registered exactly once");
        assert!(!skipped.contains(&"Thorium".to_owned()), "and not also skipped");
    }

    #[test]
    fn a_named_root_is_reported_as_not_detected() {
        // It is registered, but saying "installed" about something we only
        // took the user's word for would be a lie in the output.
        let extras = [phantom(r"Thorium", Family::Chromium)];
        let (targets, _) = registration_targets(false, &extras, &nothing_present);
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].1, "detection, not the fact that it was named");
    }

    #[test]
    fn named_roots_are_in_addition_to_the_table_not_instead_of_it() {
        let extras = [phantom("UnlistedFork", Family::Chromium)];
        let (targets, skipped) = registration_targets(true, &extras, &nothing_present);
        assert_eq!(targets.len(), BROWSERS.len() + 1);
        assert!(skipped.is_empty(), "--all-browsers skips nothing");
    }

    #[test]
    fn a_named_root_keeps_the_family_it_was_given() {
        // The family picks the manifest dialect. A Gecko browser handed the
        // Chromium manifest is registered, looks fine, and never starts the
        // host: allowed_origins means nothing to it.
        let extras = [phantom(r"SomeFork", Family::Gecko)];
        let (targets, _) = registration_targets(false, &extras, &nothing_present);
        assert_eq!(targets[0].0.family, Family::Gecko);
        assert_eq!(manifest_path(targets[0].0.family), manifest_path(Family::Gecko));
    }

    #[test]
    fn an_extra_root_is_written_family_first() {
        let parsed: ExtraRoot = "chromium:Thorium".parse().unwrap();
        assert_eq!(parsed, ExtraRoot { root: "Thorium".into(), family: Family::Chromium });

        // A root may contain separators; the split is on the first colon.
        let nested: ExtraRoot = r"gecko:Some Vendor\Their Browser".parse().unwrap();
        assert_eq!(nested.root, r"Some Vendor\Their Browser");
        assert_eq!(nested.family, Family::Gecko);
    }

    #[test]
    fn the_families_answer_to_the_words_people_use() {
        for text in ["chromium", "Chromium", "CHROME", "blink"] {
            assert_eq!(text.parse::<Family>().unwrap(), Family::Chromium, "{text}");
        }
        for text in ["gecko", "Firefox", "mozilla"] {
            assert_eq!(text.parse::<Family>().unwrap(), Family::Gecko, "{text}");
        }
    }

    #[test]
    fn a_malformed_extra_root_is_refused_with_the_shape_in_the_message() {
        // Each of these would otherwise produce a registry path that is
        // silently not the one the browser reads.
        for bad in [
            "Thorium",
            "chromium:",
            "chromium:   ",
            "opera:Opera",
            r"chromium:\Thorium",
            r"chromium:Thorium\",
        ] {
            let error = bad.parse::<ExtraRoot>().unwrap_err();
            assert!(!error.is_empty(), "{bad}");
        }

        // The common mistake is writing it the other way round, so the
        // message has to show the shape rather than just refusing.
        let error = "Thorium:chromium".parse::<ExtraRoot>().unwrap_err();
        assert!(error.contains("chromium"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn browser_detection_answers_without_panicking() {
        // Either answer is legitimate on any given machine; what matters is
        // that probing an absent browser is not an error.
        let present: Vec<&str> =
            BROWSERS.iter().filter(|b| browser_present(b.root)).map(|b| b.label).collect();
        println!("detected browsers: {present:?}");
        assert!(!browser_present(r"Nonexistent\Browser\XYZZY"));
    }

    #[cfg(windows)]
    #[test]
    fn registering_a_host_does_not_make_a_browser_look_installed() {
        // The bug this guards for: `register` creates
        // Software\<root>\NativeMessagingHosts, and so creates
        // Software\<root>. A detector that asks only whether that key
        // exists is reading evidence it planted itself, and reports every
        // browser it has ever registered as installed forever after. Found on
        // a machine where HKCU\Software\Vivaldi existed, held nothing but
        // NativeMessagingHosts, and no Vivaldi had ever been installed.
        const ROOT: &str = r"DivineIDMTest\PhantomBrowser";

        let dir = tempfile::tempdir().expect("temp dir");
        let manifest = dir.path().join("host.json");
        std::fs::write(&manifest, "{}").expect("write manifest");

        platform::delete_root(ROOT);
        assert!(!browser_present(ROOT), "absent before the test writes anything");

        platform::register(ROOT, &manifest).expect("register");
        let after_our_own_registration = browser_present(ROOT);

        // Stands in for a key the browser itself writes on first run.
        platform::register(&format!(r"{ROOT}\BLBeacon"), &manifest).expect("evidence");
        let with_browser_written_content = browser_present(ROOT);

        platform::delete_root(ROOT);

        assert!(!after_our_own_registration, "our own registration is not evidence");
        assert!(with_browser_written_content, "a key the browser wrote is evidence");
    }
}
