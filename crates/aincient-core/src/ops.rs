//! The lifecycle operations: status, install, update, backup, restore, …
//!
//! Each maps to a `docker compose` / `drush` primitive that the appliance already
//! uses, so the manager never invents behaviour the bootstrapper and `converge.sh`
//! don't already guarantee (idempotent install/upgrade, snapshot-format backups).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::docker::{
    self, compose, preflight, run_capture, run_inherited, run_streaming, try_capture,
};
use crate::stack::{InstallOptions, Stack};

/// drush, as invoked inside the `app` container.
const DRUSH: &[&str] = &["/opt/drupal/vendor/bin/drush", "--root=/opt/drupal/web"];

/// How long to wait for the console to finish booting after an install or
/// upgrade before giving up and telling the user it's still coming up. Generous:
/// a first-boot site install plus `converge.sh` migrations can run for minutes.
pub const READY_TIMEOUT: Duration = Duration::from_secs(240);

/// How long to wait after a plain `start` of an already-installed stack — no
/// fresh install or migration, so the boot is much shorter.
pub const START_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// The coarse phases a lifecycle op (install/update/start) moves through, in the
/// order a progress UI should show them. The container is "running" the moment
/// it starts, so [`Booting`](Stage::Booting) — Drupal finishing its boot — is the
/// long tail that used to be invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Checking Docker is installed and running.
    Preflight,
    /// Laying down `compose.yaml` + `.env`.
    Scaffold,
    /// `docker compose pull` — downloading the image.
    Pull,
    /// `docker compose up`/`start` — bringing the containers up.
    Starting,
    /// Containers are up; waiting for Drupal to actually serve.
    Booting,
    /// The console answered — ready to use.
    Ready,
    /// A non-phased op with no measurable progress (backup, restore, stop,
    /// password) — the UI shows an indeterminate bar and the step log.
    Working,
}

/// Receives progress from a long-running op so a front-end can render it. The
/// CLI prints stage headlines; the GUI relays them to a progress bar + log feed.
/// Both share the exact same op code — only the presentation differs.
pub trait Reporter {
    /// A milestone: advance to `fraction` (0.0–1.0, when known) with a headline.
    fn stage(&mut self, _stage: Stage, _message: &str, _fraction: Option<f32>) {}
    /// A pass-through log line from the underlying tool (docker), no bar change.
    fn log(&mut self, _line: &str) {}
    /// Whether docker's output should be captured and relayed via [`log`](Self::log)
    /// (a GUI feed), or left to inherit the terminal (the CLI). Default: inherit.
    fn captures_output(&self) -> bool {
        false
    }
}

/// A [`Reporter`] that drops everything — for callers that don't show progress.
pub struct Silent;
impl Reporter for Silent {}

/// A snapshot of the appliance's current state.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// Has a stack been laid down (`compose.yaml` present)?
    pub installed: bool,
    /// Is the `app` container running?
    pub running: bool,
    /// Is the console actually serving (Drupal answered HTTP with status < 500)?
    /// Distinct from `running`: the container reports "running" the instant it
    /// starts, while Drupal keeps booting and serves 5xx for a while after.
    pub reachable: bool,
    pub console_url: String,
    pub image: String,
    /// Local image digest (best effort).
    pub image_digest: Option<String>,
}

/// The result of comparing the local image against the registry.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheck {
    pub image: String,
    /// Local pulled digest.
    pub current: Option<String>,
    /// Registry digest for the same tag.
    pub latest: Option<String>,
    /// `Some(true)` if an update is available, `None` if it couldn't be determined.
    pub update_available: Option<bool>,
}

/// A backup file on the host.
#[derive(Debug, Clone, Serialize)]
pub struct Backup {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_unix: u64,
}

/// One AIncient model role and its binding, as `drush aincient:model-list` reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRole {
    pub role: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    /// "yes" when this is the default role the console inherits, else "".
    #[serde(default)]
    pub default: String,
}

impl ModelRole {
    /// Whether this is the default role the chat console inherits.
    pub fn is_default(&self) -> bool {
        self.default == "yes"
    }
}

#[derive(Deserialize)]
struct PsEntry {
    #[serde(rename = "Service", default)]
    service: String,
    #[serde(rename = "State", default)]
    state: String,
}

