//! Atelier Manager — the Tauri (desktop GUI) front-end.
//!
//! This is a thin shell: every command delegates to [`aincient_core`], the same
//! engine the `atelier` CLI uses. Long-running operations (install, update,
//! backup, restore) run on a blocking thread so the UI never freezes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use aincient_core::docker::Pipe;
use aincient_core::engine_admin::{self, AdminVerb, EngineAdmin, EngineInfo, FirstPairing};
use aincient_core::engine_client::{EngineError, Verb};
use aincient_core::settings::{self, Settings};
use aincient_core::{
    doctor, ops, remote, sites, ssh_key, update_cache, Backup, CacheStatus, ChangeGuard, Channel,
    InstallOptions, ModelRole, Preflight, PullEvent, Reporter, SingleFlight, Site, SiteRow, Stack,
    Stage, Status, UpdateCheck,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

mod paired;

/// A site paired with its engine (M4): no SSH, the engine's verbs only.
fn is_paired(site: &Site) -> bool {
    site.engine_url().is_some()
}

/// The site this window operates on — its registry slug, or `None` when nothing
/// is registered (a fresh machine, a hand-rolled `ATELIER_HOME`), which keeps
/// the single-site behaviour of `Stack::locate`.
///
/// Held by the window rather than read from the sites registry on every call: an
/// `atelier sites use` in a terminal must not move the GUI to another site
/// between two clicks. Every command resolves its stack from this, so no action
/// runs against a site the header isn't showing.
struct Selected(Mutex<Option<String>>);

impl Selected {
    /// Start on the site the CLI would pick right now (`ATELIER_HOME` > the
    /// registry's active site > `~/.atelier`).
    fn from_resolution() -> Self {
        let slug = sites::locate(None)
            .ok()
            .and_then(|r| r.site)
            .map(|s| s.slug);
        Self(Mutex::new(slug))
    }

    fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, slug: String) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(slug);
    }
}

/// Locate the selected site's stack, surfacing errors as strings for the webview.
fn stack(sel: &Selected) -> Result<Stack, String> {
    match sel.get() {
        Some(slug) => Stack::locate_site(Some(&slug)),
        None => Stack::locate(),
    }
    .map_err(|e| format!("{e:#}"))
}

/// Render an anyhow error (with its context chain) as a single string.
fn err(e: anyhow::Error) -> String {
    // The core words this one for the CLI (`atelier app start`); the window
    // has a button for it.
    if e.downcast_ref::<ops::NotRunning>().is_some() {
        return NOT_RUNNING.into();
    }
    format!("{e:#}")
}

/// [`ops::NotRunning`] in the window's words.
const NOT_RUNNING: &str =
    "Your website isn't running. Start it from Home (Start my website), then try again.";

// --- local or remote ---------------------------------------------------------

/// What the selected site is: a stack on this computer, or a site on a server
/// reached over SSH (M2). Every command that touches a site branches on this
/// and calls the same core path the `atelier` CLI uses for that kind.
enum Target {
    Local(Stack),
    Remote(Site),
}

/// Resolve the selected site. A registered remote site is [`Target::Remote`];
/// everything else (a local site, an unregistered `~/.atelier`) is local.
fn target(sel: &Selected) -> Result<Target, String> {
    if let Some(slug) = sel.get() {
        let resolution = sites::locate(Some(&slug)).map_err(err)?;
        if let Some(site) = resolution.site.filter(Site::is_remote) {
            return Ok(Target::Remote(site));
        }
    }
    stack(sel).map(Target::Local)
}

/// The selected site's stack, for an action that only exists on this
/// computer; a remote site gets a refusal that names its server and the
/// command to run there.
fn local_stack(sel: &Selected, what: &str) -> Result<Stack, String> {
    match target(sel)? {
        Target::Local(s) => Ok(s),
        Target::Remote(site) => {
            let r = site.remote.as_ref().expect("remote");
            Err(format!(
                "{what} isn't available here for a site on a server yet — {} is on {}. \
                 Run it there: ssh {} atelier --site {} …",
                site.label, r.host, r.host, r.slug
            ))
        }
    }
}

/// The ssh target of a remote site.
fn host_of(site: &Site) -> String {
    site.remote
        .as_ref()
        .map(|r| r.host.clone())
        .unwrap_or_default()
}

/// Open a session to a remote site's engine (copying a matching `atelier`
/// to the server when it has none, exactly as the CLI does).
fn session(site: &Site) -> Result<remote::Session<remote::Ssh>, String> {
    remote::open(site, &remote::EngineOptions::from_env())
        .map_err(|e| format!("Couldn't reach {} on {}: {e:#}", site.label, host_of(site)))
}

/// A remote command's output, line by line, into the progress feed.
fn feed(app: &AppHandle) -> impl FnMut(Pipe, &str) + '_ {
    move |_pipe, line| {
        let _ = app.emit(
            "op-progress",
            ProgressEvent {
                kind: "log",
                stage: None,
                message: remote::strip_ansi(line),
                fraction: None,
                pull: None,
            },
        );
    }
}

/// How long a status read of a server may take before the window says it
/// can't reach it.
const REMOTE_STATUS_LIMIT: std::time::Duration = std::time::Duration::from_secs(25);

