//! AIncient Manager — the Tauri (desktop GUI) front-end.
//!
//! This is a thin shell: every command delegates to [`aincient_core`], the same
//! engine the `aincient` CLI uses. Long-running operations (install, update,
//! backup, restore) run on a blocking thread so the UI never freezes.

use std::path::PathBuf;

use aincient_core::{ops, Backup, InstallOptions, Preflight, Reporter, Stack, Stage, Status, UpdateCheck};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_dialog::DialogExt;

/// Locate the stack, surfacing errors as strings for the webview.
fn stack() -> Result<Stack, String> {
    Stack::locate().map_err(|e| format!("{e:#}"))
}

/// Render an anyhow error (with its context chain) as a single string.
fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// A progress update pushed to the webview as an `op-progress` event during a
/// long lifecycle op. The frontend advances a progress bar from `fraction` and
/// appends `message` to a live log feed.
#[derive(Clone, Serialize)]
struct ProgressEvent {
    /// `"stage"` — a milestone that advances the bar; `"log"` — a passed-through
    /// docker line that only appends to the feed.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fraction: Option<f32>,
}

/// Relays core lifecycle progress to the webview. Lives on the blocking worker
/// thread; `AppHandle::emit` is thread-safe, so events reach the UI as they
/// happen rather than all at once when the op returns.
struct EventReporter {
    app: AppHandle,
}

impl Reporter for EventReporter {
    fn captures_output(&self) -> bool {
        true
    }

    fn stage(&mut self, stage: Stage, message: &str, fraction: Option<f32>) {
        let _ = self.app.emit(
            "op-progress",
            ProgressEvent {
                kind: "stage",
                stage: Some(stage),
                message: message.to_string(),
                fraction,
            },
        );
    }

    fn log(&mut self, line: &str) {
        let _ = self.app.emit(
            "op-progress",
            ProgressEvent {
                kind: "log",
                stage: None,
                message: line.to_string(),
                fraction: None,
            },
        );
    }
}

/// Run a blocking closure off the UI thread and await its result.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(r) => r,
        Err(join) => Err(format!("internal error: {join}")),
    }
}

#[tauri::command]
fn check_preflight() -> Preflight {
    aincient_core::preflight()
}

/// The first unmet Docker requirement, if any — the GUI shows this on its
/// readiness screen instead of letting an op fail with a raw Docker error.
#[tauri::command]
fn preflight_problem() -> Option<String> {
    aincient_core::preflight().problem()
}

#[tauri::command]
async fn get_status() -> Result<Status, String> {
    let s = stack()?;
    blocking(move || Ok(ops::status(&s))).await
}

#[tauri::command]
async fn get_update() -> Result<UpdateCheck, String> {
    let s = stack()?;
    blocking(move || Ok(ops::check_update(&s))).await
}

#[tauri::command]
fn list_backups() -> Result<Vec<Backup>, String> {
    let s = stack()?;
    Ok(ops::list_backups(&s))
}

#[tauri::command]
async fn set_admin_password(app: AppHandle, password: String) -> Result<(), String> {
    if password.trim().is_empty() {
        return Err("password cannot be empty".into());
    }
    let s = stack()?;
    blocking(move || ops::set_admin_password(&s, &password, &mut EventReporter { app }).map_err(err))
        .await
}

#[tauri::command]
async fn do_install(
    app: AppHandle,
    image: Option<String>,
    port: Option<u16>,
) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        let opts = InstallOptions {
            image,
            http_port: port,
        };
        // `install` reports each phase (and holds until Drupal serves) through the
        // reporter, so the UI's progress bar/log only finish on real readiness —
        // not the instant the container starts.
        ops::install(&s, &opts, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn do_update(app: AppHandle) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        ops::update(&s, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn do_backup(app: AppHandle, label: Option<String>) -> Result<String, String> {
    let s = stack()?;
    blocking(move || {
        ops::backup(&s, label.as_deref(), &mut EventReporter { app })
            .map(|p: PathBuf| p.to_string_lossy().into_owned())
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_restore(app: AppHandle, path: String) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        ops::restore(&s, std::path::Path::new(&path), &mut EventReporter { app }).map_err(err)
    })
    .await
}

/// Open a native "open file" dialog and return the chosen backup's path (or
/// `None` if cancelled). The frontend then confirms and hands the path to
/// [`do_restore`] — so restoring an *imported* snapshot goes through the exact
/// same core path (and confirm gate) as restoring one from the backups list.
/// Filtered to the archive shapes [`ops::restore`] accepts (`.tar.gz`/`.tgz`
/// bundles and legacy `.sql`/`.sql.gz` dumps); core still validates.
#[tauri::command]
async fn pick_restore_file(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    app.dialog()
        .file()
        .set_title("Choose a backup to restore")
        .add_filter("AIncient backup", &["gz", "tgz", "sql"])
        .pick_file(move |picked| {
            let path = picked.and_then(|p| p.into_path().ok());
            let _ = tx.blocking_send(path.map(|p| p.to_string_lossy().into_owned()));
        });
    Ok(rx.recv().await.flatten())
}

/// Open a native "save as" dialog and copy the selected backup out of the
/// managed `~/.atelier/backups` directory to wherever the user chooses (Desktop,
/// a USB drive, …) so they can archive it off-machine. `source` is the backup's
/// path from [`list_backups`]. Returns whether a file was written (`false` = the
/// user cancelled the dialog).
#[tauri::command]
async fn export_backup(app: AppHandle, source: String) -> Result<bool, String> {
    let src = PathBuf::from(&source);
    let default_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("aincient-backup.tar.gz")
        .to_string();
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    app.dialog()
        .file()
        .set_title("Save backup as")
        .set_file_name(&default_name)
        .save_file(move |picked| {
            let path = picked.and_then(|p| p.into_path().ok());
            let _ = tx.blocking_send(path);
        });
    let Some(dest) = rx.recv().await.flatten() else {
        return Ok(false); // cancelled
    };
    blocking(move || {
        std::fs::copy(&src, &dest)
            .map(|_| true)
            .map_err(|e| format!("could not save the backup: {e}"))
    })
    .await
}

#[tauri::command]
async fn do_reinstall(app: AppHandle) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        let opts = InstallOptions::default();
        ops::reinstall(&s, &opts, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn do_start(app: AppHandle) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        ops::start(&s, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn do_stop(app: AppHandle) -> Result<(), String> {
    let s = stack()?;
    blocking(move || ops::stop(&s, &mut EventReporter { app }).map_err(err)).await
}

#[tauri::command]
fn open_console() -> Result<(), String> {
    let s = stack()?;
    ops::open_console(&s).map_err(err)
}

#[tauri::command]
fn open_login() -> Result<(), String> {
    let s = stack()?;
    ops::open_login(&s).map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            check_preflight,
            preflight_problem,
            get_status,
            get_update,
            list_backups,
            set_admin_password,
            do_install,
            do_update,
            do_backup,
            do_restore,
            pick_restore_file,
            export_backup,
            do_reinstall,
            do_start,
            do_stop,
            open_console,
            open_login,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the AIncient Manager");
}