/// Current appliance status. Cheap probes only; never mutates the stack.
pub fn status(stack: &Stack) -> Status {
    let installed = stack.exists();
    let pf = preflight();

    let mut running = false;
    if installed && pf.ok() {
        let mut c = compose(stack);
        c.args(["ps", "--format", "json"]);
        if let Some(out) = try_capture(c) {
            running = parse_ps(&out)
                .iter()
                .any(|e| e.service == "app" && e.state.to_lowercase().contains("running"));
        }
    }

    let reachable = running && http_ready(stack.http_port());

    Status {
        installed,
        running,
        reachable,
        console_url: stack.console_url(),
        image: stack.image(),
        image_digest: local_digest(&stack.image()),
    }
}

/// Poll the console until it serves a healthy response, or `timeout` elapses,
/// reporting boot progress as it goes. Returns `true` once it's ready.
///
/// This is what holds the manager's "ready" signal back until Drupal has
/// actually finished booting (site install, `converge.sh` migrations) instead of
/// the moment the container reports "running".
fn wait_until_ready(stack: &Stack, timeout: Duration, r: &mut dyn Reporter) -> bool {
    let port = stack.http_port();
    let started = Instant::now();
    let deadline = started + timeout;
    r.stage(Stage::Booting, "Waiting for the console to finish booting…", Some(0.72));
    loop {
        if http_ready(port) {
            r.stage(Stage::Ready, "The console is up.", Some(1.0));
            return true;
        }
        if Instant::now() >= deadline {
            r.stage(
                Stage::Booting,
                "Still booting — this is taking longer than usual.",
                Some(0.95),
            );
            return false;
        }
        // Creep the bar across the boot window (0.72 → 0.95) and tick the timer.
        let elapsed = started.elapsed();
        let frac = 0.72 + 0.23 * (elapsed.as_secs_f32() / timeout.as_secs_f32());
        r.stage(
            Stage::Booting,
            &format!("Booting… ({}s)", elapsed.as_secs()),
            Some(frac.min(0.95)),
        );
        std::thread::sleep(Duration::from_millis(750));
    }
}

/// Run a chatty step, relaying its output to the reporter (GUI) or letting it
/// inherit the terminal (CLI), per [`Reporter::captures_output`].
fn run_step(cmd: Command, action: &str, r: &mut dyn Reporter) -> Result<()> {
    if r.captures_output() {
        run_streaming(cmd, action, |line| r.log(line))
    } else {
        run_inherited(cmd, action)
    }
}

/// Compare the local image to the registry tag. Best effort — returns
/// `update_available: None` when the registry can't be reached.
pub fn check_update(stack: &Stack) -> UpdateCheck {
    let image = stack.image();
    let current = local_digest(&image);
    let latest = remote_digest(&image);
    let update_available = match (&current, &latest) {
        (Some(c), Some(l)) => Some(c != l),
        _ => None,
    };
    UpdateCheck {
        image,
        current,
        latest,
        update_available,
    }
}

/// Lay down the stack (if needed), pull the image, and start it. Idempotent:
/// re-running an existing stack is an upgrade — `converge.sh` migrates in place
/// and auto-rolls-back on failure. Returns whether the console came up before
/// the readiness timeout (`false` = started but still booting).
pub fn install(stack: &Stack, opts: &InstallOptions, r: &mut dyn Reporter) -> Result<bool> {
    r.stage(Stage::Preflight, "Checking Docker…", Some(0.04));
    preflight().require()?;
    r.stage(Stage::Scaffold, "Preparing the stack…", Some(0.08));
    stack.ensure_scaffold(opts)?;
    pull(stack, r)?;
    up(stack, r)?;
    Ok(wait_until_ready(stack, READY_TIMEOUT, r))
}

/// Pull a newer image and recreate the stack — the upgrade path. Returns whether
/// the console came up before the readiness timeout.
pub fn update(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    ensure_installed(stack)?;
    r.stage(Stage::Preflight, "Checking Docker…", Some(0.04));
    preflight().require()?;
    pull(stack, r)?;
    up(stack, r)?;
    Ok(wait_until_ready(stack, READY_TIMEOUT, r))
}