/// A progress update pushed to the webview as an `op-progress` event during a
/// long lifecycle op. The frontend advances a progress bar from `fraction` and
/// appends `message` to a live log feed.
#[derive(Clone, Serialize)]
struct ProgressEvent {
    /// `"stage"` — a milestone that advances the bar; `"log"` — a passed-through
    /// docker line that only appends to the feed; `"pull"` — a structured
    /// per-layer pull event carried in `pull`.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fraction: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull: Option<PullEvent>,
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
                pull: None,
            },
        );
    }

    /// A native modal, defaulting to "Not now".
    ///
    /// Blocking is correct here and safe: the reporter lives on the blocking worker
    /// thread, never the UI thread, and the operation genuinely must not continue
    /// until this is answered. Routing it through the webview instead would mean
    /// inventing a request/response channel for a question asked once, on adoption
    /// of a legacy install.
    fn confirm(&mut self, question: &str) -> bool {
        self.app
            .dialog()
            .message(question)
            .title("Switch this site to released versions?")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Switch to releases".into(),
                "Not now".into(),
            ))
            .blocking_show()
    }

    fn log(&mut self, line: &str) {
        let _ = self.app.emit(
            "op-progress",
            ProgressEvent {
                kind: "log",
                stage: None,
                message: line.to_string(),
                fraction: None,
                pull: None,
            },
        );
    }

    fn pull_progress(&mut self, ev: &PullEvent) {
        let _ = self.app.emit(
            "op-progress",
            ProgressEvent {
                kind: "pull",
                stage: None,
                message: String::new(),
                fraction: None,
                pull: Some(ev.clone()),
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

/// `async` + [`blocking`]: a plain `fn` command runs on the main thread, and
/// `docker info` can stall when Docker Desktop is wedged (manager#6).
#[tauri::command]
async fn check_preflight() -> Result<Preflight, String> {
    blocking(|| Ok(aincient_core::preflight())).await
}

/// The first unmet Docker requirement, if any — the GUI shows this on its
/// readiness screen instead of letting an op fail with a raw Docker error.
/// Off the main thread for the same reason as [`check_preflight`]: `refresh()`
/// awaits this before anything renders.
///
/// A site on a server needs no Docker here: its engine is the server's.
#[tauri::command]
async fn preflight_problem(sel: State<'_, Selected>) -> Result<Option<String>, String> {
    if let Target::Remote(_) = target(&sel)? {
        return Ok(None);
    }
    blocking(|| Ok(aincient_core::preflight().problem())).await
}

/// The selected site's status. For a site on a server: its own CLI's answer
/// over SSH, bounded by [`REMOTE_STATUS_LIMIT`], with the URLs this machine
/// can open (its recorded domain, or none — never the server's localhost).
#[tauri::command]
async fn get_status(sel: State<'_, Selected>) -> Result<Status, String> {
    match target(&sel)? {
        Target::Local(s) => blocking(move || Ok(ops::status(&s))).await,
        Target::Remote(site) if is_paired(&site) => {
            require_experimental_for(&site)?;
            blocking(move || paired::status(&site)).await
        }
        Target::Remote(site) => {
            blocking(move || {
                let host = host_of(&site);
                let remote = site.remote.clone().expect("remote");
                remote::within(&host, REMOTE_STATUS_LIMIT, move || {
                    let st = remote::open(
                        &site,
                        &remote::EngineOptions {
                            no_install: true,
                            ..remote::EngineOptions::from_env()
                        },
                    )?
                    .status()?;
                    Ok(remote::status_for_here(&remote, st).0)
                })
                .map_err(|e| format!("Can't reach {host}: {e:#}"))
            })
            .await
        }
    }
}

/// How a destructive change to the selected site is confirmed (m2-8, D-07):
/// the dialog names where it runs, and a live site on a server keeps its
/// confirm button disabled until the slug is typed. A server that can't be
/// asked counts as live.
#[tauri::command]
async fn change_guard(sel: State<'_, Selected>) -> Result<ChangeGuard, String> {
    match target(&sel)? {
        Target::Local(s) => Ok(ChangeGuard::local(&s)),
        Target::Remote(site) if is_paired(&site) => {
            blocking(move || Ok(paired::guard(&site))).await
        }
        Target::Remote(site) => {
            blocking(move || {
                let host = host_of(&site);
                let probe = site.clone();
                let live = remote::within(&host, REMOTE_STATUS_LIMIT, move || {
                    Ok(session(&probe).map_err(anyhow::Error::msg)?.is_live())
                })
                .unwrap_or(true);
                Ok(ChangeGuard::for_site(&site, live))
            })
            .await
        }
    }
}

/// A remote site's state for the switcher: one bounded read of its server
/// (copies nothing there). Called per remote row, after the list painted.
#[tauri::command]
async fn probe_site(slug: String) -> Result<remote::SiteState, String> {
    blocking(move || {
        let resolution = sites::locate(Some(&slug)).map_err(err)?;
        let site = resolution
            .site
            .filter(Site::is_remote)
            .ok_or_else(|| format!("`{slug}` is not a site on a server"))?;
        if is_paired(&site) {
            if require_experimental().is_err() {
                return Ok(remote::SiteState::Unreachable {
                    reason: EXPERIMENTAL_OFF.into(),
                });
            }
            return Ok(paired::probe(&site));
        }
        Ok(remote::probe_state(&site, remote::PROBE_STATE_LIMIT))
    })
    .await
}

/// The one registry check allowed in flight at a time **per site** (manager#6):
/// every caller of [`get_update`] / [`refresh_update_cache`] that arrives while
/// that site's check runs waits for it and shares its result, so a slow registry
/// can't pile up `docker buildx imagetools` processes. Keyed by stack directory,
/// because sites follow different images — one global flight would hand a check
/// of site A to someone who just switched to site B.
fn update_flight(stack: &Stack) -> Arc<SingleFlight<UpdateCheck>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<PathBuf, Arc<SingleFlight<UpdateCheck>>>>> =
        OnceLock::new();
    FLIGHTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(stack.home.clone())
        .or_default()
        .clone()
}

/// A LIVE registry check — the explicit "Check for updates" path. Deadlined in
/// core, single-flight here, and its result refreshes the banner's cache.
#[tauri::command]
async fn get_update(sel: State<'_, Selected>) -> Result<UpdateCheck, String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || Ok(update_cache::live_check(&s, &update_flight(&s)))).await
        }
        // The server's own check-update: its registry, its route.
        Target::Remote(site) => blocking(move || session(&site)?.check_update().map_err(err)).await,
    }
}

/// The cached update check, if any — a file read, never the network. What the
/// Home banner (and the Home Update action, while it's fresh) reads.
#[tauri::command]
async fn get_cached_update(sel: State<'_, Selected>) -> Result<Option<CacheStatus>, String> {
    // No cache for a site on a server: its check is the server's to make.
    let Target::Local(s) = target(&sel)? else {
        return Ok(None);
    };
    blocking(move || Ok(update_cache::status(&s))).await
}

/// The background refresh: re-asks the registry only when `force` is set (once
/// after first paint) or the cache has gone stale, then returns the cache.
#[tauri::command]
async fn refresh_update_cache(
    force: bool,
    sel: State<'_, Selected>,
) -> Result<Option<CacheStatus>, String> {
    let Target::Local(s) = target(&sel)? else {
        return Ok(None);
    };
    blocking(move || Ok(update_cache::refresh(&s, &update_flight(&s), force))).await
}

