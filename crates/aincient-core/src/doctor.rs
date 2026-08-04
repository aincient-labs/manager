//! `doctor` — diagnose an appliance, and (on request) repair it.
//!
//! Three tiers, checked in dependency order, because a failure in one makes the
//! next meaningless: the **host** must have Docker before a **stack** can run,
//! and the stack must be up before the **site** inside it can be asked anything.
//! A tier whose prerequisite failed is reported as [`Severity::Skipped`], never
//! as a failure — "we couldn't look" and "we looked and it's broken" are
//! different answers, and collapsing them is exactly what made the update-check
//! bug report undiagnosable (DECISIONS 0311).
//!
//! Diagnosis is read-only. [`fix`] applies a **safe ladder** — the repairs whose
//! worst case is lost time, never lost data:
//!
//! 1. re-scaffold a missing/incomplete `compose.yaml` + `.env`
//! 2. `docker compose up -d` — bring the containers back
//! 3. `drush cache:rebuild` — the Drupal reflex, and the floor of any repair
//! 4. `drush updatedb -y` — finish a half-applied upgrade
//! 5. re-assert `www-data` ownership of the files tree
//! 6. re-run `converge.sh` — the appliance's own self-heal, which snapshots
//!    first and rolls back on failure, so even the hammer is reversible
//!
//! Deliberately NOT in the ladder, at any flag: `down -v`, reinstall, restore.
//! Those destroy data, so doctor only ever *names* them as the next step.

use std::net::{Ipv4Addr, TcpListener};
use std::process::Command;

use serde::Serialize;

use crate::docker::{self, compose, preflight, probe};
use crate::ops::{self, Reporter, Stage, DRUSH, FILES_DIR};
use crate::stack::Stack;

/// How a single check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Verified good.
    Ok,
    /// Working, but something is off or unfinished (onboarding not done, a
    /// missing optional plugin). Never fails the exit code.
    Warn,
    /// Genuinely broken — the appliance can't do its job until it's addressed.
    Fail,
    /// Not checked, because a prerequisite failed. Not a verdict.
    Skipped,
}

impl Severity {
    pub fn is_ok(self) -> bool {
        matches!(self, Severity::Ok)
    }
}

/// Which tier a check belongs to — the grouping a report renders under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The machine: Docker, ports, disk.
    Host,
    /// `~/.atelier` and the containers it describes.
    Stack,
    /// Drupal, inside the running `app` container.
    Site,
}

/// One diagnosis. `id` is stable and greppable (`site.pending_updates`) so a
/// pasted `--json` report can be reasoned about without parsing prose.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub tier: Tier,
    pub label: &'static str,
    pub severity: Severity,
    /// What we actually observed. Present whenever it adds anything beyond the
    /// label — the error text, the port number, the pending update count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the user should do. Always present when not [`Severity::Ok`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// The repair `--fix` would attempt, if any. `None` means doctor can't fix
    /// this one — the remedy is the user's to carry out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Repair>,
}

impl Check {
    fn ok(id: &'static str, tier: Tier, label: &'static str) -> Self {
        Check { id, tier, label, severity: Severity::Ok, detail: None, remedy: None, fix: None }
    }

    fn bad(
        id: &'static str,
        tier: Tier,
        label: &'static str,
        severity: Severity,
        remedy: impl Into<String>,
    ) -> Self {
        Check {
            id,
            tier,
            label,
            severity,
            detail: None,
            remedy: Some(remedy.into()),
            fix: None,
        }
    }

    fn skipped(id: &'static str, tier: Tier, label: &'static str, why: impl Into<String>) -> Self {
        Check {
            id,
            tier,
            label,
            severity: Severity::Skipped,
            detail: Some(why.into()),
            remedy: None,
            fix: None,
        }
    }

    fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }

    fn fixable(mut self, repair: Repair) -> Self {
        self.fix = Some(repair);
        self
    }
}

/// A repair doctor knows how to perform. Ordered as the ladder runs: cheapest
/// and most local first, so a site that only needed a cache rebuild never gets
/// a converge run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Repair {
    /// Write back a missing `compose.yaml` / `.env` key.
    Scaffold,
    /// `docker compose up -d`.
    StartContainers,
    /// `drush cache:rebuild` — the Drupal reflex.
    CacheRebuild,
    /// `drush updatedb -y` — finish pending database updates.
    RunUpdates,
    /// `chown -R www-data:www-data` on the files tree.
    FixFilePermissions,
    /// Re-run the appliance's `converge.sh` (snapshot + rollback built in).
    Converge,
}

impl Repair {
    /// What this repair does, in the user's language.
    pub fn describe(self) -> &'static str {
        match self {
            Repair::Scaffold => "Restore the stack files (compose.yaml + .env)",
            Repair::StartContainers => "Start the appliance containers",
            Repair::CacheRebuild => "Rebuild Drupal's caches",
            Repair::RunUpdates => "Run the pending database updates",
            Repair::FixFilePermissions => "Repair ownership of the uploaded-files directory",
            Repair::Converge => "Re-run the appliance's self-heal (snapshots first, rolls back on failure)",
        }
    }
}