/// `docker compose pull`, with a registry-login hint for the private image.
fn pull(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Pull, "Pulling the latest appliance image…", Some(0.12));
    let mut c = compose(stack);
    c.arg("pull");
    run_step(c, "pull the image", r).map_err(|e| {
        if stack.image().starts_with("ghcr.io/") {
            e.context(
                "If this is an authentication error, log in to the registry first:\n  \
                 echo \"<TOKEN>\" | docker login ghcr.io -u aincient-labs --password-stdin",
            )
        } else {
            e
        }
    })
}

/// `docker compose up -d`.
fn up(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Starting, "Starting the containers…", Some(0.6));
    let mut c = compose(stack);
    c.args(["up", "-d"]);
    run_step(c, "start the appliance", r)
}

/// `docker compose stop` — stop containers, keep data.
pub fn stop(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    ensure_installed(stack)?;
    r.stage(Stage::Working, "Stopping the appliance…", None);
    let mut c = compose(stack);
    c.arg("stop");
    run_step(c, "stop the appliance", r)
}

/// `docker compose start` — start previously-stopped containers. Returns whether
/// the console came up before the readiness timeout.
pub fn start(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    ensure_installed(stack)?;
    r.stage(Stage::Starting, "Starting the appliance…", Some(0.4));
    let mut c = compose(stack);
    c.arg("start");
    run_step(c, "start the appliance", r)?;
    Ok(wait_until_ready(stack, START_READY_TIMEOUT, r))
}

/// `docker compose down`. With `wipe_data`, also removes the volumes — a
/// destructive reset (database, files, and the saved admin password). Callers
/// must confirm with the user first.
pub fn down(stack: &Stack, wipe_data: bool) -> Result<()> {
    ensure_installed(stack)?;
    let mut c = compose(stack);
    c.arg("down");
    if wipe_data {
        c.arg("-v");
    }
    run_inherited(c, "stop and remove the appliance")
}

/// Wipe everything and install from scratch. Destructive — confirm first.
/// Returns whether the console came up before the readiness timeout.
pub fn reinstall(stack: &Stack, opts: &InstallOptions, r: &mut dyn Reporter) -> Result<bool> {
    preflight().require()?;
    if stack.exists() {
        r.stage(Stage::Scaffold, "Removing the old appliance…", Some(0.02));
        down(stack, true)?;
    }
    install(stack, opts, r)
}

/// The uploaded-files tree inside the `app` container — the `files:` volume from
/// `compose.yaml`, holding user uploads and generated image derivatives.
const FILES_DIR: &str = "/opt/drupal/web/sites/default/files";

/// Back up the whole appliance to a single portable `.tar.gz` snapshot on the
/// host: the database (`drush sql:dump --gzip`, converge's format) **plus** the
/// uploaded-files tree, alongside a `manifest.json`. Self-contained, so the
/// archive can be shared over the wire and [`restore`]d onto another host.
/// Returns the path to the created archive.
pub fn backup(stack: &Stack, label: Option<&str>, r: &mut dyn Reporter) -> Result<PathBuf> {
    r.stage(Stage::Working, "Backing up the database and files…", None);
    ensure_running(stack)?;
    std::fs::create_dir_all(stack.backups_dir())?;

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = match label {
        Some(l) if !l.is_empty() => format!("{}-{ts}", sanitize(l)),
        _ => ts.clone(),
    };
    let host_path = stack.backups_dir().join(format!("aincient-{stem}.tar.gz"));

    let drush = DRUSH.join(" ");
    // A manifest identifying the archive as an AIncient snapshot and pinning the
    // image it was taken from — so a future restore can warn on a version skew.
    // JSON has no single quotes, so it's safe inside the printf's single-quoted arg.
    let manifest = format!(
        r#"{{"format":"aincient-snapshot","version":1,"created":"{ts}","image":"{image}"}}"#,
        image = stack.image(),
    );

    // Build the bundle inside the container, then copy it out (mirrors the
    // dump→cp→rm pattern).
    r.log("Dumping the database and packing uploaded files…");
    let script = backup_script(&drush, &manifest);
    let mut build = compose(stack);
    build.args(["exec", "-T", "app", "sh", "-c", &script]);
    run_capture(build, "build the snapshot archive")?;

    r.log("Copying the snapshot out of the container…");
    let mut cp = compose(stack);
    cp.args(["cp", "app:/tmp/aincient-snapshot.tar.gz", &host_path.to_string_lossy()]);
    run_capture(cp, "copy the snapshot out of the container")?;

    // Best-effort cleanup of the in-container temp files.
    let mut rm = compose(stack);
    rm.args([
        "exec", "-T", "app", "rm", "-rf",
        "/tmp/aincient-snapshot", "/tmp/aincient-snapshot.tar.gz",
    ]);
    let _ = rm.output();

    r.log(&format!("Snapshot written to {}", host_path.display()));
    Ok(host_path)
}