/// Off the main thread: a directory listing is quick, but a plain `fn` command
/// runs on the main thread and nothing there may block (manager#6).
///
/// For a site on a server: the copies on this machine (D-03), which are what
/// a restore here uploads.
#[tauri::command]
async fn list_backups(sel: State<'_, Selected>) -> Result<Vec<Backup>, String> {
    let s = match target(&sel)? {
        Target::Local(s) => s,
        Target::Remote(site) => site.stack(),
    };
    blocking(move || Ok(ops::list_backups(&s))).await
}

#[tauri::command]
async fn set_admin_password(
    app: AppHandle,
    password: String,
    sel: State<'_, Selected>,
) -> Result<(), String> {
    if password.trim().is_empty() {
        return Err("password cannot be empty".into());
    }
    let s = local_stack(&sel, "Setting the admin password")?;
    blocking(move || {
        ops::set_admin_password(&s, &password, &mut EventReporter { app }).map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_install(
    app: AppHandle,
    image: Option<String>,
    port: Option<u16>,
    sel: State<'_, Selected>,
) -> Result<(), String> {
    let s = local_stack(&sel, "Setting up")?;
    blocking(move || {
        // A registered site without a `.env` gets a port no other site claims;
        // everything else keeps today's behaviour — the same rule as
        // `atelier app install` (DECISIONS 0393).
        let user_home = sites::user_home().ok();
        let registry = user_home
            .as_deref()
            .and_then(|uh| sites::Registry::load(uh).ok().flatten());
        let opts = InstallOptions {
            image,
            http_port: sites::port_for_install(
                &s,
                port,
                registry.as_ref(),
                user_home.as_deref(),
                sites::host_port_is_free,
            ),
        };
        // `install` reports each phase (and holds until Drupal serves) through the
        // reporter, so the UI's progress bar/log only finish on real readiness —
        // not the instant the container starts.
        ops::install(&s, &opts, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

/// The window has confirmed (a stepped route; a live remote site's slug).
/// A site on a server walks its own route: the server's `app update --yes`,
/// streamed — unless its check says there is nothing to do.
#[tauri::command]
async fn do_update(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || {
                ops::update(&s, &mut EventReporter { app }).map_err(err)?;
                Ok(())
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let session = session(&site)?;
                if session.check_update().map_err(err)?.update_available == Some(false) {
                    return Ok(());
                }
                session
                    .run_action(
                        &remote::Action::Update {
                            to: None,
                            yes: true,
                        },
                        &mut feed(&app),
                    )
                    .map_err(err)
            })
            .await
        }
    }
}

/// Switch which images this install follows, then pull and converge onto the new
/// channel. Always applies: a settings toggle that silently waits for some later
/// update would leave the UI claiming a state the appliance isn't in.
///
/// The frontend confirms first — a move from edge to stable can hand an older
/// codebase a database a newer one already migrated.
#[tauri::command]
async fn set_channel(
    app: AppHandle,
    channel: String,
    sel: State<'_, Selected>,
) -> Result<String, String> {
    let target = Channel::parse(&channel)
        .filter(|c| *c != Channel::Pinned)
        .ok_or_else(|| format!("unknown channel {channel:?} — use \"stable\" or \"edge\""))?;
    let s = local_stack(&sel, "Switching the update channel")?;
    blocking(move || {
        ops::switch_channel(&s, target, true, &mut EventReporter { app })
            .map(|(image, _ready)| image)
            .map_err(err)
    })
    .await
}

/// Back up the selected site. On a server: the server's `data backup`, then
/// a copy home that is checked against the server's SHA-256 (D-03) — the
/// returned path is that local copy.
#[tauri::command]
async fn do_backup(
    app: AppHandle,
    label: Option<String>,
    sel: State<'_, Selected>,
) -> Result<String, String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || {
                ops::backup(&s, label.as_deref(), &mut EventReporter { app })
                    .map(|p: PathBuf| p.to_string_lossy().into_owned())
                    .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let home = remote::local_backups_dir(&site);
                let got = session(&site)?
                    .backup(label, Some(&home), &mut feed(&app))
                    .map_err(err)?;
                Ok(got
                    .local
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(got.server.path))
            })
            .await
        }
    }
}

/// Whose backup `path` is, before anything is asked (m2-6, D-04): another
/// site's comes back as `mismatch` so the window can name both sites and
/// offer "Restore anyway"; an unlabelled one comes back with a `note`.
#[derive(Serialize)]
struct RestoreCheck {
    note: Option<String>,
    mismatch: Option<ops::IdentityMismatch>,
}

#[tauri::command]
async fn check_restore(path: String, sel: State<'_, Selected>) -> Result<RestoreCheck, String> {
    let target = target(&sel)?;
    blocking(move || {
        let file = std::path::Path::new(&path);
        let checked = match &target {
            Target::Local(s) => ops::restore_identity(s, file, false),
            Target::Remote(site) => session(site)?.check_restore_identity(file, false),
        };
        match checked {
            Ok(check) => Ok(RestoreCheck {
                note: check.message().map(str::to_string),
                mismatch: None,
            }),
            Err(e) => match e.downcast::<ops::IdentityMismatch>() {
                Ok(mismatch) => Ok(RestoreCheck {
                    note: None,
                    mismatch: Some(mismatch),
                }),
                Err(e) => Err(err(e)),
            },
        }
    })
    .await
}

/// Restore `path` onto the selected site, behind the identity check
/// (`ops::restore_with`; on a server, its own `data restore` after a
/// verified upload). `other_site` is the explicit "Restore anyway".
#[tauri::command]
async fn do_restore(
    app: AppHandle,
    path: String,
    other_site: Option<bool>,
    sel: State<'_, Selected>,
) -> Result<(), String> {
    let other_site = other_site.unwrap_or(false);
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || {
                ops::restore_with(
                    &s,
                    std::path::Path::new(&path),
                    &ops::RestoreOptions { other_site },
                    &mut EventReporter { app },
                )
                .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                session(&site)?
                    .restore_upload(std::path::Path::new(&path), other_site, &mut feed(&app))
                    .map_err(err)
            })
            .await
        }
    }
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
        .add_filter("Atelier backup", &["gz", "tgz", "sql"])
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
async fn do_reinstall(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    let s = local_stack(&sel, "Reinstalling")?;
    blocking(move || {
        let opts = InstallOptions::default();
        ops::reinstall(&s, &opts, &mut EventReporter { app }).map_err(err)?;
        Ok(())
    })
    .await
}

