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

/// Back up the database to the host, reusing converge's snapshot format
/// (`drush sql:dump --gzip`). Returns the path to the created archive.
pub fn backup(stack: &Stack, label: Option<&str>, r: &mut dyn Reporter) -> Result<PathBuf> {
    r.stage(Stage::Working, "Backing up the database…", None);
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

    r.log("Dumping the database…");
    let mut dump = compose(stack);
    dump.args(["exec", "-T", "app"]).args(DRUSH).args([
        "sql:dump",
        "--gzip",
        &format!("--result-file={container_base}"),
    ]);
    run_capture(dump, "dump the database")?;

    r.log("Copying the archive out of the container…");
    let mut cp = compose(stack);
    cp.args(["cp", &format!("app:{container_gz}"), &host_path.to_string_lossy()]);
    run_capture(cp, "copy the backup out of the container")?;

    // Best-effort cleanup of the in-container temp file.
    let mut rm = compose(stack);
    rm.args(["exec", "-T", "app", "rm", "-f", &container_gz]);
    let _ = rm.output();

    r.log(&format!("Backup written to {}", host_path.display()));
    Ok(host_path)
}

/// Restore the database from a host backup file. Mirrors converge's
/// `restore_snapshot`: drop, load, rebuild caches. Destructive — confirm first.
pub fn restore(stack: &Stack, file: &Path, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Working, "Restoring the database…", None);
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

/// Open the console (the chat workspace at `/aincient`) in the default browser.
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
            "no Atelier stack found at {} — run `atelier install` first",
            stack.home.display()
        );
    }
    Ok(())
}

fn ensure_running(stack: &Stack) -> Result<()> {
    ensure_installed(stack)?;
    preflight().require()?;
    if !status(stack).running {
        bail!("the appliance isn't running — start it with `atelier start`");
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
    use super::parse_http_status;

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