/// Where the static export is staged inside the `app` container before it's
/// copied out to the host.
const EXPORT_CONTAINER_DIR: &str = "/tmp/aincient-site-export";

/// Options for [`export_static`] — a thin passthrough onto the appliance's
/// `drush aincient:export` (the static-site exporter). Every field maps to a
/// flag the exporter already understands, so the manager invents no behaviour.
#[derive(Debug, Default, Clone)]
pub struct ExportOptions {
    /// Host directory to write the static site into. Defaults to
    /// `./aincient-export` in the caller's current directory.
    pub out: Option<PathBuf>,
    /// Scheme + host to render absolute links against (drush `--base-url`).
    /// `None` lets the exporter use its own default.
    pub base_url: Option<String>,
    /// Also package a `.zip` beside the exported site.
    pub zip: bool,
    /// Add `config/sync` to the zip (a portable "own your data" bundle).
    pub include_config: bool,
    /// Add `users.json` (accounts without password hashes) to the zip.
    pub include_users: bool,
    /// Skip the exporter's post-export link check.
    pub skip_link_check: bool,
}

/// Export the public site to static HTML on the host — the deploy-anywhere
/// artifact behind `atelier site export`. Runs the appliance's
/// `drush aincient:export` inside the `app` container (staging into a temp dir),
/// then copies the result out. Returns the host output directory.
pub fn export_static(stack: &Stack, opts: &ExportOptions, r: &mut dyn Reporter) -> Result<PathBuf> {
    r.stage(Stage::Working, "Exporting the site to static HTML…", None);
    ensure_running(stack)?;

    let host_out = match &opts.out {
        Some(p) => p.clone(),
        None => std::env::current_dir()
            .context("failed to read the current directory")?
            .join("aincient-export"),
    };
    if let Some(parent) = host_out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let container_zip = format!("{EXPORT_CONTAINER_DIR}.zip");

    // Assemble the drush invocation, rendering into a clean container staging dir.
    let mut export_args: Vec<String> = DRUSH.iter().map(|s| (*s).to_string()).collect();
    export_args.push("aincient:export".into());
    export_args.push(format!("--out={EXPORT_CONTAINER_DIR}"));
    if let Some(base) = &opts.base_url {
        export_args.push(format!("--base-url={base}"));
    }
    if opts.zip {
        export_args.push(format!("--zip={container_zip}"));
    }
    if opts.include_config {
        export_args.push("--include-config".into());
    }
    if opts.include_users {
        export_args.push("--include-users".into());
    }
    if opts.skip_link_check {
        export_args.push("--skip-link-check".into());
    }

    // Clear any stale staging dir so a re-export is clean.
    let mut clean = compose(stack);
    clean.args([
        "exec", "-T", "app", "rm", "-rf",
        EXPORT_CONTAINER_DIR, container_zip.as_str(),
    ]);
    let _ = clean.output();

    r.log("Rendering pages to static HTML…");
    let mut build = compose(stack);
    build.args(["exec", "-T", "app"]);
    build.args(&export_args);
    // The exporter reports page/asset counts and any broken links — surface them.
    run_step(build, "export the static site", r)?;

    r.log("Copying the exported site out of the container…");
    // `docker compose cp` copies the source dir *as* the destination — so remove
    // an existing target first, otherwise the export nests inside it.
    let _ = std::fs::remove_dir_all(&host_out);
    let mut cp = compose(stack);
    cp.args(["cp", &format!("app:{EXPORT_CONTAINER_DIR}"), &host_out.to_string_lossy()]);
    run_capture(cp, "copy the exported site out of the container")?;

    if opts.zip {
        let host_zip = host_out.with_extension("zip");
        let mut cpz = compose(stack);
        cpz.args(["cp", &format!("app:{container_zip}"), &host_zip.to_string_lossy()]);
        run_capture(cpz, "copy the export zip out of the container")?;
    }

    // Best-effort cleanup of the in-container staging files.
    let mut rm = compose(stack);
    rm.args([
        "exec", "-T", "app", "rm", "-rf",
        EXPORT_CONTAINER_DIR, container_zip.as_str(),
    ]);
    let _ = rm.output();

    r.log(&format!("Static site exported to {}", host_out.display()));
    Ok(host_out)
}