/// Run one action on a remote site's server, streamed into the feed.
async fn run_remote(app: AppHandle, site: Site, action: remote::Action) -> Result<(), String> {
    blocking(move || {
        session(&site)?
            .run_action(&action, &mut feed(&app))
            .map_err(err)
    })
    .await
}

#[tauri::command]
async fn do_start(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || {
                ops::start(&s, &mut EventReporter { app }).map_err(err)?;
                Ok(())
            })
            .await
        }
        Target::Remote(site) if is_paired(&site) => {
            require_experimental()?;
            blocking(move || paired::act(&site, Verb::Wake).map(|_| ())).await
        }
        Target::Remote(site) => run_remote(app, site, remote::Action::Start).await,
    }
}

#[tauri::command]
async fn do_stop(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || ops::stop(&s, &mut EventReporter { app }).map_err(err)).await
        }
        Target::Remote(site) => run_remote(app, site, remote::Action::Stop).await,
    }
}

// --- experimental features (m7-3, DECISIONS 0483) ----------------------------

/// The Manager's per-user settings (`~/.atelier/manager.json`).
#[tauri::command]
fn get_settings() -> Result<Settings, String> {
    Ok(settings::load(&sites::user_home().map_err(err)?))
}

/// Turn experimental features on or off. Off hides everything engine-related
/// and makes every engine command below refuse.
#[tauri::command]
fn set_experimental(on: bool) -> Result<Settings, String> {
    let home = sites::user_home().map_err(err)?;
    let mut s = settings::load(&home);
    s.experimental = on;
    settings::save(&home, &s).map_err(err)?;
    Ok(s)
}

/// Refuse unless the user turned experimental features on: the window hides
/// the engine then, and the commands don't trust the window alone.
fn require_experimental() -> Result<(), String> {
    let home = sites::user_home().map_err(err)?;
    if settings::load(&home).experimental {
        Ok(())
    } else {
        Err(
            "The engine is an experimental feature. Turn on Experimental features in \
             System first."
                .into(),
        )
    }
}

/// What a paired site says while Experimental features are off.
const EXPERIMENTAL_OFF: &str = "This site is paired through the experimental engine. Turn on \
     Experimental features in System to manage it.";

fn require_experimental_for(site: &Site) -> Result<(), String> {
    require_experimental().map_err(|_| format!("{} — {EXPERIMENTAL_OFF}", site.label))
}

/// The selected site's engine, as the window shows it.
#[derive(Serialize, Default)]
struct EngineView {
    /// The stack uses the engine (`.env` names it).
    enabled: bool,
    running: bool,
    /// From the engine's `admin status`, when it answered.
    lockdown: Option<bool>,
    /// The site's state as the engine sees it (M5): resting, waking, awake,
    /// locked or stopped.
    state: Option<String>,
    wake_code_set: Option<bool>,
    api: Option<String>,
    /// The engine speaks a newer API major: offer the Manager's update (C-08).
    manager_too_old: bool,
    /// Why the engine couldn't be read, in words.
    problem: Option<String>,
    /// The site is paired with its engine (M4): no SSH, so the engine can't
    /// be added or removed from here, and the Managers panel applies.
    paired: bool,
}

fn engine_view<D: engine_admin::DockerExec + ?Sized>(
    docker: &D,
    info: Option<EngineInfo>,
) -> EngineView {
    let Some(info) = info else {
        return EngineView::default();
    };
    let mut view = EngineView {
        enabled: info.enabled,
        running: info.running,
        ..EngineView::default()
    };
    if info.ready().is_none() {
        return view;
    }
    let read = EngineAdmin::new(docker, Some(&info), "status").and_then(|a| {
        let raw = a.status_unchecked()?;
        view.api = Some(raw.api.clone());
        view.lockdown = Some(raw.lockdown);
        view.state = Some(raw.state.clone()).filter(|s| !s.is_empty());
        view.wake_code_set = Some(raw.wake_code_set);
        a.status()
    });
    if let Err(e) = read {
        view.manager_too_old = matches!(
            e.downcast_ref::<EngineError>(),
            Some(EngineError::ManagerTooOld { .. })
        );
        view.problem = Some(format!("{e:#}"));
    }
    view
}

#[tauri::command]
async fn engine_state(sel: State<'_, Selected>) -> Result<EngineView, String> {
    require_experimental()?;
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => {
            blocking(move || {
                let e = paired::engine(&site)?;
                Ok(EngineView {
                    enabled: true,
                    running: true,
                    lockdown: Some(e.lockdown),
                    api: Some(e.api),
                    state: Some(e.state).filter(|s| !s.is_empty()),
                    wake_code_set: Some(e.wake_code_set),
                    paired: true,
                    ..EngineView::default()
                })
            })
            .await
        }
        Target::Local(s) => {
            blocking(move || {
                Ok(engine_view(
                    &engine_admin::LocalDocker,
                    engine_admin::probe(&s),
                ))
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let host = host_of(&site);
                remote::within(&host, REMOTE_STATUS_LIMIT, move || {
                    let s = remote::open(
                        &site,
                        &remote::EngineOptions {
                            no_install: true,
                            ..remote::EngineOptions::from_env()
                        },
                    )?;
                    let info = s.engine_info()?;
                    Ok(engine_view(&s, info))
                })
                .map_err(|e| format!("Can't reach {host}: {e:#}"))
            })
            .await
        }
    }
}