/// What happened when a repair ran.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub repair: Repair,
    pub description: &'static str,
    pub succeeded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The full result of a doctor run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    /// Repairs attempted this run. Empty for a plain diagnosis.
    pub actions: Vec<Action>,
}

impl Report {
    /// Nothing is failing. Warnings don't count — they're advisory by definition.
    pub fn healthy(&self) -> bool {
        !self.checks.iter().any(|c| c.severity == Severity::Fail)
    }

    pub fn count(&self, severity: Severity) -> usize {
        self.checks.iter().filter(|c| c.severity == severity).count()
    }

    /// The repairs doctor could attempt, deduplicated and in ladder order — what
    /// `--fix` would run, and what a diagnosis-only report offers.
    pub fn available_repairs(&self) -> Vec<Repair> {
        let mut repairs: Vec<Repair> = self
            .checks
            .iter()
            .filter(|c| c.severity != Severity::Ok && c.severity != Severity::Skipped)
            .filter_map(|c| c.fix)
            .collect();
        repairs.sort();
        repairs.dedup();
        repairs
    }
}

// --- diagnosis ---------------------------------------------------------------

/// Examine the host, the stack and the site. Read-only — never mutates anything.
pub fn diagnose(stack: &Stack) -> Report {
    let mut checks = Vec::new();

    let host_ok = check_host(stack, &mut checks);
    let stack_ok = check_stack(stack, host_ok, &mut checks);
    check_site(stack, stack_ok, &mut checks);

    Report { checks, actions: Vec::new() }
}

/// Diagnose, then climb the repair ladder one rung at a time, re-diagnosing
/// after each — stopping the moment the appliance is healthy.
///
/// Re-diagnosing between rungs is what keeps the ladder honest in both
/// directions. It stops early, so a site that only needed a cache rebuild never
/// gets a `converge` run it didn't need. And it can't report success it didn't
/// achieve: the returned report is a fresh reading of the state the user is
/// actually left in, not an assumption that the repairs worked. `actions`
/// records every attempt either way, including the ones that failed.
pub fn fix(stack: &Stack, r: &mut dyn Reporter) -> Report {
    let mut report = diagnose(stack);
    let mut actions = Vec::new();
    // Each repair is attempted at most once, so the loop is bounded by the
    // number of Repair variants no matter what the re-diagnosis reports.
    let mut attempted: Vec<Repair> = Vec::new();

    while let Some(repair) = report
        .available_repairs()
        .into_iter()
        .find(|candidate| !attempted.contains(candidate))
    {
        attempted.push(repair);
        r.stage(Stage::Working, repair.describe(), None);
        actions.push(match apply(stack, repair, r) {
            Ok(detail) => Action {
                repair,
                description: repair.describe(),
                succeeded: true,
                detail,
            },
            Err(e) => Action {
                repair,
                description: repair.describe(),
                succeeded: false,
                detail: Some(e),
            },
        });

        report = diagnose(stack);
        if report.healthy() {
            break;
        }
    }

    report.actions = actions;
    report
}

/// Perform one repair. `Ok(Some(detail))` carries anything worth showing.
fn apply(stack: &Stack, repair: Repair, r: &mut dyn Reporter) -> Result<Option<String>, String> {
    match repair {
        Repair::Scaffold => stack
            .repair_scaffold()
            .map(|changes| (!changes.is_empty()).then(|| changes.join("; ")))
            .map_err(|e| format!("{e:#}")),
        Repair::StartContainers => {
            let mut c = compose(stack);
            c.args(["up", "-d"]);
            probe(c)?;
            // Containers report "running" the instant they start, but the
            // entrypoint then runs converge (site install or migrations) for up
            // to minutes. Re-diagnosing immediately would ask a half-booted
            // Drupal whether it bootstraps, get "no", and escalate to a repair
            // the site never needed. Wait for it to actually serve first.
            if ops::wait_until_ready(stack, ops::READY_TIMEOUT, r) {
                Ok(None)
            } else {
                Ok(Some(
                    "started, but the console hasn't answered yet — it may still be booting"
                        .to_string(),
                ))
            }
        }
        Repair::CacheRebuild => drush(stack, &["cache:rebuild"]).map(Some),
        Repair::RunUpdates => drush(stack, &["updatedb", "-y"]).map(Some),
        Repair::FixFilePermissions => {
            let mut c = compose(stack);
            c.args(["exec", "-T", "app", "chown", "-R", "www-data:www-data", FILES_DIR]);
            probe(c).map(|_| None)
        }
        Repair::Converge => {
            // converge.sh is chatty and slow (snapshot, updatedb, config import,
            // health gate) — relay it line by line rather than leaving the user
            // staring at nothing.
            let mut c = compose(stack);
            c.args(["exec", "-T", "app", CONVERGE]);
            docker::run_streaming(c, "re-converge the site", |line| r.log(line))
                .map(|_| None)
                .map_err(|e| format!("{e:#}"))
        }
    }
}