/// Restore the appliance from a host backup file. Destructive — confirm first.
///
/// A `.tar.gz` **snapshot bundle** (from [`backup`]) restores the database
/// *and* the uploaded-files tree; a legacy `.sql`/`.sql.gz` dump restores the
/// database only. Both mirror converge's `restore_snapshot` for the DB: drop,
/// load, rebuild caches.
pub fn restore(stack: &Stack, file: &Path, r: &mut dyn Reporter) -> Result<()> {
    ensure_running(stack)?;
    if !file.is_file() {
        bail!("backup file not found: {}", file.display());
    }
    let name = file.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    if is_snapshot_bundle(name) {
        return restore_bundle(stack, file, r);
    }

    r.stage(Stage::Working, "Restoring the database…", None);
    let gzipped = file
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gz"))
        .unwrap_or(false);
    let container_tmp = if gzipped {
        "/tmp/aincient-restore.sql.gz"
    } else {
        "/tmp/aincient-restore.sql"
    };

    r.log("Copying the backup into the container…");
    let mut cp = compose(stack);
    cp.args(["cp", &file.to_string_lossy(), &format!("app:{container_tmp}")]);
    run_capture(cp, "copy the backup into the container")?;

    r.log("Dropping the current database…");
    let mut drop = compose(stack);
    drop.args(["exec", "-T", "app"]).args(DRUSH).args(["sql:drop", "-y"]);
    run_capture(drop, "drop the existing database")?;

    r.log("Loading the backup…");
    let drush = DRUSH.join(" ");
    let load_cmd = if gzipped {
        format!("zcat {container_tmp} | {drush} sql:cli")
    } else {
        format!("cat {container_tmp} | {drush} sql:cli")
    };
    let mut load = compose(stack);
    load.args(["exec", "-T", "app", "sh", "-c", &load_cmd]);
    run_capture(load, "load the backup")?;

    r.log("Rebuilding caches…");
    let mut cr = compose(stack);
    cr.args(["exec", "-T", "app"]).args(DRUSH).arg("cache:rebuild");
    let _ = cr.output();

    let mut rm = compose(stack);
    rm.args(["exec", "-T", "app", "rm", "-f", container_tmp]);
    let _ = rm.output();

    Ok(())
}

/// Restore a full snapshot bundle (`.tar.gz` from [`backup`]): database + files.
/// The whole thing runs as one in-container script so the DB load and the files
/// swap stay together. Files are rewritten as root, so we re-assert
/// `www-data` ownership afterwards — exactly as `entrypoint.sh` does on boot —
/// or restored uploads and their derivatives would be unwritable/unreadable.
fn restore_bundle(stack: &Stack, file: &Path, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Working, "Restoring the database and files…", None);

    r.log("Copying the snapshot into the container…");
    let mut cp = compose(stack);
    cp.args(["cp", &file.to_string_lossy(), "app:/tmp/aincient-restore.tar.gz"]);
    run_capture(cp, "copy the snapshot into the container")?;

    r.log("Unpacking and restoring database + files…");
    let script = restore_bundle_script(&DRUSH.join(" "));
    let mut run = compose(stack);
    run.args(["exec", "-T", "app", "sh", "-c", &script]);
    run_capture(run, "restore the snapshot")?;

    Ok(())
}

