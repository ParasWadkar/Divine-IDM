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
async fn revoke_pairing(state: State<'_, AppState>, client_id: String) -> Result<(), String> {
    state.simple(Request::RevokePairing { client_id }).await
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
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            reveal_window(app);
        }))
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            snapshot,
            add_download,
            cancel_job,
            resume_job,
            approve_pairing,
            deny_pairing,
            revoke_pairing,
            save_settings,
            cancel_post_action,
            history,
            clear_history,
            reveal,
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