/// Add the engine to the selected site (the window confirmed first): on this
/// computer core's enable + `up`; on a server its own `atelier engine enable
/// --experimental`, which runs `up` there. Returns the install code that
/// pairs the first Manager (m4-6), when the engine made one, for the window
/// to show once next to the site's address; with a Manager paired already
/// none is made, and the log feed says so.
#[tauri::command]
async fn engine_enable(app: AppHandle, sel: State<'_, Selected>) -> Result<Option<String>, String> {
    require_experimental()?;
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => Err(remote::no_ssh(
            &site.slug,
            site.engine_url().unwrap_or_default(),
        )),
        Target::Local(s) => {
            blocking(move || {
                Ok(match engine_admin::enable_local(&s).map_err(err)? {
                    Some(FirstPairing::Code(code)) => Some(code),
                    Some(FirstPairing::AlreadyPaired) => {
                        feed(&app)(Pipe::Stdout, engine_admin::ALREADY_PAIRED);
                        None
                    }
                    None => None,
                })
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let mut code = None;
                let mut sink = feed(&app);
                session(&site)?
                    .run_streamed(
                        &["engine", "enable", "--experimental"],
                        remote::LIFECYCLE_DEADLINE,
                        &mut |pipe, line| {
                            let plain = remote::strip_ansi(line);
                            if let Some(c) = plain
                                .strip_prefix("Install code: ")
                                .and_then(engine_admin::install_code_in)
                            {
                                // Shown in the dialog, not in the log feed.
                                code = Some(c);
                                return;
                            }
                            sink(pipe, line)
                        },
                    )
                    .map_err(err)?;
                Ok(code)
            })
            .await
        }
    }
}

/// Remove the engine and restore edge + app + db (m3-6, C-09; confirmed in
/// the window first). Allowed with experimental features off too: taking the
/// root-holding sidecar away must never be hidden behind the opt-in.
#[tauri::command]
async fn engine_remove(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Local(s) => {
            blocking(move || {
                let mut say = |line: &str| {
                    let _ = app.emit(
                        "op-progress",
                        ProgressEvent {
                            kind: "log",
                            stage: None,
                            message: line.to_string(),
                            fraction: None,
                            pull: None,
                        },
                    );
                };
                engine_admin::remove(&mut engine_admin::LocalRemove::new(&s), &mut say)
                    .map(|_| ())
                    .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                session(&site)?
                    .run_streamed(
                        &["engine", "remove", "--experimental", "--yes"],
                        remote::LIFECYCLE_DEADLINE,
                        &mut feed(&app),
                    )
                    .map_err(err)
            })
            .await
        }
    }
}

/// Lock down or unlock through the engine (m3-7; confirmed in the window
/// first): `docker exec` here or over SSH, the version read first.
#[tauri::command]
async fn engine_lockdown(lock: bool, sel: State<'_, Selected>) -> Result<String, String> {
    require_experimental()?;
    let verb = if lock {
        AdminVerb::Lockdown
    } else {
        AdminVerb::Unlock
    };
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => {
            let v = if lock { Verb::Lockdown } else { Verb::Unlock };
            blocking(move || paired::act(&site, v)).await
        }
        Target::Local(s) => {
            blocking(move || {
                let info = engine_admin::probe(&s);
                EngineAdmin::new(&engine_admin::LocalDocker, info.as_ref(), verb.word())
                    .and_then(|a| a.run(verb))
                    .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let s = session(&site)?;
                let info = s.engine_info().map_err(err)?;
                EngineAdmin::new(&s, info.as_ref(), verb.word())
                    .and_then(|a| a.run(verb))
                    .map_err(err)
            })
            .await
        }
    }
}

/// Rest or wake the selected site through its engine (M5, m5-1, m5-3;
/// rest is confirmed in the window first, D-07). Resting needs the engine:
/// `docker exec` here or over SSH, the API for a paired site. A failed
/// freeze keeps the site awake, and the error says why.
#[tauri::command]
async fn engine_rest(rest: bool, sel: State<'_, Selected>) -> Result<String, String> {
    require_experimental()?;
    let (verb, word) = if rest {
        (AdminVerb::Rest, "rest")
    } else {
        (AdminVerb::Wake, "wake")
    };
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => {
            let v = if rest { Verb::Rest } else { Verb::Wake };
            blocking(move || paired::act(&site, v)).await
        }
        Target::Local(s) => {
            blocking(move || {
                let info = engine_admin::probe(&s);
                EngineAdmin::new(&engine_admin::LocalDocker, info.as_ref(), word)
                    .and_then(|a| a.run(verb))
                    .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let s = session(&site)?;
                let info = s.engine_info().map_err(err)?;
                EngineAdmin::new(&s, info.as_ref(), word)
                    .and_then(|a| a.run(verb))
                    .map_err(err)
            })
            .await
        }
    }
}

/// A new wake code for the selected site (shown once in the window), or
/// none with `revoke` (m5-3, D-10).
#[tauri::command]
async fn engine_wake_code(
    revoke: bool,
    sel: State<'_, Selected>,
) -> Result<Option<String>, String> {
    require_experimental()?;
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => {
            blocking(move || paired::wake_code(&site, revoke)).await
        }
        Target::Local(s) => {
            blocking(move || {
                engine_admin::wake_code(
                    &engine_admin::LocalDocker,
                    engine_admin::probe(&s).as_ref(),
                    revoke,
                )
                .map_err(err)
            })
            .await
        }
        Target::Remote(site) => {
            blocking(move || {
                let s = session(&site)?;
                let info = s.engine_info().map_err(err)?;
                engine_admin::wake_code(&s, info.as_ref(), revoke).map_err(err)
            })
            .await
        }
    }
}

/// Remove the appliance's containers. With `wipe = true` this also deletes the
/// data volumes (database, files, admin password) — a harder teardown than a
/// plain stop, but lighter than a full reinstall. Quick, so no progress feed.
#[tauri::command]
async fn do_down(wipe: bool, sel: State<'_, Selected>) -> Result<(), String> {
    let s = local_stack(&sel, "Removing the containers")?;
    blocking(move || ops::down(&s, wipe).map_err(err)).await
}

/// Read back the most recent appliance log lines for the Activity view.
/// `service` narrows to one container (`app`/`db`); `None` shows everything.
#[tauri::command]
async fn get_logs(
    service: Option<String>,
    lines: Option<usize>,
    sel: State<'_, Selected>,
) -> Result<String, String> {
    let s = local_stack(&sel, "The activity log")?;
    let n = lines.unwrap_or(400);
    blocking(move || ops::tail_logs(&s, service.as_deref(), n).map_err(err)).await
}