/// The in-container shell that builds a snapshot bundle. `drush --gzip` appends
/// `.gz` to --result-file (see converge.sh). One `tar` with two `-C` changes
/// packs the staged DB dump + manifest, then the live files tree, so `files/`
/// lands at the archive root. `manifest` is JSON (no single quotes), so it's
/// safe inside the single-quoted `printf` argument.
fn backup_script(drush: &str, manifest: &str) -> String {
    format!(
        "set -e\n\
         STAGE=/tmp/aincient-snapshot\n\
         ARCHIVE=/tmp/aincient-snapshot.tar.gz\n\
         rm -rf \"$STAGE\" \"$ARCHIVE\"\n\
         mkdir -p \"$STAGE\"\n\
         {drush} sql:dump --gzip --result-file=\"$STAGE/database.sql\" >/dev/null\n\
         printf '%s\\n' '{manifest}' > \"$STAGE/manifest.json\"\n\
         tar czf \"$ARCHIVE\" -C \"$STAGE\" manifest.json database.sql.gz \
         -C /opt/drupal/web/sites/default files\n",
    )
}

/// The in-container shell that restores a snapshot bundle: drop + reload the DB,
/// then swap the files tree. `find -mindepth 1 -delete` clears the files dir
/// (dotfiles included) without removing the volume mount point; `cp -a`
/// preserves the tree; the `chown` re-asserts `www-data` ownership (files are
/// written as root) exactly as `entrypoint.sh` does. cache:rebuild and cleanup
/// are best-effort so a hiccup there doesn't fail the restore.
fn restore_bundle_script(drush: &str) -> String {
    format!(
        "set -e\n\
         ARCHIVE=/tmp/aincient-restore.tar.gz\n\
         WORK=/tmp/aincient-restore\n\
         DEST={FILES_DIR}\n\
         rm -rf \"$WORK\"\n\
         mkdir -p \"$WORK\"\n\
         tar xzf \"$ARCHIVE\" -C \"$WORK\"\n\
         {drush} sql:drop -y\n\
         zcat \"$WORK/database.sql.gz\" | {drush} sql:cli\n\
         if [ -d \"$WORK/files\" ]; then\n\
         \x20 find \"$DEST\" -mindepth 1 -delete 2>/dev/null || true\n\
         \x20 cp -a \"$WORK/files/.\" \"$DEST/\"\n\
         \x20 chown -R www-data:www-data \"$DEST\"\n\
         fi\n\
         {drush} cache:rebuild || true\n\
         rm -rf \"$WORK\" \"$ARCHIVE\" || true\n",
    )
}

/// True if `name` is a full snapshot bundle (database + files), vs. a legacy
/// DB-only `.sql`/`.sql.gz` dump.
fn is_snapshot_bundle(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".tar.gz") || n.ends_with(".tgz")
}

/// True if `name` is a restorable backup: a snapshot bundle or a legacy dump.
fn is_backup_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    is_snapshot_bundle(name) || n.ends_with(".sql.gz") || n.ends_with(".sql")
}

/// List host backups, newest first.
pub fn list_backups(stack: &Stack) -> Vec<Backup> {
    let dir = stack.backups_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_backup_file(&name) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Backup {
            name,
            path: path.to_string_lossy().into_owned(),
            size_bytes: meta.len(),
            modified_unix,
        });
    }
    out.sort_by(|a, b| b.modified_unix.cmp(&a.modified_unix));
    out
}

/// Read the admin password generated on first boot, if it's still on the volume.
pub fn admin_password(stack: &Stack) -> Option<String> {
    let mut c = compose(stack);
    c.args([
        "exec",
        "-T",
        "app",
        "sh",
        "-c",
        "cat /opt/drupal/private/INITIAL_ADMIN_PASSWORD 2>/dev/null",
    ]);
    try_capture(c)
}

/// Set a new admin password.
pub fn set_admin_password(stack: &Stack, password: &str, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Working, "Setting the admin password…", None);
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["user:password", "admin", password]);
    run_capture(c, "set the admin password")?;
    r.log("Admin password updated.");
    Ok(())
}

/// List the AIncient model roles and their bindings.
///
/// Shells `drush aincient:model-list --format=json` inside the `app` container —
/// the same source of truth the console form and onboarding write.
pub fn model_list(stack: &Stack) -> Result<Vec<ModelRole>> {
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["aincient:model-list", "--format=json"]);
    let out = run_capture(c, "list the model roles")?;
    serde_json::from_str(out.trim()).context("could not parse the model-list output")
}

