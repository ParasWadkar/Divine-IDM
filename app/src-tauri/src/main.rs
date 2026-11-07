//! DivinePulse, the desktop application.
//!
//! The engine runs **in this process**, not as a daemon the window talks to.
//! Closing the window hides to the tray and downloads carry on; quitting from
//! the tray is the only thing that stops them. A daemon would double the IPC
//! surface and give the user two things to keep alive instead of one.
//!
//! The window reaches the engine through exactly the same [`Request`] surface
//! the CLI and the browser extension use, as a client named `desktop`. That
//! is deliberate: pairing, the browser kill switch and every other refusal
//! are enforced in one place, so the GUI cannot become a second door into
//! the engine that skips a check the others make.

// Without this a release build pops a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

use idm_ipc::{IpcServer, JobId, Request, Response, Settings};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};

mod autostart;
mod batch;
mod fileicon;
mod opened;
mod state;
mod verify;
use state::AppState;

/// How often the window is sent a fresh snapshot.
///
/// Fast enough that the speed graph moves smoothly, slow enough that an idle
/// app is not waking the webview constantly. The engine's own progress
/// callback fires far more often than this; the UI does not need every one.
const TICK: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Everything the window needs to draw itself, in one round trip.
///
/// One command rather than several because the parts are drawn together: a
/// job list from one instant and a settings block from another can disagree,
/// and the flicker that causes is avoidable by not splitting them.
#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    state.snapshot().await
}

#[tauri::command]
async fn add_download(
    state: State<'_, AppState>,
    url: String,
    filename: Option<String>,
    mirrors: Option<Vec<String>>,
    queue: Option<i64>,
) -> Result<usize, String> {
    let addresses = batch::expand(url.trim())?;
    let mirrors: Vec<String> = mirrors
        .unwrap_or_default()
        .into_iter()
        .map(|mirror| mirror.trim().to_owned())
        .filter(|mirror| !mirror.is_empty())
        .collect();

    batch::mirrors_allowed(addresses.len(), &mirrors)?;

    // A name was chosen for one file, so it cannot be given to all of
    // them; past the first the server's own name is the only one left.
    let mut queued = 0;
    let mut failed: Option<String> = None;
    for (index, address) in addresses.iter().enumerate() {
        let name = if index == 0 { filename.clone() } else { None };
        let mirrors = if index == 0 { mirrors.clone() } else { Vec::new() };
        match state.add_download(address.clone(), name, mirrors, queue).await {
            Ok(_) => queued += 1,
            Err(error) => failed = failed.or(Some(error)),
        }
    }

    // One address that failed is the whole request failing. Several, and
    // what matters is how many started, so the rest are not silently lost
    // behind the first one's reason.
    match failed {
        Some(error) if queued == 0 => Err(error),
        _ => Ok(queued),
    }
}

/// The named queues, with what each is running right now.
#[tauri::command]
async fn queues(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    queue_reply(state.request(Request::ListQueues).await)
}