// --- tier 1: the host --------------------------------------------------------

/// Returns whether the host can run containers at all.
fn check_host(stack: &Stack, out: &mut Vec<Check>) -> bool {
    let pf = preflight();

    // Report the VERSION of each tool, not just that it's there. "Docker
    // installed ✓" is the same line on every machine and tells a bug report
    // nothing; the version is what distinguishes a host where something works
    // from one where it doesn't, and it costs one extra probe on a command the
    // user ran on purpose.
    out.push(
        with_version(
            flag(
                "docker.installed",
                Tier::Host,
                "Docker installed",
                pf.docker_installed,
                Severity::Fail,
                "Install Docker Desktop (Mac/Windows) or Docker Engine (Linux). Atelier runs as a \
                 container, so Docker is required.",
            ),
            pf.docker_installed.then(docker_cli_version).flatten(),
        ),
    );
    // The engine, not the CLI — they're separate programs and routinely differ
    // (a remote context, OrbStack, colima, or a Desktop mid-upgrade), which is
    // precisely the sort of skew a report needs to name.
    out.push(with_version(
        flag(
            "docker.running",
            Tier::Host,
            "Docker running",
            pf.docker_running,
            Severity::Fail,
            "Start Docker, then run doctor again.",
        ),
        pf.docker_running.then(docker_engine_version).flatten(),
    ));
    out.push(with_version(
        flag(
            "docker.compose",
            Tier::Host,
            "Compose plugin",
            pf.compose_available,
            Severity::Fail,
            "Install the Docker Compose plugin (it ships with Docker Desktop) — check with \
             `docker compose version`.",
        ),
        pf.compose_available.then(compose_version).flatten(),
    ));

    // buildx is the one dependency nothing else in the manager needs, so a host
    // without it works perfectly right up to the update check and then fails
    // there with no obvious link back (DECISIONS 0311). Naming it here is the
    // whole reason it's a check: it's a warning, not a failure.
    if pf.docker_installed {
        let buildx = probe({
            let mut c = docker::docker();
            c.args(["buildx", "version"]);
            c
        })
        .ok();
        out.push(with_version(
            flag(
                "docker.buildx",
                Tier::Host,
                "buildx plugin (update checks)",
                buildx.is_some(),
                Severity::Warn,
                "Install Docker's buildx plugin (`docker-buildx-plugin` on Linux; it ships with \
                 Docker Desktop). Only the update check needs it — everything else works without.",
            ),
            buildx.as_deref().and_then(parse_buildx_version),
        ));
    } else {
        out.push(Check::skipped(
            "docker.buildx",
            Tier::Host,
            "buildx plugin (update checks)",
            "Docker isn't installed.",
        ));
    }

    out.push(check_port(stack, pf.docker_running));
    out.push(check_disk(stack));

    pf.ok()
}

/// Attach a tool's version as the check's detail, when we could read one.
///
/// Only ever decorates — a version we couldn't parse must never change a
/// verdict, since `preflight` already decided whether the tool works.
fn with_version(check: Check, version: Option<String>) -> Check {
    match version {
        Some(v) => check.detail(v),
        None => check,
    }
}

/// `docker --version` → `Docker version 27.3.1, build ce12230` → `27.3.1`.
fn docker_cli_version() -> Option<String> {
    let mut c = docker::docker();
    c.arg("--version");
    parse_docker_cli_version(&probe(c).ok()?)
}

fn parse_docker_cli_version(out: &str) -> Option<String> {
    // Take the token after "version", trimming the comma that precedes ", build".
    let after = out.split_whitespace().skip_while(|w| !w.eq_ignore_ascii_case("version")).nth(1)?;
    non_empty(after.trim_end_matches(','))
}

/// The daemon's own version, via `docker info`. Distinct from the CLI's — see
/// the call site.
fn docker_engine_version() -> Option<String> {
    let mut c = docker::docker();
    c.args(["info", "--format", "{{.ServerVersion}}"]);
    let out = probe(c).ok()?;
    non_empty(out.trim()).map(|v| format!("engine {v}"))
}

/// `docker compose version --short` → `2.29.7`. The `--short` form exists
/// precisely for this, so there's nothing to parse beyond a trim.
fn compose_version() -> Option<String> {
    let mut c = docker::docker();
    c.args(["compose", "version", "--short"]);
    let out = probe(c).ok()?;
    non_empty(out.trim()).map(|v| v.trim_start_matches('v').to_string())
}

/// `docker buildx version` → `github.com/docker/buildx v0.17.1 <sha>` → `0.17.1`.
/// The line leads with a package path, so the version is the first `vN…` token
/// rather than the second word.
fn parse_buildx_version(out: &str) -> Option<String> {
    let token = out.split_whitespace().find(|w| {
        w.strip_prefix('v')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })?;
    non_empty(token.trim_start_matches('v'))
}