/// Bind a model role to a provider + model, then project it onto the framework.
///
/// Shells `drush aincient:model-set <role> <provider> <model>`; drush validates
/// the role + provider and returns a non-zero exit on a bad binding.
pub fn model_set(stack: &Stack, role: &str, provider: &str, model: &str) -> Result<()> {
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["aincient:model-set", role, provider, model]);
    run_capture(c, "set the model role").map(|_| ())
}

/// Build (but don't run) a `docker compose logs` command, so callers choose how
/// to wire stdout (the CLI inherits it; the GUI pipes it).
pub fn logs_command(stack: &Stack, follow: bool, service: Option<&str>) -> Command {
    let mut c = compose(stack);
    c.arg("logs");
    if follow {
        c.arg("-f");
    }
    if let Some(svc) = service {
        c.arg(svc);
    }
    c
}

/// Open the console (the chat workspace at `/atelier`) in the default browser.
pub fn open_console(stack: &Stack) -> Result<()> {
    open_url(&stack.console_url())
}

/// Open Drupal's login form (`/user/login`) in the default browser so the
/// operator can sign in directly — the manager never reveals the password.
pub fn open_login(stack: &Stack) -> Result<()> {
    open_url(&stack.login_url())
}

/// Open a URL in the user's default browser.
fn open_url(url: &str) -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.status()
        .with_context(|| format!("could not open {url} in a browser"))?;
    Ok(())
}

// --- internals --------------------------------------------------------------

fn ensure_installed(stack: &Stack) -> Result<()> {
    if !stack.exists() {
        bail!(
            "no Atelier stack found at {} — run `atelier app install` first",
            stack.home.display()
        );
    }
    Ok(())
}

fn ensure_running(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    preflight().require()?;
    if !status(stack).running {
        bail!("the appliance isn't running — start it with `atelier app start`");
    }
    Ok(())
}

fn parse_ps(out: &str) -> Vec<PsEntry> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(arr) = serde_json::from_str::<Vec<PsEntry>>(trimmed) {
        return arr;
    }
    trimmed
        .lines()
        .filter_map(|l| serde_json::from_str::<PsEntry>(l.trim()).ok())
        .collect()
}

/// Whether the console answers an HTTP request with a non-server-error status.
///
/// We send a real request and treat any response below 500 — a 200, a redirect
/// to the console, even a 404/403 — as "Drupal is serving". Only a connection
/// failure or a 5xx (the half-booted state that lands users on a broken page)
/// counts as not-ready. HTTP/1.0 + `Connection: close` keeps the response simple
/// (no keep-alive, no chunking) so reading the status line is enough.
fn http_ready(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let req = "GET / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }

    // The status line is the first thing on the wire; 128 bytes is plenty.
    let mut buf = [0u8; 128];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    parse_http_status(&buf[..n]).is_some_and(|code| code < 500)
}

