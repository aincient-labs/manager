//! AIncient Manager — the Tauri (desktop GUI) front-end.
//!
//! This is a thin shell: every command delegates to [`aincient_core`], the same
//! engine the `aincient` CLI uses. Long-running operations (install, update,
//! backup, restore) run on a blocking thread so the UI never freezes.

use std::path::PathBuf;

use aincient_core::{ops, Backup, InstallOptions, Preflight, Stack, Status, UpdateCheck};

/// Locate the stack, surfacing errors as strings for the webview.
fn stack() -> Result<Stack, String> {
    Stack::locate().map_err(|e| format!("{e:#}"))
}

/// Render an anyhow error (with its context chain) as a single string.
fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
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
async fn admin_password() -> Result<Option<String>, String> {
    let s = stack()?;
    blocking(move || Ok(ops::admin_password(&s))).await
}

#[tauri::command]
async fn set_admin_password(password: String) -> Result<(), String> {
    if password.trim().is_empty() {
        return Err("password cannot be empty".into());
    }
    let s = stack()?;
    blocking(move || ops::set_admin_password(&s, &password).map_err(err)).await
}

#[tauri::command]
async fn do_install(
    key: Option<String>,
    image: Option<String>,
    port: Option<u16>,
) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        let opts = InstallOptions {
            ai_key: key,
            image,
            http_port: port,
        };
        ops::install(&s, &opts).map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_update() -> Result<(), String> {
    let s = stack()?;
    blocking(move || ops::update(&s).map_err(err)).await
}

#[tauri::command]
async fn do_backup(label: Option<String>) -> Result<String, String> {
    let s = stack()?;
    blocking(move || {
        ops::backup(&s, label.as_deref())
            .map(|p: PathBuf| p.to_string_lossy().into_owned())
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_restore(path: String) -> Result<(), String> {
    let s = stack()?;
    blocking(move || ops::restore(&s, std::path::Path::new(&path)).map_err(err)).await
}

#[tauri::command]
async fn do_reinstall(key: Option<String>) -> Result<(), String> {
    let s = stack()?;
    blocking(move || {
        let opts = InstallOptions {
            ai_key: key,
            ..Default::default()
        };
        ops::reinstall(&s, &opts).map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_start() -> Result<(), String> {
    let s = stack()?;
    blocking(move || ops::start(&s).map_err(err)).await
}

#[tauri::command]
async fn do_stop() -> Result<(), String> {
    let s = stack()?;
    blocking(move || ops::stop(&s).map_err(err)).await
}

#[tauri::command]
fn open_console() -> Result<(), String> {
    let s = stack()?;
    ops::open_console(&s).map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            check_preflight,
            preflight_problem,
            get_status,
            get_update,
            list_backups,
            admin_password,
            set_admin_password,
            do_install,
            do_update,
            do_backup,
            do_restore,
            do_reinstall,
            do_start,
            do_stop,
            open_console,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the AIncient Manager");
}