/// Open a native folder picker for the static-export destination. Returns the
/// chosen directory (or `None` if the user cancelled).
#[tauri::command]
async fn pick_export_dir(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    app.dialog()
        .file()
        .set_title("Choose where to save your site")
        .pick_folder(move |picked| {
            let path = picked.and_then(|p| p.into_path().ok());
            let _ = tx.blocking_send(path.map(|p| p.to_string_lossy().into_owned()));
        });
    Ok(rx.recv().await.flatten())
}

/// Export the built site to static HTML — the deploy-anywhere artifact behind
/// `atelier site export`. Streams progress through the same reporter as the
/// lifecycle ops and returns the host output directory.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn site_export(
    app: AppHandle,
    out: Option<String>,
    base_url: Option<String>,
    zip: bool,
    include_config: bool,
    include_users: bool,
    skip_link_check: bool,
    sel: State<'_, Selected>,
) -> Result<String, String> {
    let s = match target(&sel)? {
        Target::Local(s) => s,
        // On a server the export is written there, where its own CLI puts
        // it; the returned line says where (`host:path`).
        Target::Remote(site) => {
            return blocking(move || {
                let host = host_of(&site);
                let opts = ops::ExportOptions {
                    out: None,
                    base_url: base_url.filter(|u| !u.trim().is_empty()),
                    zip,
                    include_config,
                    include_users,
                    skip_link_check,
                };
                let mut written = None;
                let mut sink = feed(&app);
                session(&site)?
                    .run_action(&remote::Action::Export(opts), &mut |pipe, line| {
                        let plain = remote::strip_ansi(line);
                        if let Some((_, path)) = plain.split_once("Static site at ") {
                            written = Some(path.trim().to_string());
                        }
                        sink(pipe, line)
                    })
                    .map_err(err)?;
                Ok(format!(
                    "{host}:{}",
                    written.unwrap_or_else(|| "(see the log)".to_string())
                ))
            })
            .await;
        }
    };
    blocking(move || {
        // The picker asks "where to save it" — the answer is a *parent* folder.
        // The site goes into a named subfolder, so the engine's guard against
        // replacing anything but a previous export never sees the user's own folder.
        let opts = ops::ExportOptions {
            out: out.map(|d| PathBuf::from(d).join(ops::EXPORT_DIR_NAME)),
            base_url: base_url.filter(|u| !u.trim().is_empty()),
            zip,
            include_config,
            include_users,
            skip_link_check,
        };
        ops::export_static(&s, &opts, &mut EventReporter { app })
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(err)
    })
    .await
}

/// List each Atelier model role and the provider/model it's bound to. Needs the
/// appliance running (it asks Drupal), so it may fail with a friendly error the
/// AI view surfaces as "start your site first."
#[tauri::command]
async fn get_model_roles(sel: State<'_, Selected>) -> Result<Vec<ModelRole>, String> {
    let s = local_stack(&sel, "AI model roles")?;
    blocking(move || ops::model_list(&s).map_err(err)).await
}

/// Bind one role (reasoning|task|fast) to a provider + model.
#[tauri::command]
async fn set_model_role(
    role: String,
    provider: String,
    model: String,
    sel: State<'_, Selected>,
) -> Result<(), String> {
    let s = local_stack(&sel, "AI model roles")?;
    blocking(move || ops::model_set(&s, &role, &provider, &model).map_err(err)).await
}

/// The manager's own version — distinct from the appliance image's version,
/// which is what `get_status`/`get_update` report.
///
/// Surfaced in the window chrome so it lands in any screenshot, including ones
/// taken from the Docker-not-ready and first-run screens where a report is most
/// likely to start. Baked in at compile time, so it needs no Docker and can't
/// disagree with the binary that's running.
#[tauri::command]
fn manager_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Diagnose the appliance — host, stack and site — without changing anything.
/// Runs off the UI thread: it shells several docker/drush probes.
///
/// For a site on a server: the checks that run here (the key, the SSH login,
/// the server's CLI), then the server's own report (m2-9).
#[tauri::command]
async fn run_doctor(app: AppHandle, sel: State<'_, Selected>) -> Result<doctor::Report, String> {
    match target(&sel)? {
        Target::Local(s) => blocking(move || Ok(doctor::diagnose(&s))).await,
        Target::Remote(site) => {
            blocking(move || remote::doctor(&site, false, &mut feed(&app)).map_err(err)).await
        }
    }
}

/// Run the safe repair ladder, then re-diagnose. Progress (including a streamed
/// `converge` run, which can take minutes) goes through the same `op-progress`
/// feed the lifecycle ops use, so the UI shows the existing overlay.
///
/// Nothing here deletes data — the destructive recoveries (remove, reinstall,
/// restore) stay behind their own confirmations elsewhere in the UI.
#[tauri::command]
async fn run_doctor_fix(
    app: AppHandle,
    sel: State<'_, Selected>,
) -> Result<doctor::Report, String> {
    match target(&sel)? {
        Target::Local(s) => blocking(move || Ok(doctor::fix(&s, &mut EventReporter { app }))).await,
        Target::Remote(site) => {
            blocking(move || remote::doctor(&site, true, &mut feed(&app)).map_err(err)).await
        }
    }
}

/// The SSH tunnel to the console of the remote site this window shows, when
/// it has none recorded (m2-4, D-02). Held until the window switches site or
/// the Manager quits; dropping it closes the tunnel.
#[derive(Default)]
struct ConsoleTunnel(Mutex<Option<remote::Tunnel>>);

impl ConsoleTunnel {
    fn close(&self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
    }
}