/// Collects the links on a page.
///
/// `follow` is the string `"this_page"` or `"and_what_it_links_to"`,
/// matching the two values of the request's enum. A string rather than a
/// number for the same reason the enum is not an integer: there is to be
/// nothing here that a careless change could turn into a depth of three.
#[tauri::command]
async fn grab_links(
    state: State<'_, AppState>,
    url: String,
    follow: Option<String>,
) -> Result<serde_json::Value, String> {
    let follow = match follow.as_deref() {
        Some("and_what_it_links_to") => idm_ipc::GrabDepth::AndWhatItLinksTo,
        _ => idm_ipc::GrabDepth::ThisPage,
    };
    match state.request(Request::GrabLinks { url, follow }).await {
        Response::Grabbed { links, pages, note } => Ok(serde_json::json!({
            "links": links,
            "pages": pages,
            "note": note,
        })),
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// The address to open the web interface at, or nothing when it is off.
///
/// The key is in it, which is the point: it is shown on the user's own
/// screen so they can type or scan it into a phone, and the page takes it
/// out of the address bar as soon as it loads.
#[tauri::command]
async fn web_address(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let settings = match state.request(Request::GetSettings).await {
        Response::Settings(settings) => settings,
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    if !settings.web_interface {
        return Ok(None);
    }

    let vault = idm_core::win::credentials::WindowsVault;
    let token = idm_engine::web::token(&vault)?;
    Ok(Some(idm_engine::web::where_to_open(settings.web_port_or_default(), token.as_str())))
}

/// The secret aria2-compatible clients must present, or nothing when the
/// endpoint is off.
///
/// Shown on the user's own screen so they can paste it into a client's
/// "RPC secret" box. Unlike the web interface's there is no address to go
/// with it: the endpoint is on loopback at a port the user can already see.
#[tauri::command]
async fn rpc_secret(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let settings = match state.request(Request::GetSettings).await {
        Response::Settings(settings) => settings,
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    if !settings.rpc_interface {
        return Ok(None);
    }

    let vault = idm_core::win::credentials::WindowsVault;
    Ok(Some(idm_engine::rpc::secret(&vault)?))
}

/// Writes everything still to download out to a file the user picks.
///
/// The list the engine builds is aria2's input-file format, so what comes
/// out is a file `aria2c -i` will fetch as well as one this reads back.
/// Returns how many downloads it describes, or nothing if the save dialog
/// was dismissed.
#[tauri::command]
async fn export_list(app: AppHandle, state: State<'_, AppState>) -> Result<Option<usize>, String> {
    use tauri_plugin_dialog::DialogExt;

    let (text, items) = match state.request(Request::ExportList).await {
        Response::Listed { text, items } => (text, items),
        Response::Rejected { reason } => return Err(reason),
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    if items == 0 {
        return Err("there is nothing waiting to download".into());
    }

    let mut dialog = app
        .dialog()
        .file()
        .set_title("Save the download list")
        .set_file_name("downloads.txt")
        .add_filter("Download list", &["txt"]);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }

    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.save_file(move |picked| {
        let _ = sender.send(picked);
    });
    let Some(path) = receiver.await.ok().flatten().and_then(|path| path.into_path().ok()) else {
        return Ok(None);
    };

    std::fs::write(&path, text).map_err(|error| format!("could not write the list: {error}"))?;
    Ok(Some(items))
}

/// Reads a list of downloads out of a file the user picks, and queues it.
///
/// The file is read here rather than by the engine, deliberately: the
/// engine takes the text, never a path. A request carrying a path would be
/// a request that reads a file of someone else's choosing, and the browser
/// side speaks the same protocol.
#[tauri::command]
async fn import_list(
    app: AppHandle,
    state: State<'_, AppState>,
    queue: Option<i64>,
) -> Result<Option<serde_json::Value>, String> {
    use tauri_plugin_dialog::DialogExt;

    let mut dialog = app
        .dialog()
        .file()
        .set_title("Open a list of downloads")
        .add_filter("Download lists", &["txt", "ef2", "lst"]);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }

    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.pick_file(move |picked| {
        let _ = sender.send(picked);
    });
    let Some(path) = receiver.await.ok().flatten().and_then(|path| path.into_path().ok()) else {
        return Ok(None);
    };

    // Capped before it is read into memory. A list is a file, and a file
    // can be a gigabyte of anything.
    const LARGEST_LIST_BYTES: u64 = 8 << 20;
    match std::fs::metadata(&path) {
        Ok(data) if data.len() > LARGEST_LIST_BYTES => {
            return Err("that file is too large to be a list of downloads".into());
        }
        Ok(_) => {}
        Err(error) => return Err(format!("could not open that file: {error}")),
    }

    let text = std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| format!("could not read that file: {error}"))?;
    let name = path.file_name().map(|part| part.to_string_lossy().into_owned()).unwrap_or_default();

    match state.request(Request::ImportList { name, text, queue }).await {
        Response::Imported { added, skipped } => {
            Ok(Some(serde_json::json!({ "added": added, "skipped": skipped })))
        }
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Queues a list of addresses that were picked out of a page.
///
/// Separate from `add_download` rather than called in a loop, and not
/// because of the round trips: `add_download` expands `[1-9]` into nine
/// downloads, and these addresses came off somebody else's page. A real
/// link with brackets in it would quietly become nine requests for files
/// that do not exist.
#[tauri::command]
async fn add_links(
    state: State<'_, AppState>,
    urls: Vec<String>,
    queue: Option<i64>,
) -> Result<usize, String> {
    if urls.len() > idm_engine::grab::MOST_LINKS {
        return Err(format!("at most {} at once", idm_engine::grab::MOST_LINKS));
    }

    let mut queued = 0;
    let mut failed: Option<String> = None;
    for url in urls {
        match state.add_download(url, None, Vec::new(), queue).await {
            Ok(_) => queued += 1,
            Err(error) => failed = failed.or(Some(error)),
        }
    }
    // One that failed out of many is not the request failing: the rest are
    // downloading, and saying so is more useful than an error that hides
    // what worked.
    match failed {
        Some(error) if queued == 0 => Err(error),
        _ => Ok(queued),
    }
}

/// Lists what is inside an archive, reading only its index.
#[tauri::command]
async fn peek_archive(
    state: State<'_, AppState>,
    url: String,
) -> Result<serde_json::Value, String> {
    match state.request(Request::PeekArchive { url }).await {
        Response::Archive { entries, truncated, total_uncompressed } => Ok(serde_json::json!({
            "entries": entries,
            "truncated": truncated,
            "total_uncompressed": total_uncompressed,
        })),
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// The bandwidth schedule, and what it is doing right now.
#[tauri::command]
async fn bandwidth_rules(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    schedule_reply(state.request(Request::ListBandwidthRules).await)
}

/// Adds a rule or replaces one, and returns the schedule as it now stands.
#[tauri::command]
async fn set_bandwidth_rule(
    state: State<'_, AppState>,
    id: Option<i64>,
    days: u8,
    start_minute: u16,
    end_minute: u16,
    bytes_per_sec: u64,
    stop: bool,
) -> Result<serde_json::Value, String> {
    schedule_reply(
        state
            .request(Request::SetBandwidthRule {
                id,
                days,
                start_minute,
                end_minute,
                bytes_per_sec,
                stop,
            })
            .await,
    )
}

#[tauri::command]
async fn forget_bandwidth_rule(
    state: State<'_, AppState>,
    id: i64,
) -> Result<serde_json::Value, String> {
    schedule_reply(state.request(Request::RemoveBandwidthRule { id }).await)
}

/// The rules and the line about what is in force, as one reply.
///
/// Together because they are answered together: a rule that has just
/// changed usually changes what is in force, and asking twice would show
/// the window a list and a verdict from two different instants.
fn schedule_reply(response: Response) -> Result<serde_json::Value, String> {
    match response {
        Response::BandwidthRules { rules, now } => Ok(serde_json::json!({
            "rules": rules,
            "now": now,
        })),
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Which sites and proxies have a password saved. Never the passwords.
#[tauri::command]
async fn credentials(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    match state.request(Request::ListCredentials).await {
        Response::Credentials { entries } => {
            serde_json::to_value(entries).map_err(|error| error.to_string())
        }
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Saves a password in Windows' own vault, replacing any for the same thing.
///
/// The reply is the fresh list, because that is the only visible effect and
/// asking for it separately would show a list from another instant.
#[tauri::command]
async fn save_credential(
    state: State<'_, AppState>,
    scope: idm_ipc::CredentialScope,
    username: String,
    password: String,
) -> Result<serde_json::Value, String> {
    state
        .simple(Request::SetCredential {
            scope,
            username,
            password: idm_ipc::SecretString::new(password),
        })
        .await?;
    credentials(state).await
}

#[tauri::command]
async fn forget_credential(
    state: State<'_, AppState>,
    scope: idm_ipc::CredentialScope,
) -> Result<serde_json::Value, String> {
    state.simple(Request::RemoveCredential { scope }).await?;
    credentials(state).await
}

/// The external programs the app can run, and whether each is here.
#[tauri::command]
async fn sidecars(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    match state.request(Request::ListSidecars).await {
        Response::Sidecars { tools } => {
            serde_json::to_value(tools).map_err(|error| error.to_string())
        }
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Fetches one of them, checked against its pinned hash, and installs it.
///
/// Slow by nature -- a hundred megabytes -- and awaited rather than fired
/// off, so the button that started it can stay busy until it is actually
/// done and then say what happened. The reply is the fresh list, because
/// the whole point of the call is that one line of it changed.
#[tauri::command]
async fn fetch_sidecar(
    state: State<'_, AppState>,
    name: String,
) -> Result<serde_json::Value, String> {
    state.simple(Request::FetchSidecar { name }).await?;
    sidecars(state).await
}

/// Creates a queue, or changes how many a queue runs at once.
///
/// Returns the queues as they now stand, because the engine answers these
/// with the whole list: the counts on every queue can change when one of
/// them is touched, and asking again in a second round trip would show the
/// window a list from a different instant.
#[tauri::command]
async fn set_queue(
    state: State<'_, AppState>,
    name: String,
    max_concurrent: u32,
) -> Result<serde_json::Value, String> {
    queue_reply(state.request(Request::SetQueue { name, max_concurrent }).await)
}

#[tauri::command]
async fn enable_queue(
    state: State<'_, AppState>,
    id: i64,
    enabled: bool,
) -> Result<serde_json::Value, String> {
    queue_reply(state.request(Request::EnableQueue { id, enabled }).await)
}

#[tauri::command]
async fn remove_queue(state: State<'_, AppState>, id: i64) -> Result<serde_json::Value, String> {
    queue_reply(state.request(Request::RemoveQueue { id }).await)
}

/// Moves a download into a queue, or out of one when `queue` is absent.
#[tauri::command]
async fn assign_queue(
    state: State<'_, AppState>,
    id: u64,
    queue: Option<i64>,
) -> Result<serde_json::Value, String> {
    queue_reply(state.request(Request::AssignQueue { id: JobId(id), queue }).await)
}

fn queue_reply(response: Response) -> Result<serde_json::Value, String> {
    match response {
        Response::Queues { queues } => {
            serde_json::to_value(queues).map_err(|error| error.to_string())
        }
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

#[tauri::command]
async fn resume_job(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    state.simple(Request::Resume { id: JobId(id) }).await
}

/// Starts a failed download again from the History tab.
///
/// `id` here is the History row's own id, not the session-local job id
/// `resume_job` takes — History outlives any single run of the engine, so
/// it has to be addressed by something that does too.
#[tauri::command]
async fn resume_from_history(
    state: State<'_, AppState>,
    id: i64,
    url: Option<String>,
) -> Result<(), String> {
    state.simple(Request::ResumeFromHistory { row_id: id, url }).await
}

/// Holds a download where it is, keeping every byte it already has.
#[tauri::command]
async fn pause_job(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    state.simple(Request::Pause { id: JobId(id) }).await
}

/// Points a download at a new address without losing what it has.
///
/// Headers are optional and separate, because a signed link and the cookie
/// that authorises it usually expire together: replacing only the address
/// earns the same refusal a moment later.
#[tauri::command]
async fn refresh_link(
    state: State<'_, AppState>,
    id: u64,
    url: String,
    referrer: Option<String>,
    cookies: Option<String>,
) -> Result<(), String> {
    state.simple(Request::RefreshUrl { id: JobId(id), url, referrer, cookies }).await
}

#[tauri::command]
async fn cancel_job(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    match state.request(Request::Cancel { id: JobId(id) }).await {
        Response::Rejected { reason } => Err(reason),
        Response::Error { message } => Err(message),
        _ => Ok(()),
    }
}

#[tauri::command]
async fn approve_pairing(state: State<'_, AppState>, request_id: String) -> Result<(), String> {
    state.simple(Request::ApprovePairing { request_id }).await
}

#[tauri::command]
async fn deny_pairing(state: State<'_, AppState>, request_id: String) -> Result<(), String> {
    state.simple(Request::DenyPairing { request_id }).await
}

#[tauri::command]
async fn set_browser_enabled(
    state: State<'_, AppState>,
    client_ids: Vec<String>,
    enabled: bool,
) -> Result<(), String> {
    // Every profile of the browser, since the window shows one line per
    // browser. Stops at the first refusal rather than leaving half of them
    // switched.
    for client_id in client_ids {
        state.simple(Request::SetBrowserEnabled { client_id, enabled }).await?;
    }
    Ok(())
}

/// The browser list: which browsers are here, and what each is doing.
///
/// Built from what is installed, what is paired and what the user has hidden,
/// by a function in the engine crate that has its own tests. This only gathers
/// the three inputs; keeping the logic out of here is what keeps it testable.
#[tauri::command]
async fn browsers(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let paired = match state.request(Request::ListPairings).await {
        Response::Pairings { paired, .. } => paired,
        Response::Error { message } => return Err(message),
        _ => Vec::new(),
    };
    let hidden = match state.request(Request::GetSettings).await {
        Response::Settings(settings) => settings.hidden_browsers,
        _ => Vec::new(),
    };
    let installed = idm_engine::browsers::installed();
    let rows = idm_engine::browsers::rows(&installed, &paired, &hidden);
    serde_json::to_value(rows).map_err(|error| error.to_string())
}

#[tauri::command]
async fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    // The registry is written only after the engine has accepted the
    // settings, so a refused change cannot leave the two disagreeing about
    // whether this app starts with Windows.
    let wanted = settings.start_with_windows;
    state.simple(Request::SetSettings(settings)).await?;
    autostart::apply(wanted)
}

/// Opens the system folder picker and returns the choice, or nothing if the
/// person cancelled.
///
/// Checks a finished download against a hash published by its source.
///
/// Takes the path rather than a job, because what is being checked is the
/// file on disk: a download finished days ago, or one whose row has since
/// been cleared from History, is exactly when this is worth asking.
#[tauri::command]
async fn verify_file(path: String, expected: String) -> Result<verify::Verdict, String> {
    // Reading a whole file is seconds for an installer and minutes for a
    // disc image, so it goes nowhere near the runtime's async workers.
    tokio::task::spawn_blocking(move || verify::check(std::path::Path::new(&path), &expected))
        .await
        .map_err(|error| error.to_string())?
}

/// Looks for a hash the download's publisher put somewhere findable.
///
/// `Ok(None)` rather than an error when nothing is found: that is the
/// ordinary outcome and says nothing went wrong, so the window falls back to
/// asking rather than having to decide which failures not to show. The
/// `Result` is Tauri's requirement for an async command borrowing state, not
/// a second channel for "no hash".
#[tauri::command]
async fn published_hash(
    state: State<'_, AppState>,
    url: String,
    filename: String,
    referrer: Option<String>,
) -> Result<Option<idm_engine::checksum::Published>, String> {
    let proxy = state.proxy(&url).await;
    Ok(idm_engine::checksum::published(&url, referrer.as_deref(), &filename, proxy).await)
}

/// The icon the shell shows for a kind of file, as raw pixels.
///
/// Asked once per extension and kept by the window, because the answer is
/// the same for every `.zip` it will ever list and the shell takes long
/// enough that doing it per row would be felt.
#[tauri::command]
async fn file_icon(extension: String) -> Result<fileicon::Icon, String> {
    // The shell call is blocking and talks to COM, so it does not belong on
    // the runtime's async workers.
    tokio::task::spawn_blocking(move || fileicon::for_extension(&extension))
        .await
        .map_err(|error| error.to_string())?
}

/// A command of our own rather than the dialog plugin's JavaScript API, so
/// the page is not granted the plugin: a window that can open arbitrary
/// dialogs is a larger permission than a window that can ask this one
/// question. It also keeps the capability list to the single thing the page
/// genuinely needs from the shell.
#[tauri::command]
async fn choose_folder(app: AppHandle, start: Option<String>) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;

    let mut dialog = app.dialog().file().set_title("Choose the download folder");
    // Opens where the current choice is, so changing it slightly -- the
    // usual reason to open it -- does not mean navigating from the top.
    if let Some(start) = start.filter(|path| std::path::Path::new(path).is_dir()) {
        dialog = dialog.set_directory(start);
    }
    // Modal over the window, so it cannot be lost behind it.
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }

    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |picked| {
        let _ = sender.send(picked);
    });
    receiver
        .await
        .ok()
        .flatten()
        .and_then(|path| path.into_path().ok())
        .map(|path| path.display().to_string())
}

#[tauri::command]
async fn cancel_post_action(state: State<'_, AppState>) -> Result<(), String> {
    state.simple(Request::CancelPostAction).await
}

#[tauri::command]
async fn history(state: State<'_, AppState>, limit: u32) -> Result<serde_json::Value, String> {
    state.history(limit)
}

#[tauri::command]
async fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    state.clear_history()
}

#[tauri::command]
async fn clear_incomplete_history(state: State<'_, AppState>) -> Result<(), String> {
    state.clear_incomplete_history()
}

#[tauri::command]
async fn remove_history_row(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.remove_history_row(id)
}

/// Opens a finished file with whatever Windows uses for it.
///
/// Through `explorer.exe` rather than a shell string: the path arrives as one
/// argv element, so a filename containing quotes or an ampersand cannot turn
/// into extra arguments. A download's name comes from a server, which makes
/// that the difference between opening a file and running a command.
#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Deletes one downloaded file.
///
/// The only thing in this program that removes a file somebody downloaded,
/// and it exists because of the one that does not: a detection leaves the
/// file alone and says so, which is right, and then leaves the person
/// holding something they want gone.
///
/// To the recycle bin would be friendlier and is not offered, because the
/// case this is for is a file Windows Defender has just called malware and
/// moving it somewhere it still exists is not what "delete" should mean
/// there.
///
/// A directory is refused. A torrent's download is a folder, and a
/// recursive delete reached from a menu is a different and much larger
/// promise than this one.
#[tauri::command]
fn delete_file(path: String) -> Result<(), String> {
    let path = std::path::PathBuf::from(path);
    match std::fs::symlink_metadata(&path) {
        Ok(data) if data.is_dir() => {
            Err("that download is a folder, not a file; remove it in Explorer".into())
        }
        Ok(_) => std::fs::remove_file(&path).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Shows a finished file in Explorer, selected.
#[tauri::command]
fn reveal(path: String) -> Result<(), String> {
    // An argv array, never a shell string: the path comes from a server's
    // Content-Disposition by way of the engine, and a shell would read
    // quotes and ampersands in it as syntax.
    std::process::Command::new("explorer.exe")
        .arg("/select,")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Where the bundled extension is, so the window can show it.
///
/// The installer registers the native messaging host, which is the half a
/// user cannot do. It cannot install the extension: this is not in any web
/// store, and a browser will only load an unpacked extension through its own
/// developer-mode flow, by hand. So the remaining step is theirs, and the
/// only way it does not become "ask the person who gave you this" is for the
/// app to say where the folder is and open it.
///
/// Returns nothing when the folder cannot be found, and the window then says
/// so rather than showing a path that goes nowhere. See [`extension_dir`] for
/// where it looks; none of it is a fixed location.
#[tauri::command]
fn browser_setup(app: AppHandle) -> Option<serde_json::Value> {
    let chromium = extension_dir(&app)?;
    // Firefox wants the manifest itself, not the folder, in its "load
    // temporary add-on" dialog.
    let firefox = chromium.with_file_name("firefox").join("manifest.json");

    Some(serde_json::json!({
        "chromiumDir": display_path(&chromium),
        "firefoxManifest": firefox.is_file().then(|| display_path(&firefox)),
    }))
}

/// Where the bundled Chromium extension is, found from where the app runs.
///
/// Nothing here is a fixed path. An installed copy keeps it under its own
/// install directory, which is wherever the user's installer put it, so it is
/// found relative to the executable and to the resource directory. A copy run
/// straight out of a source checkout has it under `extension/dist` at the
/// repository root, which is found by walking up from `target\release` --
/// and only from there: climbing the directory tree looking for any folder
/// named `extension` would let a folder placed in a parent directory pose as
/// the bundled one, and this path is what the user is told to load into their
/// browser.
fn extension_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    let exe_dir =
        std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    // Source checkout: <repo>\target\<profile>\divinepulse.exe. Checked
    // first because that is where the extension is *built*. Building an
    // installer also copies the extension into `target\<profile>\extension`,
    // and that copy goes stale the moment the source changes -- so the window
    // would point at an old extension while a new one sat next to it.
    if let Some(dir) = &exe_dir {
        let in_target = dir
            .parent()
            .is_some_and(|target| target.file_name().is_some_and(|name| name == "target"));
        if in_target && let Some(repo) = dir.parent().and_then(|target| target.parent()) {
            candidates.push(repo.join("extension").join("dist").join("chromium"));
        }
    }
    // Installed: the install directory, then beside the executable. The same
    // place in an installed copy; the second keeps a portable unpacked build
    // working.
    if let Ok(resources) = app.path().resource_dir() {
        candidates.push(resources.join("extension").join("chromium"));
    }
    if let Some(dir) = &exe_dir {
        candidates.push(dir.join("extension").join("chromium"));
    }

    candidates.into_iter().find(|dir| dir.join("manifest.json").is_file())
}

/// A path as a person would type it.
///
/// Tauri reports resource paths with Windows' verbatim prefix, `\\?\D:\...`.
/// It is a valid path and an unfriendly thing to show someone who is about to
/// paste it into a browser's folder dialog, so it is dropped here. A network
/// share keeps its `\\?\UNC\` form converted to the ordinary `\\server\share`.
fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
}

/// Files the shell opened with this app since the window last asked.
///
/// Collected by the window when it loads and whenever it is told there are
/// more, because a launch from a double-click can happen before the window
/// exists to be told.
#[tauri::command]
fn opened_files(state: State<'_, AppState>) -> Vec<String> {
    state.take_opened()
}

/// Remembers files the shell opened with this app, and tells the window.
fn handle_opened_files(app: &AppHandle, files: Vec<String>) {
    if files.is_empty() {
        return;
    }
    app.state::<AppState>().note_opened(files);
    // No payload: the window asks for the list, which also covers the case of
    // it not being ready to hear this yet.
    let _ = app.emit("opened-file", ());
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

// ---------------------------------------------------------------------------

fn main() {
    // Tauri runs its own tokio runtime, so there is no second one here. The
    // engine's download tasks and the IPC server both live on it.
    tauri::Builder::default()
        // Registered first, as the plugin requires: a second launch hands
        // its arguments to the copy already running and exits, rather than
        // starting a second engine that opens the same database and loses a
        // race for the same named pipe. Double-clicking the icon while the
        // app sits in the tray is the ordinary way to hit this.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A second launch is how a double-clicked file reaches an app that
            // is already running in the tray.
            handle_opened_files(app, opened::torrent_files(&args));
            reveal_window(app);
        }))
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            snapshot,
            add_download,
            cancel_job,
            resume_job,
            resume_from_history,
            pause_job,
            refresh_link,
            queues,
            delete_file,
            peek_archive,
            grab_links,
            add_links,
            export_list,
            web_address,
            rpc_secret,
            import_list,
            bandwidth_rules,
            set_bandwidth_rule,
            forget_bandwidth_rule,
            credentials,
            save_credential,
            forget_credential,
            sidecars,
            fetch_sidecar,
            set_queue,
            enable_queue,
            remove_queue,
            assign_queue,
            approve_pairing,
            deny_pairing,
            set_browser_enabled,
            browsers,
            save_settings,
            cancel_post_action,
            history,
            clear_history,
            clear_incomplete_history,
            remove_history_row,
            reveal,
            open_file,
            file_icon,
            verify_file,
            published_hash,
            choose_folder,
            browser_setup,
            opened_files,
            quit,
        ])
        .setup(|app| {
            build_tray(app.handle())?;
            start_ticker(app.handle().clone());
            start_endpoint(app.handle().clone());
            start_web(app.handle().clone());
            start_rpc(app.handle().clone());

            // Started by the Run key at sign-in: come up in the tray rather
            // than throwing a window at someone who has just logged in.
            if std::env::args().any(|argument| argument == "--hidden")
                && let Some(window) = app.get_webview_window("main")
            {
                let _ = window.hide();
            }

            // A cold start from a double-click: the file is in this launch's
            // own arguments. A file means the user wants to see something
            // happen, so the window is not left hidden in the tray for it.
            let files = opened::torrent_files(std::env::args());
            if !files.is_empty() {
                handle_opened_files(app.handle(), files);
                reveal_window(app.handle());
            }

            restore_downloads(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Hide rather than exit: downloads are still running, and a
                // window close is not a request to abandon them. The tray is
                // where quitting lives, and it says so.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("the application failed to start")
        // `build` + `run` rather than `run` alone, for the sake of this one
        // event. Every way out of the app ends in a process exit that runs
        // no destructor, so the database's connection is never closed and
        // its write-ahead log is never folded back -- leaving the newest
        // history in a file beside `state.sqlite3` rather than in it. This
        // is the single place every route out passes through: the tray's
        // Quit, the window's, and a signed-out or restarting session.
        .run(|app, event| {
            if let RunEvent::Exit = event {
                app.state::<AppState>().engine.checkpoint();
            }
        });
}

/// Brings back downloads that were interrupted by a restart.
///
/// After the endpoint, so a browser that reconnects first does not race it,
/// and spawned rather than awaited so a slow disk cannot hold up the window.
fn restore_downloads(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let restored = app.state::<AppState>().engine.restore_interrupted().await;
        if restored > 0 {
            let _ = app.emit("restored", restored);
        }
    });
}

/// Serves the local endpoint the browser extension talks to.
///
/// Running it here is what lets the app replace `idm --serve` rather than sit
/// alongside it: one process owns the engine, the pipe and the window.
fn start_endpoint(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let engine = Arc::clone(&app.state::<AppState>().engine);
        match IpcServer::bind() {
            Ok(server) => {
                if let Err(error) = server.serve(engine).await {
                    tracing::error!(%error, "the local endpoint stopped");
                }
            }
            Err(error) => {
                // Usually another copy already listening. The window is still
                // useful on its own, so this is reported rather than fatal.
                tracing::error!(%error, "could not open the local endpoint");
            }
        }
    });
}

/// Keeps the web interface matching its setting, for as long as the app runs.
///
/// A supervisor rather than a start, because the switch is in Settings:
/// turning it off should close the port, and turning it on should not mean
/// restarting the app.
fn start_web(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let engine = Arc::clone(&app.state::<AppState>().engine);
        idm_engine::web::supervise(engine).await;
    });
}

/// The same, for the aria2-compatible RPC endpoint.
///
/// Its own supervisor rather than a branch inside the web one: they are
/// separate switches on separate ports, and one failing to bind must not
/// stop the other from trying.
fn start_rpc(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let engine = Arc::clone(&app.state::<AppState>().engine);
        idm_engine::rpc::supervise(engine).await;
    });
}

/// Pushes a snapshot to the window on a timer.
///
/// The window does not poll. A pull loop in the webview keeps the renderer
/// busy even when nothing has changed, and makes the update rate a property
/// of JavaScript timers rather than something this side can reason about.
fn start_ticker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;

            // Only when someone can see it. A hidden window costs nothing to
            // skip, and this runs for as long as the app does.
            let visible = app
                .get_webview_window("main")
                .and_then(|window| window.is_visible().ok())
                .unwrap_or(false);
            if !visible {
                continue;
            }

            let state = app.state::<AppState>();
            if let Ok(payload) = state.snapshot().await {
                let _ = app.emit("snapshot", payload);
            }
        }
    });
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open DivinePulse", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit_item])?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().expect("a window icon").clone())
        .tooltip("DivinePulse")
        .menu(&menu)
        // The menu belongs on right-click only, so a left click can do the
        // obvious thing and open the window.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => reveal_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left, button_state: MouseButtonState::Up, ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        // Unminimise too: a window that was minimised stays that way on
        // `show` alone, so the click would look like it did nothing.
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::display_path;
    use std::path::Path;

    #[test]
    fn the_verbatim_prefix_is_dropped() {
        assert_eq!(
            display_path(Path::new(r"\\?\D:\Apps\DivinePulse\extension")),
            r"D:\Apps\DivinePulse\extension"
        );
    }

    #[test]
    fn a_network_share_keeps_its_shape() {
        assert_eq!(display_path(Path::new(r"\\?\UNC\server\share\ext")), r"\\server\share\ext");
    }

    #[test]
    fn an_ordinary_path_is_left_alone() {
        assert_eq!(
            display_path(Path::new(r"C:\Program Files\DivinePulse")),
            r"C:\Program Files\DivinePulse"
        );
    }
}