/// Pull the 3-digit status code out of an HTTP status line (`HTTP/1.1 200 OK`).
fn parse_http_status(bytes: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(bytes).ok()?.lines().next()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn local_digest(image: &str) -> Option<String> {
    let mut c = docker::docker();
    c.args(["image", "inspect", image, "--format", "{{index .RepoDigests 0}}"]);
    try_capture(c).and_then(|s| s.split('@').nth(1).map(str::to_string))
}

fn remote_digest(image: &str) -> Option<String> {
    let mut c = docker::docker();
    c.args(["buildx", "imagetools", "inspect", image, "--format", "{{.Manifest.Digest}}"]);
    try_capture(c)
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        backup_script, is_backup_file, is_snapshot_bundle, list_backups, parse_http_status,
        restore_bundle_script,
    };
    use crate::stack::Stack;

    /// Syntax-check a shell snippet with `sh -n` (parse only; nothing executes).
    fn assert_valid_sh(script: &str) {
        let out = std::process::Command::new("sh")
            .args(["-n", "-c", script])
            .output()
            .expect("run sh -n");
        assert!(
            out.status.success(),
            "generated shell failed to parse:\n{script}\n---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn backup_script_is_valid_shell_and_packs_db_and_files() {
        let drush = "/opt/drupal/vendor/bin/drush --root=/opt/drupal/web";
        let manifest = r#"{"format":"aincient-snapshot","version":1,"created":"x","image":"y"}"#;
        let script = backup_script(drush, manifest);
        assert_valid_sh(&script);
        assert!(script.contains("sql:dump --gzip"), "dumps the database");
        // Both members packed: the staged DB dump and the live files tree.
        assert!(script.contains("database.sql.gz"), "packs the db dump");
        assert!(
            script.contains("-C /opt/drupal/web/sites/default files"),
            "packs the files tree"
        );
    }

    #[test]
    fn restore_bundle_script_is_valid_shell_and_reasserts_ownership() {
        let drush = "/opt/drupal/vendor/bin/drush --root=/opt/drupal/web";
        let script = restore_bundle_script(drush);
        assert_valid_sh(&script);
        assert!(script.contains("sql:drop -y"), "drops before load");
        assert!(script.contains("| /opt/drupal/vendor/bin/drush"), "loads the db");
        // The scotty gotcha: files written as root must be chowned back to www-data.
        assert!(
            script.contains("chown -R www-data:www-data"),
            "re-asserts files ownership"
        );
    }

    #[test]
    fn recognises_snapshot_bundles_vs_legacy_dumps() {
        assert!(is_snapshot_bundle("aincient-20260707.tar.gz"));
        assert!(is_snapshot_bundle("SNAP.TGZ")); // case-insensitive
        assert!(!is_snapshot_bundle("aincient-20260707.sql.gz"));
        assert!(!is_snapshot_bundle("aincient-20260707.sql"));
    }

    #[test]
    fn accepts_both_bundles_and_legacy_dumps_as_backups() {
        for good in ["a.tar.gz", "a.tgz", "a.sql.gz", "a.sql"] {
            assert!(is_backup_file(good), "{good} should be a backup file");
        }
        for bad in ["notes.txt", "a.zip", "a.tar", "archive.gz"] {
            assert!(!is_backup_file(bad), "{bad} should not be a backup file");
        }
    }

    #[test]
    fn list_backups_includes_bundles_and_dumps_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("aincient-backups-{}", std::process::id()));
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        // Two restorable backups + one unrelated file that must be ignored.
        std::fs::write(backups.join("aincient-old.sql.gz"), b"legacy").unwrap();
        std::fs::write(backups.join("aincient-new.tar.gz"), b"bundle").unwrap();
        std::fs::write(backups.join("README.txt"), b"nope").unwrap();

        let listed = list_backups(&Stack { home: dir.clone() });
        let mut names: Vec<_> = listed.iter().map(|b| b.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["aincient-new.tar.gz", "aincient-old.sql.gz"]);
        // Results are sorted newest-first by mtime (no strict assertion here —
        // the two files may share a timestamp; ordering is covered by mtime desc).

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_status_codes_from_the_status_line() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_http_status(b"HTTP/1.0 302 Found\r\nLocation: /x"), Some(302));
        assert_eq!(parse_http_status(b"HTTP/1.1 503 Service Unavailable\r\n"), Some(503));
    }

    #[test]
    fn rejects_garbage_and_empty_input() {
        assert_eq!(parse_http_status(b""), None);
        assert_eq!(parse_http_status(b"not http at all"), None);
        assert_eq!(parse_http_status(b"HTTP/1.1\r\n"), None);
    }

    #[test]
    fn readiness_is_below_500() {
        // Mirrors the threshold http_ready applies to the parsed code.
        for (raw, ready) in [
            (b"HTTP/1.1 200 OK\r\n".as_slice(), true),
            (b"HTTP/1.1 302 Found\r\n".as_slice(), true),
            (b"HTTP/1.1 404 Not Found\r\n".as_slice(), true),
            (b"HTTP/1.1 500 Internal Server Error\r\n".as_slice(), false),
            (b"HTTP/1.1 503 Service Unavailable\r\n".as_slice(), false),
        ] {
            assert_eq!(parse_http_status(raw).is_some_and(|c| c < 500), ready);
        }
    }
}