/// Is the console's port usable? Occupied is only a problem when it *isn't* our
/// own appliance holding it — a running Atelier is supposed to own that port.
fn check_port(stack: &Stack, docker_running: bool) -> Check {
    const ID: &str = "host.port";
    const LABEL: &str = "Console port available";
    let port = stack.http_port();

    if port_free(port) {
        return Check::ok(ID, Tier::Host, LABEL).detail(format!("port {port}"));
    }
    // Something is listening. If it's our own app container, that's the healthy
    // case, not a conflict.
    if docker_running && stack.exists() && ops::status(stack).running {
        return Check::ok(ID, Tier::Host, LABEL).detail(format!("port {port} — held by Atelier"));
    }
    Check::bad(
        ID,
        Tier::Host,
        LABEL,
        Severity::Fail,
        format!(
            "Another program is already listening on port {port}. Stop it, or move Atelier to a \
             free port with `atelier app install --port <PORT>`."
        ),
    )
    .detail(format!("port {port} is in use by something that isn't Atelier"))
}

fn port_free(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// Free space where the stack (and Docker's volumes, on Linux) lives. The image
/// is ~1 GB and a converge takes a database snapshot, so a nearly-full disk is a
/// real and confusing failure mode — an upgrade dies mid-migration.
fn check_disk(stack: &Stack) -> Check {
    const ID: &str = "host.disk";
    const LABEL: &str = "Free disk space";
    const NEEDED_MB: u64 = 4096;

    let Some(free_mb) = free_space_mb(&stack.home) else {
        return Check::skipped(ID, Tier::Host, LABEL, "Couldn't read free space on this platform.");
    };
    let detail = format!("{:.1} GB free", free_mb as f64 / 1024.0);
    if free_mb >= NEEDED_MB {
        return Check::ok(ID, Tier::Host, LABEL).detail(detail);
    }
    Check::bad(
        ID,
        Tier::Host,
        LABEL,
        Severity::Warn,
        format!(
            "Less than {} GB free. The appliance image plus a pre-upgrade database snapshot need \
             room — free some space before updating.",
            NEEDED_MB / 1024
        ),
    )
    .detail(detail)
}

/// Free megabytes on the filesystem holding `path`, via POSIX `df -Pk`. Walks up
/// to the nearest existing ancestor, since the stack directory may not exist yet.
#[cfg(unix)]
fn free_space_mb(path: &std::path::Path) -> Option<u64> {
    let mut probe_path = path;
    while !probe_path.exists() {
        probe_path = probe_path.parent()?;
    }
    let out = Command::new("df").arg("-Pk").arg(probe_path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df_available_kb(&String::from_utf8_lossy(&out.stdout)).map(|kb| kb / 1024)
}

#[cfg(not(unix))]
fn free_space_mb(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The "Available" column (4th) of `df -Pk`'s data row, in KB. POSIX output mode
/// guarantees one row per filesystem on a single line, so no continuation
/// handling is needed.
fn parse_df_available_kb(out: &str) -> Option<u64> {
    let row = out.lines().nth(1)?;
    row.split_whitespace().nth(3)?.parse().ok()
}

// --- tier 2: the stack -------------------------------------------------------

/// Returns whether the `app` container is running — the gate for the site tier.
fn check_stack(stack: &Stack, host_ok: bool, out: &mut Vec<Check>) -> bool {
    // The stack files are on disk, so this one is answerable even without Docker.
    let scaffolded = stack.exists();
    out.push(if scaffolded {
        Check::ok("stack.present", Tier::Stack, "Stack files present")
            .detail(stack.home.display().to_string())
    } else {
        Check::bad(
            "stack.present",
            Tier::Stack,
            "Stack files present",
            Severity::Fail,
            "No appliance is installed here. Run `atelier app install`.",
        )
        .detail(format!("nothing at {}", stack.home.display()))
    });

    // A stack with no HASH_SALT can't boot Drupal at all (compose refuses to
    // interpolate it), and it's a plain missing line — safe to write back.
    if scaffolded {
        let salt = stack.env_get("HASH_SALT");
        out.push(match salt {
            Some(s) if s.len() >= 32 => Check::ok("stack.env", Tier::Stack, "Stack settings (.env)"),
            _ => Check::bad(
                "stack.env",
                Tier::Stack,
                "Stack settings (.env)",
                Severity::Fail,
                "The stack's .env is missing its HASH_SALT — Drupal can't boot without one.",
            )
            .fixable(Repair::Scaffold),
        });
    } else {
        out.push(Check::skipped(
            "stack.env",
            Tier::Stack,
            "Stack settings (.env)",
            "No stack yet.",
        ));
    }

    if !host_ok || !scaffolded {
        // Say which prerequisite is actually missing. "Docker isn't ready" on a
        // machine with perfectly good Docker sends the user to fix the wrong
        // thing — the same undiagnosability that motivated this command.
        let why = if !host_ok {
            "Docker isn't ready — fix the checks above first."
        } else {
            "No appliance is installed here yet."
        };
        for (id, label) in [
            ("stack.compose_valid", "compose.yaml is valid"),
            ("stack.db", "Database container healthy"),
            ("stack.app", "Appliance container running"),
        ] {
            out.push(Check::skipped(id, Tier::Stack, label, why));
        }
        return false;
    }

    // `compose config -q` parses and interpolates the file without running
    // anything — it catches a truncated compose.yaml or an unset variable.
    let mut cfg = compose(stack);
    cfg.args(["config", "-q"]);
    out.push(match probe(cfg) {
        Ok(_) => Check::ok("stack.compose_valid", Tier::Stack, "compose.yaml is valid"),
        Err(e) => Check::bad(
            "stack.compose_valid",
            Tier::Stack,
            "compose.yaml is valid",
            Severity::Fail,
            "The stack's compose.yaml doesn't parse. Doctor can rewrite it from the built-in \
             template (your data volumes are untouched).",
        )
        .detail(first_line(&e))
        .fixable(Repair::Scaffold),
    });

    out.push(check_container(stack, "db", "stack.db", "Database container healthy"));

    let app = check_container(stack, "app", "stack.app", "Appliance container running");
    let app_up = app.severity.is_ok();
    out.push(app);
    app_up
}

/// Inspect one compose service: is its container up, and (if it declares a
/// healthcheck) healthy? A container stuck in a restart loop reports "running"
/// on any single glance, so the restart count is read too — that's the state
/// that otherwise looks fine and never serves.
fn check_container(stack: &Stack, service: &str, id: &'static str, label: &'static str) -> Check {
    let mut ps = compose(stack);
    ps.args(["ps", "-q", service]);
    let container = match probe(ps) {
        Ok(out) if !out.trim().is_empty() => out.trim().lines().next().unwrap_or("").to_string(),
        Ok(_) => {
            return Check::bad(
                id,
                Tier::Stack,
                label,
                Severity::Fail,
                "The container isn't there. Start the appliance with `atelier app start`.",
            )
            .detail(format!("no `{service}` container"))
            .fixable(Repair::StartContainers)
        }
        Err(e) => {
            return Check::bad(
                id,
                Tier::Stack,
                label,
                Severity::Fail,
                "Couldn't ask Docker about the appliance's containers.",
            )
            .detail(first_line(&e))
        }
    };

    let mut inspect = docker::docker();
    inspect.args([
        "inspect",
        &container,
        "--format",
        "{{.State.Status}}|{{.RestartCount}}|{{if .State.Health}}{{.State.Health.Status}}{{end}}",
    ]);
    let raw = match probe(inspect) {
        Ok(out) => out,
        Err(e) => {
            return Check::bad(id, Tier::Stack, label, Severity::Fail, "Couldn't inspect the container.")
                .detail(first_line(&e))
        }
    };
    let state = ContainerState::parse(&raw);

    if !state.running {
        return Check::bad(
            id,
            Tier::Stack,
            label,
            Severity::Fail,
            "The container isn't running. Start it with `atelier app start`.",
        )
        .detail(format!("state: {}", state.status))
        .fixable(Repair::StartContainers);
    }
    // A handful of restarts is a crash loop, not a hiccup: compose's
    // `restart: unless-stopped` means it will keep going forever.
    if state.restarts >= RESTART_LOOP_THRESHOLD {
        return Check::bad(
            id,
            Tier::Stack,
            label,
            Severity::Fail,
            "The container keeps restarting — it's crashing on boot. Read the reason with \
             `atelier app logs app`, then restore a backup if the site itself is damaged.",
        )
        .detail(format!("{} restarts", state.restarts));
    }
    if state.health.as_deref() == Some("unhealthy") {
        return Check::bad(
            id,
            Tier::Stack,
            label,
            Severity::Fail,
            "Docker reports the container as unhealthy — the site inside it failed its health \
             check. The site checks below say why.",
        )
        .detail("health: unhealthy");
    }
    let mut check = Check::ok(id, Tier::Stack, label);
    if let Some(h) = state.health {
        check = check.detail(format!("health: {h}"));
    }
    check
}

/// Enough consecutive restarts to call it a loop rather than one bad boot.
const RESTART_LOOP_THRESHOLD: u32 = 3;

#[derive(Debug, PartialEq)]
struct ContainerState {
    status: String,
    running: bool,
    restarts: u32,
    /// `None` when the image declares no healthcheck (the `db` image doesn't
    /// always), which is not the same as unhealthy.
    health: Option<String>,
}

impl ContainerState {
    fn parse(raw: &str) -> Self {
        let mut parts = raw.trim().split('|');
        let status = parts.next().unwrap_or_default().trim().to_string();
        let restarts = parts.next().unwrap_or_default().trim().parse().unwrap_or(0);
        let health = parts.next().map(str::trim).filter(|h| !h.is_empty()).map(str::to_string);
        ContainerState {
            running: status == "running",
            status,
            restarts,
            health,
        }
    }
}

// --- tier 3: the site --------------------------------------------------------

/// Where the appliance image puts its self-heal script (`Dockerfile` COPYs both
/// into `/usr/local/bin`).
const CONVERGE: &str = "/usr/local/bin/converge.sh";
const HEALTHCHECK: &str = "/usr/local/bin/healthcheck.sh";

fn check_site(stack: &Stack, app_up: bool, out: &mut Vec<Check>) {
    const SKIP: &str = "The appliance isn't running, so Drupal can't be asked.";

    if !app_up {
        for (id, label) in [
            ("site.bootstrap", "Drupal boots"),
            ("site.updates", "Database updates applied"),
            ("site.health", "Site health check"),
            ("site.files", "Uploaded files writable"),
            ("site.ai", "AI model connected"),
        ] {
            out.push(Check::skipped(id, Tier::Site, label, SKIP));
        }
        return;
    }

    // Bootstrap is the floor: everything below it talks to Drupal.
    let boots = drush(stack, &["status", "--field=bootstrap"])
        .map(|s| s.to_lowercase().contains("successful"))
        .unwrap_or(false);
    out.push(if boots {
        Check::ok("site.bootstrap", Tier::Site, "Drupal boots")
    } else {
        // A stale compiled container (a module that moved on disk between
        // releases) is the classic cause, and cache:rebuild is exactly its cure —
        // which is why the reflex is worth having as a real repair.
        Check::bad(
            "site.bootstrap",
            Tier::Site,
            "Drupal boots",
            Severity::Fail,
            "Drupal won't bootstrap. Doctor will rebuild its caches and, if that isn't enough, \
             re-run the appliance's self-heal.",
        )
        .fixable(Repair::CacheRebuild)
    });

    if !boots {
        // Everything below needs a bootstrapped Drupal — except the ownership
        // check, which is a plain `stat` and is often the actual cause.
        out.push(check_files(stack));
        for (id, label) in [
            ("site.updates", "Database updates applied"),
            ("site.health", "Site health check"),
            ("site.ai", "AI model connected"),
        ] {
            out.push(Check::skipped(id, Tier::Site, label, "Drupal doesn't boot."));
        }
        // Bootstrap failure that a cache rebuild may not cure — offer the hammer.
        out.push(
            Check::bad(
                "site.converge",
                Tier::Site,
                "Site converged cleanly",
                Severity::Fail,
                "The site is in a state a cache rebuild may not fix. Doctor can re-run the \
                 appliance's self-heal, which snapshots the database first and rolls back if the \
                 repair makes things worse.",
            )
            .fixable(Repair::Converge),
        );
        return;
    }

    // Pending updates mean an upgrade stopped halfway — the site runs on new
    // code against an old schema, which fails in ways that look unrelated.
    out.push(match drush(stack, &["updatedb:status", "--format=string"]) {
        Ok(s) if s.trim().is_empty() => {
            Check::ok("site.updates", Tier::Site, "Database updates applied")
        }
        Ok(s) => Check::bad(
            "site.updates",
            Tier::Site,
            "Database updates applied",
            Severity::Fail,
            "Database updates are pending — an upgrade didn't finish. Doctor will run them.",
        )
        .detail(first_line(&s))
        .fixable(Repair::RunUpdates),
        Err(e) => Check::bad(
            "site.updates",
            Tier::Site,
            "Database updates applied",
            Severity::Warn,
            "Couldn't read the update status.",
        )
        .detail(first_line(&e)),
    });

    out.push(check_files(stack));

    // The image's own health gate — the same one converge runs and compose polls.
    // Reusing it means doctor and the container can never disagree about whether
    // the site is well.
    let mut hc = compose(stack);
    hc.args(["exec", "-T", "app", HEALTHCHECK]);
    out.push(match probe(hc) {
        Ok(_) => Check::ok("site.health", Tier::Site, "Site health check"),
        Err(e) => Check::bad(
            "site.health",
            Tier::Site,
            "Site health check",
            Severity::Fail,
            "The site's own health check failed. Doctor will rebuild caches and re-run the \
             appliance's self-heal (which snapshots first and rolls back on failure).",
        )
        .detail(health_detail(&e))
        .fixable(Repair::Converge),
    });

    out.push(check_ai(stack));
}

/// Ownership of the uploaded-files tree. converge and restore both write it as
/// root; if the `www-data` re-assert is ever missed, uploads and image
/// derivatives silently stop working while everything else looks fine.
fn check_files(stack: &Stack) -> Check {
    const ID: &str = "site.files";
    const LABEL: &str = "Uploaded files writable";

    let mut c = compose(stack);
    c.args(["exec", "-T", "app", "stat", "-c", "%U", FILES_DIR]);
    match probe(c) {
        Ok(owner) if owner.trim() == "www-data" => Check::ok(ID, Tier::Site, LABEL),
        Ok(owner) => Check::bad(
            ID,
            Tier::Site,
            LABEL,
            Severity::Fail,
            "The uploaded-files directory isn't owned by the web server, so uploads and image \
             derivatives will fail. Doctor will repair the ownership.",
        )
        .detail(format!("owned by {}", owner.trim()))
        .fixable(Repair::FixFilePermissions),
        Err(e) => Check::bad(ID, Tier::Site, LABEL, Severity::Warn, "Couldn't read the files directory.")
            .detail(first_line(&e)),
    }
}

/// Is a model actually bound? This is a warning, never a failure: a freshly
/// installed appliance is *supposed* to boot with no provider connected, and the
/// remedy is onboarding, not a repair.
///
/// Asks the role layer (`aincient:model-list`), which is the only authority on
/// this since `drupal/ai` was uninstalled.
fn check_ai(stack: &Stack) -> Check {
    const ID: &str = "site.ai";
    const LABEL: &str = "AI model connected";

    match ops::model_list(stack) {
        Ok(roles) => {
            let bound = roles
                .iter()
                .any(|r| r.is_default() && !r.provider.is_empty() && !r.model.is_empty());
            if bound {
                let d = roles
                    .iter()
                    .find(|r| r.is_default())
                    .map(|r| format!("{}:{}", r.provider, r.model))
                    .unwrap_or_default();
                Check::ok(ID, Tier::Site, LABEL).detail(d)
            } else {
                Check::bad(
                    ID,
                    Tier::Site,
                    LABEL,
                    Severity::Warn,
                    "No AI model is bound yet — chat won't work until you connect a provider. \
                     Open the console and finish the first-run wizard, or use `atelier ai model set`.",
                )
            }
        }
        Err(e) => Check::bad(ID, Tier::Site, LABEL, Severity::Warn, "Couldn't read the model roles.")
            .detail(first_line(&format!("{e:#}"))),
    }
}

// --- helpers -----------------------------------------------------------------

/// Run drush inside the `app` container, returning stdout or the failure reason.
fn drush(stack: &Stack, args: &[&str]) -> Result<String, String> {
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"]).args(DRUSH).args(args);
    probe(c)
}

fn flag(
    id: &'static str,
    tier: Tier,
    label: &'static str,
    ok: bool,
    severity: Severity,
    remedy: &'static str,
) -> Check {
    if ok {
        Check::ok(id, tier, label)
    } else {
        Check::bad(id, tier, label, severity, remedy)
    }
}

/// `Some(s)` unless it's blank — so a tool that answered with nothing reads as
/// "no version", not as an empty label.
fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// healthcheck.sh prefixes its verdict (`[health] FAIL: …`); surface that line
/// rather than the first line of whatever noise preceded it.
fn health_detail(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| l.contains("FAIL:"))
        .map(|l| l.replace("[health] FAIL: ", ""))
        .unwrap_or_else(|| first_line(stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_state_reads_status_restarts_and_health() {
        let s = ContainerState::parse("running|0|healthy");
        assert!(s.running);
        assert_eq!(s.restarts, 0);
        assert_eq!(s.health.as_deref(), Some("healthy"));
    }

    #[test]
    fn a_container_without_a_healthcheck_is_not_unhealthy() {
        // The trailing field is empty when the image declares no healthcheck —
        // that must read as "no opinion", not as a failure.
        let s = ContainerState::parse("running|0|");
        assert!(s.running);
        assert_eq!(s.health, None);
    }

    #[test]
    fn a_restart_loop_is_visible_even_though_the_status_says_running() {
        let s = ContainerState::parse("running|17|");
        assert!(s.running, "a crash loop still reports running at any single glance");
        assert!(s.restarts >= RESTART_LOOP_THRESHOLD);
    }

    #[test]
    fn exited_containers_are_not_running() {
        let s = ContainerState::parse("exited|1|");
        assert!(!s.running);
        assert_eq!(s.status, "exited");
    }

    // Version fixtures below are verbatim output of the real commands.

    #[test]
    fn docker_cli_version_drops_the_build_suffix() {
        assert_eq!(
            parse_docker_cli_version("Docker version 27.3.1, build ce12230").as_deref(),
            Some("27.3.1")
        );
        // OrbStack and Desktop both add their own build metadata; same shape.
        assert_eq!(
            parse_docker_cli_version("Docker version 28.0.4, build b8034c0").as_deref(),
            Some("28.0.4")
        );
    }

    #[test]
    fn docker_cli_version_tolerates_unexpected_output() {
        assert_eq!(parse_docker_cli_version(""), None);
        assert_eq!(parse_docker_cli_version("command not found"), None);
        // "version" present but nothing after it.
        assert_eq!(parse_docker_cli_version("Docker version"), None);
    }

    #[test]
    fn buildx_version_is_the_v_token_not_the_second_word() {
        // The line leads with a package path, so a naive "second word" read
        // would return the path.
        assert_eq!(
            parse_buildx_version("github.com/docker/buildx v0.17.1 88d9f2a").as_deref(),
            Some("0.17.1")
        );
    }

    #[test]
    fn buildx_version_tolerates_unexpected_output() {
        assert_eq!(parse_buildx_version(""), None);
        assert_eq!(
            parse_buildx_version("docker: unknown command: docker buildx"),
            None
        );
    }

    #[test]
    fn a_version_only_ever_decorates_a_check() {
        // It must never turn a pass into a failure or vice versa.
        let passing = Check::ok("docker.installed", Tier::Host, "Docker installed");
        let decorated = with_version(passing, Some("27.3.1".into()));
        assert_eq!(decorated.severity, Severity::Ok);
        assert_eq!(decorated.detail.as_deref(), Some("27.3.1"));

        let failing = Check::bad("x", Tier::Host, "x", Severity::Fail, "install it");
        let undecorated = with_version(failing, None);
        assert_eq!(undecorated.severity, Severity::Fail);
        assert!(undecorated.detail.is_none(), "no version, no detail");
    }

    #[test]
    fn df_available_column_is_the_fourth() {
        // Verbatim `df -Pk /` on macOS 15.
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk3s1s1 971350180 10485760 123456789    8%    /\n";
        assert_eq!(parse_df_available_kb(out), Some(123_456_789));
    }

    #[test]
    fn df_garbage_is_not_a_number() {
        assert_eq!(parse_df_available_kb(""), None);
        assert_eq!(parse_df_available_kb("Filesystem 1024-blocks\n"), None);
    }

    #[test]
    fn health_detail_prefers_the_scripts_own_verdict() {
        let stderr = "[converge] something\n[health] FAIL: module not enabled: aincient_core\n";
        assert_eq!(health_detail(stderr), "module not enabled: aincient_core");
    }

    #[test]
    fn health_detail_falls_back_to_the_first_real_line() {
        assert_eq!(health_detail("\n\nsomething broke\n"), "something broke");
    }

    #[test]
    fn repairs_are_offered_in_ladder_order_and_deduplicated() {
        // Two different checks asking for a cache rebuild must not run it twice,
        // and the cheap repair must come before the hammer.
        let report = Report {
            checks: vec![
                Check::bad("a", Tier::Site, "a", Severity::Fail, "x").fixable(Repair::Converge),
                Check::bad("b", Tier::Site, "b", Severity::Fail, "x").fixable(Repair::CacheRebuild),
                Check::bad("c", Tier::Site, "c", Severity::Fail, "x").fixable(Repair::CacheRebuild),
                Check::bad("d", Tier::Stack, "d", Severity::Fail, "x")
                    .fixable(Repair::StartContainers),
            ],
            actions: Vec::new(),
        };
        assert_eq!(
            report.available_repairs(),
            vec![Repair::StartContainers, Repair::CacheRebuild, Repair::Converge]
        );
    }

    #[test]
    fn passing_and_skipped_checks_never_trigger_a_repair() {
        // A skipped check carries a fix hint (so the UI can show it), but doctor
        // must not act on a verdict it never actually reached.
        let report = Report {
            checks: vec![
                Check::ok("a", Tier::Site, "a").fixable(Repair::CacheRebuild),
                Check::skipped("b", Tier::Site, "b", "not run").fixable(Repair::Scaffold),
            ],
            actions: Vec::new(),
        };
        assert!(report.available_repairs().is_empty());
    }

    #[test]
    fn warnings_do_not_make_a_report_unhealthy() {
        let report = Report {
            checks: vec![
                Check::ok("a", Tier::Host, "a"),
                Check::bad("b", Tier::Site, "b", Severity::Warn, "advice"),
                Check::skipped("c", Tier::Site, "c", "not run"),
            ],
            actions: Vec::new(),
        };
        assert!(report.healthy(), "an advisory is not a failure");
        assert_eq!(report.count(Severity::Warn), 1);
        assert_eq!(report.count(Severity::Skipped), 1);
    }

    #[test]
    fn a_single_failure_makes_the_report_unhealthy() {
        let report = Report {
            checks: vec![
                Check::ok("a", Tier::Host, "a"),
                Check::bad("b", Tier::Site, "b", Severity::Fail, "fix it"),
            ],
            actions: Vec::new(),
        };
        assert!(!report.healthy());
    }

    #[test]
    fn every_non_ok_check_carries_a_remedy() {
        // The contract that makes a pasted report actionable: no verdict is ever
        // "it's broken" with nothing to do about it.
        let bad = Check::bad("x", Tier::Site, "x", Severity::Fail, "do the thing");
        assert!(bad.remedy.is_some());
    }
}