/// A remote site's console: its recorded domain, or a tunnel kept in
/// [`ConsoleTunnel`]. An open, live tunnel is reused rather than doubled.
async fn open_remote_console(app: AppHandle, site: Site) -> Result<(), String> {
    blocking(move || {
        let held = app.state::<ConsoleTunnel>();
        {
            let mut guard = held.0.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(tunnel) = guard.as_mut() {
                if tunnel.is_alive() {
                    return ops::open_url(&tunnel.url).map_err(err);
                }
                guard.take();
            }
        }
        let access = remote::console_access_in(&session(&site)?, &site).map_err(err)?;
        ops::open_url(&access.url).map_err(err)?;
        if let Some(tunnel) = access.tunnel {
            *held.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(tunnel);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn open_console(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => ops::open_url(&format!(
            "{}/atelier",
            site.engine_url().unwrap_or_default()
        ))
        .map_err(err),
        Target::Local(s) => ops::open_console(&s).map_err(err),
        Target::Remote(site) => open_remote_console(app, site).await,
    }
}

/// Open the console signed in — mints a one-time login link inside the
/// container (a `drush` exec, so it runs off the UI thread) and opens it, so a
/// fresh operator lands in `/atelier` authenticated instead of access-denied.
/// A site on a server opens at its domain or through a tunnel (sign in there).
#[tauri::command]
async fn open_console_authed(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => ops::open_url(&format!(
            "{}/atelier",
            site.engine_url().unwrap_or_default()
        ))
        .map_err(err),
        Target::Local(s) => blocking(move || ops::open_console_authed(&s).map_err(err)).await,
        Target::Remote(site) => open_remote_console(app, site).await,
    }
}

/// Open a folder (the exported site) in the OS file manager. Shells out to the
/// platform opener, exactly as the core does for URLs — no extra plugin needed.
#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer")
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(&path);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open the folder: {e}"))
}

#[tauri::command]
async fn open_login(app: AppHandle, sel: State<'_, Selected>) -> Result<(), String> {
    match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => ops::open_url(&format!(
            "{}/atelier",
            site.engine_url().unwrap_or_default()
        ))
        .map_err(err),
        Target::Local(s) => ops::open_login(&s).map_err(err),
        Target::Remote(site) => open_remote_console(app, site).await,
    }
}

/// Open an external URL (docs/guides) in the default browser. Plain
/// `<a target="_blank">` does nothing inside the Tauri WebView, so the
/// frontend routes external links through here — the same platform opener the
/// core uses for the console.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    ops::open_url(&url).map_err(err)
}

// --- sites -------------------------------------------------------------------

/// One row of the header's site switcher: what the registry knows, plus whether
/// the site is running. A site on a server carries its `host` (in `row`) and
/// is never probed here — the window asks [`probe_site`] for each one after
/// the list painted, so a slow server can't hold the switcher up.
#[derive(Serialize)]
struct SiteView {
    #[serde(flatten)]
    row: SiteRow,
    running: bool,
}

#[derive(Serialize)]
struct SitesView {
    /// The slug this window operates on, when it is a registered site.
    selected: Option<String>,
    sites: Vec<SiteView>,
}

/// Every registered site (plus an installed, not yet adopted `~/.atelier`) and
/// which one this window shows. One `compose ps` per installed site when Docker
/// is up — no HTTP probes — so it's cheap enough to run on every refresh.
#[tauri::command]
async fn list_sites(sel: State<'_, Selected>) -> Result<SitesView, String> {
    let chosen = sel.get();
    blocking(move || {
        let user_home = sites::user_home().map_err(err)?;
        let rows = sites::list_sites(&user_home).map_err(err)?;
        let docker_up = rows.iter().any(|r| r.installed) && aincient_core::preflight().ok();
        // Nothing chosen yet (a fresh machine that has since installed, or a
        // legacy `~/.atelier`): show whichever row the fallback stack is.
        let selected = chosen.or_else(|| {
            let home = Stack::locate().ok()?.home;
            rows.iter().find(|r| r.home == home).map(|r| r.slug.clone())
        });
        let sites = rows
            .into_iter()
            .map(|row| {
                let running = docker_up && row.installed && ops::is_running(&row.stack());
                SiteView { row, running }
            })
            .collect();
        Ok(SitesView { selected, sites })
    })
    .await
}

/// Switch this window to `slug`, and make it the registry's active site, so the
/// Manager reopens on it and a bare `atelier` command in a terminal agrees.
#[tauri::command]
async fn select_site(
    slug: String,
    sel: State<'_, Selected>,
    tunnel: State<'_, ConsoleTunnel>,
) -> Result<(), String> {
    let chosen = slug.clone();
    blocking(move || {
        let user_home = sites::user_home().map_err(err)?;
        sites::use_site(&user_home, &chosen)
            .map(|_| ())
            .map_err(err)
    })
    .await?;
    if sel.get().as_deref() != Some(slug.as_str()) {
        tunnel.close();
    }
    sel.set(slug);
    Ok(())
}

/// Register a new site on a free port and switch to it. Lays down its stack
/// files only; the window then shows the set-up screen for it, exactly as on a
/// first run.
#[tauri::command]
async fn add_site(slug: String, label: String, sel: State<'_, Selected>) -> Result<(), String> {
    let chosen = slug.clone();
    blocking(move || {
        let user_home = sites::user_home().map_err(err)?;
        let label = Some(label.trim()).filter(|l| !l.is_empty());
        sites::add_site(&user_home, &chosen, label, None, sites::host_port_is_free).map_err(err)?;
        sites::use_site(&user_home, &chosen)
            .map(|_| ())
            .map_err(err)
    })
    .await?;
    sel.set(slug);
    Ok(())
}

/// The guided key for `host` (D-05): the one in the OS keychain, or a new one
/// stored there — with the `authorized_keys` line and a command that adds it.
#[derive(Serialize)]
struct GuidedKeyView {
    public_line: String,
    command: String,
    created: bool,
}

#[tauri::command]
async fn guided_key(host: String) -> Result<GuidedKeyView, String> {
    blocking(move || {
        let host = host.trim().to_string();
        if !sites::is_valid_host(&host) {
            return Err(format!(
                "`{host}` is not a usable SSH target (an alias, host or user@host)"
            ));
        }
        let key = ssh_key::ensure_guided_key(ssh_key::guided_keys(), &host).map_err(err)?;
        Ok(GuidedKeyView {
            command: ssh_key::authorize_command(&key.public_line),
            public_line: key.public_line,
            created: key.created,
        })
    })
    .await
}

