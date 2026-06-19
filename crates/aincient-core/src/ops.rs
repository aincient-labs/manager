//! The lifecycle operations: status, install, update, backup, restore, …
//!
//! Each maps to a `docker compose` / `drush` primitive that the appliance already
//! uses, so the manager never invents behaviour the bootstrapper and `converge.sh`
//! don't already guarantee (idempotent install/upgrade, snapshot-format backups).

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::docker::{self, compose, preflight, run_capture, run_inherited, try_capture};
use crate::stack::{InstallOptions, Stack};

/// drush, as invoked inside the `app` container.
const DRUSH: &[&str] = &["/opt/drupal/vendor/bin/drush", "--root=/opt/drupal/web"];

/// A snapshot of the appliance's current state.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// Has a stack been laid down (`compose.yaml` present)?
    pub installed: bool,
    /// Is the `app` container running?
    pub running: bool,
    /// Is the console accepting TCP connections on its port?
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

    let reachable = running && port_reachable(stack.http_port());

    Status {
        installed,
        running,
        reachable,
        console_url: stack.console_url(),
        image: stack.image(),
        image_digest: local_digest(&stack.image()),
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
/// and auto-rolls-back on failure.
pub fn install(stack: &Stack, opts: &InstallOptions) -> Result<()> {
    preflight().require()?;
    stack.ensure_scaffold(opts)?;
    pull(stack)?;
    up(stack)
}

/// Pull a newer image and recreate the stack — the upgrade path.
pub fn update(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    preflight().require()?;
    pull(stack)?;
    up(stack)
}

/// `docker compose pull`, with a registry-login hint for the private image.
pub fn pull(stack: &Stack) -> Result<()> {
    let mut c = compose(stack);
    c.arg("pull");
    run_inherited(c, "pull the image").map_err(|e| {
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
pub fn up(stack: &Stack) -> Result<()> {
    let mut c = compose(stack);
    c.args(["up", "-d"]);
    run_inherited(c, "start the appliance")
}

/// `docker compose stop` — stop containers, keep data.
pub fn stop(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    let mut c = compose(stack);
    c.arg("stop");
    run_inherited(c, "stop the appliance")
}

/// `docker compose start` — start previously-stopped containers.
pub fn start(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    let mut c = compose(stack);
    c.arg("start");
    run_inherited(c, "start the appliance")
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
pub fn reinstall(stack: &Stack, opts: &InstallOptions) -> Result<()> {
    preflight().require()?;
    if stack.exists() {
        down(stack, true)?;
    }
    install(stack, opts)
}

/// Back up the database to the host, reusing converge's snapshot format
/// (`drush sql:dump --gzip`). Returns the path to the created archive.
pub fn backup(stack: &Stack, label: Option<&str>) -> Result<PathBuf> {
    ensure_running(stack)?;
    std::fs::create_dir_all(stack.backups_dir())?;

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = match label {
        Some(l) if !l.is_empty() => format!("{}-{ts}", sanitize(l)),
        _ => ts.clone(),
    };
    let host_path = stack.backups_dir().join(format!("aincient-{stem}.sql.gz"));

    // drush appends `.gz` to --result-file when --gzip is set (see converge.sh).
    let container_base = format!("/opt/drupal/private/snapshots/manual-{ts}.sql");
    let container_gz = format!("{container_base}.gz");

    let mut dump = compose(stack);
    dump.args(["exec", "-T", "app"]).args(DRUSH).args([
        "sql:dump",
        "--gzip",
        &format!("--result-file={container_base}"),
    ]);
    run_capture(dump, "dump the database")?;

    let mut cp = compose(stack);
    cp.args(["cp", &format!("app:{container_gz}"), &host_path.to_string_lossy()]);
    run_capture(cp, "copy the backup out of the container")?;

    // Best-effort cleanup of the in-container temp file.
    let mut rm = compose(stack);
    rm.args(["exec", "-T", "app", "rm", "-f", &container_gz]);
    let _ = rm.output();

    Ok(host_path)
}

/// Restore the database from a host backup file. Mirrors converge's
/// `restore_snapshot`: drop, load, rebuild caches. Destructive — confirm first.
pub fn restore(stack: &Stack, file: &Path) -> Result<()> {
    ensure_running(stack)?;
    if !file.is_file() {
        bail!("backup file not found: {}", file.display());
    }
    let gzipped = file
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gz"))
        .unwrap_or(false);
    let container_tmp = if gzipped {
        "/tmp/aincient-restore.sql.gz"
    } else {
        "/tmp/aincient-restore.sql"
    };

    let mut cp = compose(stack);
    cp.args(["cp", &file.to_string_lossy(), &format!("app:{container_tmp}")]);
    run_capture(cp, "copy the backup into the container")?;

    let mut drop = compose(stack);
    drop.args(["exec", "-T", "app"]).args(DRUSH).args(["sql:drop", "-y"]);
    run_capture(drop, "drop the existing database")?;

    let drush = DRUSH.join(" ");
    let load_cmd = if gzipped {
        format!("zcat {container_tmp} | {drush} sql:cli")
    } else {
        format!("cat {container_tmp} | {drush} sql:cli")
    };
    let mut load = compose(stack);
    load.args(["exec", "-T", "app", "sh", "-c", &load_cmd]);
    run_capture(load, "load the backup")?;

    let mut cr = compose(stack);
    cr.args(["exec", "-T", "app"]).args(DRUSH).arg("cache:rebuild");
    let _ = cr.output();

    let mut rm = compose(stack);
    rm.args(["exec", "-T", "app", "rm", "-f", container_tmp]);
    let _ = rm.output();

    Ok(())
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
        if !(name.ends_with(".sql.gz") || name.ends_with(".sql")) {
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
pub fn set_admin_password(stack: &Stack, password: &str) -> Result<()> {
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["user:password", "admin", password]);
    run_capture(c, "set the admin password").map(|_| ())
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

/// Open the console in the user's default browser.
pub fn open_console(stack: &Stack) -> Result<()> {
    let url = stack.console_url();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(&url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", &url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(&url);
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
            "no AIncient stack found at {} — run `aincient install` first",
            stack.home.display()
        );
    }
    Ok(())
}

fn ensure_running(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    preflight().require()?;
    if !status(stack).running {
        bail!("the appliance isn't running — start it with `aincient start`");
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

fn port_reachable(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
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
