//! Divine IDM, the desktop application.
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
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

mod autostart;
mod opened;
mod state;
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
) -> Result<u64, String> {
    state.add_download(url, filename).await
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
async fn resume_from_history(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.simple(Request::ResumeFromHistory { row_id: id }).await
}

/// Holds a download where it is, keeping every byte it already has.
#[tauri::command]
async fn pause_job(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    state.simple(Request::Pause { id: JobId(id) }).await
}

/// Points a download at a new address without losing what it has.
#[tauri::command]
async fn refresh_link(state: State<'_, AppState>, id: u64, url: String) -> Result<(), String> {
    state.simple(Request::RefreshUrl { id: JobId(id), url }).await
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

    // Source checkout: <repo>\target\<profile>\divine-idm.exe. Checked
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
            choose_folder,
            browser_setup,
            opened_files,
            quit,
        ])
        .setup(|app| {
            build_tray(app.handle())?;
            start_ticker(app.handle().clone());
            start_endpoint(app.handle().clone());

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
        .run(tauri::generate_context!())
        .expect("the application failed to start");
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
    let show = MenuItem::with_id(app, "show", "Open Divine IDM", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit_item])?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().expect("a window icon").clone())
        .tooltip("Divine IDM")
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
            display_path(Path::new(r"\\?\D:\Apps\Divine IDM\extension")),
            r"D:\Apps\Divine IDM\extension"
        );
    }

    #[test]
    fn a_network_share_keeps_its_shape() {
        assert_eq!(display_path(Path::new(r"\\?\UNC\server\share\ext")), r"\\server\share\ext");
    }

    #[test]
    fn an_ordinary_path_is_left_alone() {
        assert_eq!(
            display_path(Path::new(r"C:\Program Files\Divine IDM")),
            r"C:\Program Files\Divine IDM"
        );
    }
}