/// Attach a site already running on a server (U-03) and switch to it — the
/// same entry and check as `atelier sites add <SLUG> --host …`: the registry
/// is written only after the server answered and reported the site running.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn add_remote_site(
    slug: String,
    label: String,
    host: String,
    remote_site: Option<String>,
    domain: Option<String>,
    guided: bool,
    sel: State<'_, Selected>,
    tunnel: State<'_, ConsoleTunnel>,
) -> Result<(), String> {
    let chosen = slug.clone();
    blocking(move || {
        let user_home = sites::user_home().map_err(err)?;
        let label = Some(label.trim()).filter(|l| !l.is_empty());
        let key = if guided {
            sites::KeyMode::Guided
        } else {
            sites::KeyMode::Ssh
        };
        let site = sites::remote_entry(
            &user_home,
            &chosen,
            label,
            &host,
            remote_site.as_deref(),
            key,
            domain.as_deref(),
        )
        .map_err(err)?;
        sites::add_remote_site(&user_home, site, |site| {
            remote::open(site, &remote::EngineOptions::from_env())?.attach_check()?;
            Ok(())
        })
        .map_err(err)?;
        sites::use_site(&user_home, &chosen)
            .map(|_| ())
            .map_err(err)
    })
    .await?;
    tunnel.close();
    sel.set(slug);
    Ok(())
}

/// Change a site's display name. Its slug, folder and data stay as they are.
#[tauri::command]
async fn rename_site(slug: String, label: String) -> Result<(), String> {
    blocking(move || {
        let user_home = sites::user_home().map_err(err)?;
        sites::rename_site(&user_home, &slug, &label)
            .map(|_| ())
            .map_err(err)
    })
    .await
}

// --- pairing without SSH (M4, E-02; experimental) ----------------------------

/// What this Manager calls itself when it pairs, unless the owner types
/// another.
#[tauri::command]
async fn manager_name() -> Result<String, String> {
    blocking(|| Ok(aincient_core::pairing::default_name())).await
}

/// "Add a site → On a server → With an install code" (m4-2): pair, keep the
/// token in the keychain, register and switch to it.
#[tauri::command]
async fn pair_with_install_code(
    slug: String,
    label: String,
    url: String,
    code: String,
    name: String,
    sel: State<'_, Selected>,
    tunnel: State<'_, ConsoleTunnel>,
) -> Result<(), String> {
    require_experimental()?;
    let chosen = slug.clone();
    blocking(move || paired::add_with_install_code(&slug, &label, &url, &code, &name)).await?;
    tunnel.close();
    sel.set(chosen);
    Ok(())
}

/// "Add a site → On a server → Ask to join" (m4-5): the code to show.
#[tauri::command]
async fn join_start(
    slug: String,
    label: String,
    url: String,
    name: String,
    joining: State<'_, paired::Joining>,
) -> Result<paired::JoinView, String> {
    require_experimental()?;
    let j = joining.inner().clone();
    // One short request; the waiting is the window's polling.
    blocking(move || paired::join_start(&j, &slug, &label, &url, &name)).await
}

/// One poll of the join this window waits on.
#[tauri::command]
async fn join_poll(
    joining: State<'_, paired::Joining>,
    sel: State<'_, Selected>,
    tunnel: State<'_, ConsoleTunnel>,
) -> Result<paired::Polled, String> {
    require_experimental()?;
    let j = joining.inner().clone();
    let polled = blocking(move || paired::join_poll(&j)).await?;
    if let paired::Polled::Paired { slug } = &polled {
        tunnel.close();
        sel.set(slug.clone());
    }
    Ok(polled)
}

#[tauri::command]
fn join_cancel(joining: State<'_, paired::Joining>) {
    paired::join_cancel(joining.inner());
}

/// The selected engine-paired site, or a refusal.
fn paired_site(sel: &Selected) -> Result<Site, String> {
    require_experimental()?;
    match target(sel)? {
        Target::Remote(site) if is_paired(&site) => Ok(site),
        _ => Err("This site isn't paired with its engine by this Manager.".into()),
    }
}

/// The Managers panel (m4-3, m4-5).
#[tauri::command]
async fn engine_managers(sel: State<'_, Selected>) -> Result<paired::ManagersView, String> {
    let site = paired_site(&sel)?;
    blocking(move || paired::managers(&site)).await
}

/// On the primary: approve the request whose code the owner TYPED (the
/// window never fills it in). Returns the approved Manager's name.
#[tauri::command]
async fn engine_approve(code: String, sel: State<'_, Selected>) -> Result<String, String> {
    let site = paired_site(&sel)?;
    blocking(move || paired::approve(&site, &code)).await
}

#[tauri::command]
async fn engine_revoke(id: String, sel: State<'_, Selected>) -> Result<(), String> {
    let site = paired_site(&sel)?;
    blocking(move || paired::revoke(&site, &id)).await
}

/// "Forget this site": revoke this Manager's pairing, delete its token,
/// drop the site. Allowed with Experimental features off too, like
/// removing the engine: leaving must never be hidden behind the opt-in.
#[tauri::command]
async fn engine_forget(sel: State<'_, Selected>) -> Result<(), String> {
    let site = match target(&sel)? {
        Target::Remote(site) if is_paired(&site) => site,
        _ => return Err("This site isn't paired with its engine.".into()),
    };
    blocking(move || paired::forget(&site)).await?;
    *sel.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Selected::from_resolution())
        .manage(paired::Joining::default())
        .manage(ConsoleTunnel::default())
        .invoke_handler(tauri::generate_handler![
            check_preflight,
            preflight_problem,
            get_status,
            change_guard,
            probe_site,
            get_update,
            get_cached_update,
            refresh_update_cache,
            list_backups,
            set_admin_password,
            do_install,
            do_update,
            do_backup,
            check_restore,
            do_restore,
            pick_restore_file,
            export_backup,
            do_reinstall,
            do_start,
            do_stop,
            do_down,
            get_logs,
            pick_export_dir,
            site_export,
            get_model_roles,
            set_model_role,
            set_channel,
            run_doctor,
            run_doctor_fix,
            manager_version,
            reveal_path,
            open_console,
            open_console_authed,
            open_login,
            open_url,
            list_sites,
            select_site,
            add_site,
            add_remote_site,
            guided_key,
            rename_site,
            get_settings,
            set_experimental,
            engine_state,
            engine_enable,
            engine_remove,
            engine_lockdown,
            engine_rest,
            engine_wake_code,
            manager_name,
            pair_with_install_code,
            join_start,
            join_poll,
            join_cancel,
            engine_managers,
            engine_approve,
            engine_revoke,
            engine_forget,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Atelier Manager")
        .run(|app, event| {
            // Quitting closes a console tunnel this window still holds.
            if let tauri::RunEvent::Exit = event {
                app.state::<ConsoleTunnel>().close();
            }
        });
}
