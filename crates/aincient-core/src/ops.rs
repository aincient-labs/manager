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

use crate::boot::{self, Boot};
use crate::docker::{
    self, compose, preflight, run_capture, run_inherited, run_streaming, try_capture,
};
use crate::engine_admin::AdminVerb;
use crate::stack::{Channel, InstallOptions, Stack, LEGACY_DEFAULT_IMAGE};

/// drush, as invoked inside the `app` container.
pub(crate) const DRUSH: &[&str] = &["/opt/drupal/vendor/bin/drush", "--root=/opt/drupal/web"];

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
    /// The steps this op will walk, announced once before the first of them
    /// (DECISIONS 0515). Progress still arrives through [`stage`](Self::stage);
    /// the plan only tells a front end how to group it — the GUI draws one block
    /// per step, the CLI numbers its headings. Default: ignore it, so reporters
    /// that only know `stage`/`log` keep working unchanged.
    fn plan(&mut self, _steps: &[PlanStep]) {}
    /// A pass-through log line from the underlying tool (docker), no bar change.
    fn log(&mut self, _line: &str) {}
    /// A structured pull-progress event (per-layer phase + bytes), emitted in
    /// place of raw `log` lines when docker can produce them and the reporter
    /// captures output. Default: degrade to a plain log line, so reporters that
    /// only know `log` keep working unchanged.
    fn pull_progress(&mut self, ev: &docker::PullEvent) {
        self.log(&ev.to_line());
    }
    /// Whether docker's output should be captured and relayed via [`log`](Self::log)
    /// (a GUI feed), or left to inherit the terminal (the CLI). Default: inherit.
    fn captures_output(&self) -> bool {
        false
    }

    /// Ask the operator to approve a step that changes the image their site runs
    /// on. `true` means go ahead.
    ///
    /// **The default is NO, and that is the point.** A reporter that hasn't been
    /// taught to ask cannot answer on the operator's behalf — the worst outcome of
    /// the default is an install left exactly as it was, which is always a state
    /// the site already survives. The alternative default silently moves someone's
    /// running site to a different build, which is how the 2026-08-06 incident
    /// started (`plans/converge-branch-guard.md`).
    fn confirm(&mut self, _question: &str) -> bool {
        false
    }

    /// Ask the owner whether to restart Docker (`runtime` names it, "OrbStack")
    /// — `detail` says what else that stops. `true` means go ahead. The default
    /// is NO for the same reason as [`confirm`](Self::confirm): a restart stops
    /// every container on the computer, and a reporter that can't ask (a script,
    /// a remote doctor) must not answer for the owner.
    fn confirm_docker_restart(&mut self, _runtime: &str, _detail: &str) -> bool {
        false
    }
}

/// A [`Reporter`] that drops everything — for callers that don't show progress.
pub struct Silent;
impl Reporter for Silent {}

/// One step of an op's [`Plan`]: what to call it and the slice of the overall
/// 0.0–1.0 progress it covers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanStep {
    /// Stable machine id (`"dump"`, `"pull"`, …) — for reports and tests.
    pub id: String,
    /// Short label the GUI prints under the step's blocks ("Database").
    pub label: String,
    pub from: f32,
    pub to: f32,
}

/// The steps an op walks, built from relative weights (DECISIONS 0515).
///
/// It doesn't hold the reporter, so an op keeps using `r` freely between
/// steps: announce the plan, then wrap each step in [`run`](Plan::run).
pub struct Plan {
    steps: Vec<PlanStep>,
}

impl Plan {
    /// `(id, label, weight)` triples, in order. Weights are relative.
    pub fn weighted(steps: &[(&str, &str, f32)]) -> Self {
        let total: f32 = steps.iter().map(|s| s.2).sum::<f32>().max(f32::EPSILON);
        let mut at = 0.0;
        let steps = steps
            .iter()
            .map(|(id, label, w)| {
                let from = at;
                at += w / total;
                PlanStep {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                    from,
                    to: at.min(1.0),
                }
            })
            .collect();
        Plan { steps }
    }

    /// Steps with explicit fraction ranges — for the lifecycle ops, whose shared
    /// helpers (`pull`, `up`, `wait_until_ready`) already report fixed fractions.
    fn ranged(steps: &[(&str, &str, f32, f32)]) -> Self {
        Plan {
            steps: steps
                .iter()
                .map(|(id, label, from, to)| PlanStep {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                    from: *from,
                    to: *to,
                })
                .collect(),
        }
    }

    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    /// Tell the reporter which steps are coming.
    pub fn announce(&self, r: &mut dyn Reporter) {
        r.plan(&self.steps);
    }

    /// Run step `id`: report its headline at the step's start, then `f`. The
    /// error, if any, propagates unchanged — the front end marks the step that
    /// was running when the op failed.
    pub fn run<T>(
        &self,
        r: &mut dyn Reporter,
        id: &str,
        message: &str,
        f: impl FnOnce(&mut dyn Reporter) -> Result<T>,
    ) -> Result<T> {
        let from = self.steps.iter().find(|s| s.id == id).map(|s| s.from);
        debug_assert!(from.is_some(), "step {id} is not in the plan");
        r.stage(Stage::Working, message, from);
        f(r)
    }

    /// The op finished: fill the bar.
    pub fn finish(&self, r: &mut dyn Reporter, message: &str) {
        r.stage(Stage::Working, message, Some(1.0));
    }
}

/// Install, reinstall and a single-hop update: the fractions `pull`, `up` and
/// `wait_until_ready` already report, named.
fn lifecycle_plan() -> Plan {
    Plan::ranged(&[
        ("prepare", "Prep", 0.0, PULL_FROM),
        ("pull", "Download", PULL_FROM, UP_FROM),
        ("start", "Start", UP_FROM, BOOT_FROM),
        ("boot", "Boot", BOOT_FROM, 1.0),
    ])
}

/// Where `pull` starts the bar.
const PULL_FROM: f32 = 0.12;
/// Where `up` starts the bar.
const UP_FROM: f32 = 0.6;
/// Where `wait_until_ready` starts the bar.
const BOOT_FROM: f32 = 0.72;
/// Where `start` starts the bar (no download).
const START_FROM: f32 = 0.4;

/// A snapshot of the appliance's current state.
///
/// `Deserialize` because a remote site's status is the server's own
/// `atelier app status --json`, read back over SSH (M2, D-01).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Public site root (the front page visitors see) — the "View my site"
    /// target, distinct from the `/atelier` console.
    pub site_url: String,
    pub image: String,
    /// Which stream of images this install follows — stable, edge, or pinned.
    pub channel: Channel,
    /// Registry digest of the build the site's `app` container runs, or of the
    /// configured tag when there's no container (best effort).
    pub image_digest: Option<String>,
    /// The `org.opencontainers.image.version` the `app` container reports —
    /// what the site actually calls itself (`v0.6.1`, `edge+f8bdcb9`), running
    /// or stopped (a resting site keeps its container). `None` without an `app`
    /// container, or when the image predates the stamp. Read off the container,
    /// so it's the version installed, not whatever the tag now points at.
    pub version: Option<String>,
    /// The site's identity (m2-6, D-04) — see [`Stack::site_id`]. `None` until
    /// its first backup, and from a server whose CLI predates it.
    #[serde(default)]
    pub site_id: Option<String>,
    /// The engine sidecar (EXPERIMENTAL, 0483), when `.env` enables it: its
    /// container and whether it runs. `None` without it, and from a server
    /// whose CLI predates it (the Manager then uses the host CLI, m3-7).
    #[serde(default)]
    pub engine: Option<crate::engine_admin::EngineInfo>,
    /// The site is stopped and its database was removed from Docker by
    /// accident ([`DatabaseErased`]): Start refuses, and a restore is the way
    /// back, so the window offers Restore though the site isn't running.
    /// `false` from a server whose CLI predates it.
    #[serde(default)]
    pub database_erased: bool,
}

/// The result of comparing the local image against the registry.
///
/// `Deserialize` because the GUI caches the last one on disk (see
/// [`crate::update_cache`]) and reads it back instead of re-asking the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheck {
    pub image: String,
    /// Local pulled digest.
    pub current: Option<String>,
    /// Registry digest for the same tag.
    pub latest: Option<String>,
    /// The installed image's `org.opencontainers.image.version` label — what the
    /// running Atelier actually calls itself (`v0.1.1`, `edge+f8bdcb9`). Absent on
    /// images built before the stamp existed.
    pub current_version: Option<String>,
    /// Same label, read from the registry without pulling.
    pub latest_version: Option<String>,
    /// `Some(true)` if an update is available, `None` if it couldn't be determined.
    pub update_available: Option<bool>,
    /// Why the check was inconclusive, phrased for the user. `None` when
    /// `update_available` is conclusive.
    pub problem: Option<String>,
    /// The route the update would take, present only when there IS an update.
    /// Usually one hop; more when the target refuses to migrate from this far
    /// back. Reported here so the operator sees a stepped upgrade coming before
    /// committing to it, rather than discovering it mid-run.
    pub plan: Option<UpgradePlan>,
    /// True when the registry couldn't even be connected to (the ~2 s pre-check
    /// failed), so a caller can say "you're offline" and stop — rather than start
    /// an update whose pull is bound to fail. `default` so a cache written before
    /// the field existed still reads.
    #[serde(default)]
    pub offline: bool,
    /// A stepped update stopped before its last hop ([`Stack::update_target`])
    /// and there is still a way to go: offer "Continue update", not "a new
    /// version". `default` for caches and servers that predate it.
    #[serde(default)]
    pub unfinished: bool,
}

/// A released version, as a comparable triple.
///
/// Only `X.Y.Z` (with or without a leading `v`) parses. `dev` and `edge+<sha7>`
/// deliberately do NOT: they have no position in the version order, so no floor
/// can be checked against them, and inventing one would be worse than admitting
/// it. Callers treat `None` as "unknown", never as "old".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

/// Serialized as `"0.3.0"`, not as the `[0,3,0]` a tuple struct would give.
/// Every consumer — `--json`, the GUI — wants to print it, and the ordering the
/// triple exists for is Rust-side only.
impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// The inverse of the `Serialize` above — read back from the update cache.
impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Version::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("not a release version: {s:?}")))
    }
}

impl Version {
    /// Parse a release version, or `None` for anything that isn't one.
    ///
    /// Semver **build metadata** is stripped, because that is how a rolling build
    /// says which release it descends from: `v0.5.1+edge.a1b2c3d` is a build off
    /// main after `v0.5.1`, and comparing it as `0.5.1` is both truthful (it
    /// contains everything 0.5.1 shipped) and conservative (the unreleased work on
    /// top counts for nothing). The older `edge+<sha7>` stamp has no version in
    /// front of the `+` and correctly stays unparseable.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let mut parts = s.split('.');
        let mut next = || parts.next().filter(|p| !p.is_empty())?.parse::<u64>().ok();
        let (major, minor, patch) = (next()?, next()?, next()?);
        parts
            .next()
            .is_none()
            .then_some(Version(major, minor, patch))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The immutable tag a released version is pullable under — the form a waypoint
/// takes. Matches what the release workflow publishes (`v` + the bare version).
pub fn waypoint_image(v: Version) -> String {
    format!("{}:v{v}", crate::stack::IMAGE_REPO)
}

/// One hop of an upgrade: an image to converge onto before going further.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeStep {
    /// The image this hop runs.
    pub image: String,
    /// The version it is, when known (a channel tag's version is read from the
    /// registry; a waypoint's is the version that named it).
    pub version: Option<Version>,
    /// Set on the last hop — the one that lands on what the operator asked for.
    /// Intermediate hops are waypoints nobody chose.
    pub is_target: bool,
    /// Why this hop exists, for a plan the operator can read. `None` on the target.
    pub reason: Option<String>,
}

/// A route from the installed version to the target — one hop in the ordinary
/// case, more when a release refuses to migrate from as far back as this install.
///
/// THE POINT OF THE WHOLE MECHANISM. A release can be unable to migrate
/// arbitrarily old state — most concretely when it deletes the code a pending
/// update needs (drop `drupal/ai` and the update that uninstalls it is gone with
/// it). Each image therefore declares the oldest version it can migrate from, and
/// `converge.sh` refuses anything older before touching the database. That
/// refusal is a safety net, not a user experience: on its own it leaves the
/// operator to work out the route by hand. So the manager reads those declarations
/// FROM THE REGISTRY, WITHOUT PULLING (`dev.atelier.upgrade.min-from`), walks them
/// backwards from the target until it reaches one this install satisfies, and
/// applies the hops in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradePlan {
    /// The version installed now, when the image says (unstamped builds don't).
    pub from: Option<Version>,
    /// The image the operator is heading for — a channel tag, or an explicit one.
    pub target_image: String,
    /// Every hop, in the order they must be applied; the last is the target.
    pub steps: Vec<UpgradeStep>,
    /// Why the route couldn't be verified, phrased for the operator. Set when the
    /// registry or the installed version couldn't be read — in which case `steps`
    /// falls back to the single direct hop, which is safe because the appliance
    /// refuses an impossible migration itself rather than half-performing one.
    pub problem: Option<String>,
    /// Whether the operator named the target themselves (`--to`, a literal image
    /// reference) rather than the channel resolving it. Decides whether applying
    /// the plan may rewrite the configured image: a named target must be written
    /// or it never runs, while a channel plan's target is `prospective_image()` —
    /// writing that would apply a legacy migration the operator may just have
    /// declined.
    pub explicit_target: bool,
}

impl UpgradePlan {
    /// Whether this route passes through versions nobody asked for.
    pub fn is_stepped(&self) -> bool {
        self.steps.len() > 1
    }

    /// The version this plan lands on, when it's a stamped release. `None` for a
    /// literal image reference (a fork, a local build) that carries no version.
    pub fn target_version(&self) -> Option<Version> {
        self.steps.last().and_then(|s| s.version)
    }

    /// True only when the target can be PROVEN older than what's installed — both
    /// versions known and target < from. Unknown on either side is deliberately
    /// `false`: a site's database only migrates forward, so a proven backwards move
    /// is refused, but we never invent a downgrade we can't prove — `converge.sh`
    /// remains the backstop for the cases we can't read (unstamped install, a
    /// literal image target).
    pub fn is_downgrade(&self) -> bool {
        matches!((self.from, self.target_version()), (Some(from), Some(to)) if to < from)
    }

    /// The waypoints — every hop that isn't the destination.
    pub fn waypoints(&self) -> impl Iterator<Item = &UpgradeStep> {
        self.steps.iter().filter(|s| !s.is_target)
    }
}

/// What `data backup --json` reports about the archive it wrote — what a
/// Manager driving this CLI over SSH needs to fetch and verify it (m2-5, D-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    /// Absolute path of the archive on the machine that wrote it.
    pub path: String,
    /// Its file name.
    pub name: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
    /// The identity of the site it was taken from (m2-6, D-04).
    #[serde(default)]
    pub site_id: Option<String>,
    /// That site's name there (`Label (slug)`).
    #[serde(default)]
    pub site_name: Option<String>,
}

/// A backup file on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
pub(crate) struct PsEntry {
    #[serde(rename = "Name", default)]
    pub(crate) name: String,
    #[serde(rename = "Service", default)]
    pub(crate) service: String,
    #[serde(rename = "State", default)]
    pub(crate) state: String,
}

/// Current appliance status. Cheap probes only; never mutates the stack.
pub fn status(stack: &Stack) -> Status {
    let installed = stack.exists();
    let running = installed && preflight().ok() && is_running(stack);

    let reachable = running && http_ready(stack.http_port());

    Status {
        installed,
        running,
        reachable,
        console_url: stack.console_url(),
        site_url: stack.site_url(),
        image: stack.image(),
        channel: stack.channel(),
        image_digest: site_digest(stack),
        version: installed.then(|| app_image_version(stack)).flatten(),
        site_id: stack.site_id(),
        engine: installed
            .then(|| crate::engine_admin::probe(stack))
            .flatten()
            .map(with_engine_state),
        // One `volume inspect`, asked only of a stopped site.
        database_erased: installed && !running && preflight().ok() && stack_database_erased(stack),
    }
}

/// The engine's word for the site (M5: resting, awake, locked, …), when it
/// runs and answers `admin status`; a server's `app status --json` carries
/// it to a Manager that reaches it over SSH.
fn with_engine_state(mut info: crate::engine_admin::EngineInfo) -> crate::engine_admin::EngineInfo {
    use crate::engine_admin::{EngineAdmin, LocalDocker};
    info.state = EngineAdmin::new(&LocalDocker, Some(&info), "status")
        .and_then(|a| a.status_unchecked())
        .ok()
        .map(|st| st.state)
        .filter(|s| !s.is_empty());
    info
}

/// The engine's word for a site that isn't running (resting, locked, …), so a
/// list of sites can say why it is down. `None` without an engine or an answer.
/// Costs a `compose ps` and one `docker exec`, so ask only for stopped sites.
pub fn engine_state_of(stack: &Stack) -> Option<String> {
    crate::engine_admin::probe(stack)
        .map(with_engine_state)
        .and_then(|e| e.state)
}

/// Is the stack's `app` container running? One `compose ps`, no HTTP probe —
/// cheap enough to ask for every site in a list. Assumes Docker is up; a failed
/// probe reads as not running.
pub fn is_running(stack: &Stack) -> bool {
    let mut c = compose(stack);
    c.args(["ps", "--format", "json"]);
    try_capture(c).is_some_and(|out| {
        parse_ps(&out)
            .iter()
            .any(|e| e.service == "app" && e.state.to_lowercase().contains("running"))
    })
}

/// Poll the console until it serves a healthy response, or `timeout` elapses,
/// reporting boot progress as it goes.
///
/// This is what holds the manager's "ready" signal back until Drupal has
/// actually finished booting (site install, `converge.sh` migrations) instead of
/// the moment the container reports "running". Every [`boot::LOOK_EVERY`] it
/// also looks at the containers and the app's log since it last started, so a
/// boot that has already failed, or a site or Docker that went away, ends the
/// wait with the reason instead of running out the clock; and it reads the log
/// once more before calling an answering site ready, because the entrypoint
/// starts the web server even after a failed converge ([`Boot`]).
pub fn wait_until_ready(stack: &Stack, timeout: Duration, r: &mut dyn Reporter) -> Boot {
    let port = stack.http_port();
    let started = Instant::now();
    let deadline = started + timeout;
    let mut next_look = started + boot::LOOK_EVERY;
    // What every service's restart count was before the wait: a restart
    // policy that starts a killed container again within a second hides the
    // death from a look that only asks "is it running?".
    let wait_started = std::time::SystemTime::now();
    let mut strikes = boot::Strikes::from_baseline(&boot::services_now(stack));
    r.stage(
        Stage::Booting,
        "Waiting for the console to finish booting…",
        Some(BOOT_FROM),
    );
    loop {
        if http_ready(port) {
            let look = boot::look(stack, false);
            if let Some(why) = look.log.as_deref().and_then(boot::boot_failure) {
                let why = memory_or(stack, wait_started, &strikes, why, r);
                return boot_failed(r, &look, why);
            }
            r.stage(Stage::Ready, "The console is up.", Some(1.0));
            return Boot::Ready;
        }
        let now = Instant::now();
        if now >= next_look {
            let look = boot::look(stack, true);
            if let Some(why) = boot::judge(&look, &mut strikes) {
                let why = memory_or(stack, wait_started, &strikes, why, r);
                return boot_failed(r, &look, why);
            }
            next_look = Instant::now() + boot::LOOK_EVERY;
        }
        if Instant::now() >= deadline {
            // Ran out of time: memory may be why (a container Docker keeps
            // restarting between looks), and that is worth saying.
            if let Some(why) = boot::memory_verdict(stack, wait_started, &strikes) {
                return Boot::Failed(why);
            }
            r.stage(
                Stage::Booting,
                "Still booting — this is taking longer than usual.",
                Some(0.95),
            );
            return Boot::Booting;
        }
        // Creep the bar across the boot window (0.72 → 0.95) and tick the timer.
        let elapsed = started.elapsed();
        let frac = BOOT_FROM + (0.95 - BOOT_FROM) * (elapsed.as_secs_f32() / timeout.as_secs_f32());
        r.stage(
            Stage::Booting,
            &format!("Booting… ({}s)", elapsed.as_secs()),
            Some(frac.min(0.95)),
        );
        std::thread::sleep(Duration::from_millis(750));
    }
}

/// The reason a failed wait gives: out of memory when Docker's own record
/// says so ([`boot::memory_verdict`]: the flag, an `oom` event, a killed
/// Postgres process), since a converge that failed or a service that stopped
/// is then only the symptom; `why` otherwise. Docker going away and a
/// half-attached network keep theirs: the owner's step there is restarting
/// Docker, and Docker's record can't be read.
fn memory_or(
    stack: &Stack,
    started: std::time::SystemTime,
    strikes: &boot::Strikes,
    why: String,
    r: &mut dyn Reporter,
) -> String {
    if why == boot::NETWORK_STUCK || why == boot::DOCKER_STOPPED {
        return why;
    }
    match boot::memory_verdict(stack, started, strikes) {
        Some(memory) => {
            r.log(&format!("The wait saw: {why}"));
            memory
        }
        None => why,
    }
}

/// End a wait as failed: the app's own failure lines go to the log (a bug
/// report wants them), the reason to the caller.
fn boot_failed(r: &mut dyn Reporter, look: &boot::Look, why: String) -> Boot {
    for line in look
        .log
        .iter()
        .flat_map(|l| boot::after_last_success(l).lines())
        .filter(|l| l.contains("[converge] FATAL") || l.contains("converge FAILED"))
    {
        r.log(line);
    }
    for (network, container) in &look.half_attached {
        r.log(&format!("Half attached: {container} in {network}"));
    }
    Boot::Failed(why)
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

/// Compare the local image to the registry tag, reading both digests (the
/// comparison) and both version labels (what to *call* them).
///
/// Inconclusive is a legitimate outcome, but it must say why: an unattributed
/// "couldn't check" is what made atelier-cms#7 undiagnosable from a bug report.
/// Each failure is attributed to one of five causes — Docker unavailable, the
/// image not pulled, `buildx` missing, the host offline (no connection to the
/// registry at all, decided in ~2 s without spawning buildx), the registry
/// unreachable — since the fix differs for every one of them. Every probe here
/// has a deadline, so the check always comes back (manager#6).
pub fn check_update(stack: &Stack) -> UpdateCheck {
    let image = stack.image();
    // What the registry gets asked about is where an update would *come from*, which
    // on a legacy install is not what `.env` says — see `Stack::prospective_image`.
    // The local probe below keeps using `image`, because that is what is on this
    // machine. Digests compare across the rename regardless: `parse_local_probe`
    // keeps only the `sha256:…`, and the rename moved the package rather than
    // republishing it.
    let mut check = UpdateCheck {
        image: image.clone(),
        current: None,
        latest: None,
        current_version: None,
        latest_version: None,
        update_available: None,
        problem: None,
        plan: None,
        offline: false,
        unfinished: false,
    };

    // Docker itself is the floor: without it neither probe can say anything, and
    // `preflight` already phrases that case well.
    if let Some(problem) = preflight().problem() {
        check.problem = Some(problem);
        return check;
    }

    // The site's own build is the one its `app` container runs, not whatever the
    // tag points at now: sites share the moving tag, so another site's update
    // re-points `:latest` while this one keeps running the old build. Comparing
    // the tag would then report "up to date" next to an old version. Without a
    // container (not installed yet, or removed), the tag is all there is.
    match site_probe(stack) {
        Ok(probe) => {
            check.current = probe.digest;
            check.current_version = probe.version;
            if check.current.is_none() {
                check.problem = Some(format!(
                    "{image} was built or loaded locally, so it carries no registry \
                     digest to compare against."
                ));
            }
        }
        Err(e) => {
            check.problem = Some(format!(
                "{image} isn't on this machine yet, so there's nothing to compare — \
                 install Atelier first. ({e})"
            ));
        }
    }

    // An unfinished stepped update heads for the image its route ends on: the
    // `.env` now names the waypoint it stopped at, which has nothing newer. One
    // the site has since moved past is dropped ([`live_update_target`]).
    let resumes = live_update_target(stack, check.current_version.as_deref());
    let target_image = resumes.clone().unwrap_or_else(|| stack.prospective_image());
    let mut target = None;
    match remote_probe(&target_image) {
        Ok(probe) => {
            check.latest = probe.digest.clone();
            check.latest_version = probe.version.clone();
            target = Some(probe);
        }
        Err(e) => {
            check.offline = e == REGISTRY_OFFLINE;
            if check.problem.is_none() {
                check.problem = Some(registry_problem(&target_image, &e));
            }
        }
    }

    check.update_available = match (&check.current, &check.latest) {
        (Some(c), Some(l)) => Some(c != l),
        _ => None,
    };
    if check.update_available.is_some() {
        check.problem = None;
    } else if check.problem.is_none() {
        check.problem = Some(format!("Couldn't compare {image} against the registry."));
    }

    // Only when there's something to do: the route is advice about an update, and
    // computing it probes the registry once per waypoint considered.
    if check.update_available == Some(true) {
        if let Some(target) = target {
            let from = check.current_version.as_deref().and_then(Version::parse);
            let mut plan = plan_route(from, target_image, target, None);
            // A resumed route must write its target, or a last single hop
            // would pull the waypoint `.env` names again.
            plan.explicit_target = resumes.is_some();
            check.plan = Some(plan);
        }
        check.unfinished = resumes.is_some();
    }
    check
}

/// The image an update heads for and whether the route must write it into
/// `.env` (see [`UpgradePlan::explicit_target`]): the one the operator named,
/// else an unfinished update's target ([`Stack::update_target`]), else the
/// channel's. A resumed target counts as named: the hops before it rewrote
/// `.env` to a waypoint, so nothing else would bring the channel image back.
fn route_target(
    named: Option<String>,
    unfinished: Option<String>,
    prospective: impl FnOnce() -> String,
) -> (String, bool) {
    match (named, unfinished) {
        (Some(image), _) | (None, Some(image)) => (image, true),
        (None, None) => (prospective(), false),
    }
}

/// The version an image reference names by its tag (`…/atelier-cms:v0.21.0`,
/// as [`waypoint_image`] writes it); `None` for a moving tag or a digest.
fn tag_version(image: &str) -> Option<Version> {
    let name = image.split('@').next()?;
    let (_, tag) = name.rsplit_once(':').filter(|(_, t)| !t.contains('/'))?;
    Version::parse(tag)
}

/// Whether an unfinished update's target is stale: the site already runs that
/// version or a newer one (an install, a restore, or another route moved it
/// past). Unknown on either side is not stale, the way it always was.
fn update_target_is_stale(target: Option<Version>, running: Option<Version>) -> bool {
    matches!((target, running), (Some(t), Some(r)) if t <= r)
}

/// [`Stack::update_target`], unless it is stale ([`update_target_is_stale`])
/// next to the version the site runs (`running`): then it is deleted, so check
/// stops saying "update unfinished" and Update stops heading for an older
/// image (review 2026-11-14). A target with no version in its tag is asked of
/// the registry.
fn live_update_target(stack: &Stack, running: Option<&str>) -> Option<String> {
    live_update_target_with(stack, running, |image| {
        remote_probe(image)
            .ok()?
            .version
            .as_deref()
            .and_then(Version::parse)
    })
}

fn live_update_target_with(
    stack: &Stack,
    running: Option<&str>,
    registry_version: impl FnOnce(&str) -> Option<Version>,
) -> Option<String> {
    let target = stack.update_target()?;
    // Unknown is never stale: keep the route without asking the registry.
    let Some(running) = running.and_then(Version::parse) else {
        return Some(target);
    };
    let version = tag_version(&target).or_else(|| registry_version(&target));
    if update_target_is_stale(version, Some(running)) {
        stack.clear_update_target();
        return None;
    }
    Some(target)
}

/// After an install or a restore: drop an unfinished update's target the site
/// has moved past ([`live_update_target`]). One still ahead is kept: its hops
/// pinned `.env` to a waypoint, and without the target the next update would
/// find nothing newer than that waypoint.
fn drop_stale_update_target(stack: &Stack) {
    if stack.update_target().is_none() {
        return;
    }
    let running = site_probe(stack).ok().and_then(|p| p.version);
    let _ = live_update_target(stack, running.as_deref());
}

/// How many waypoints a route may contain before we call it broken rather than
/// long. Each hop is a full pull + migrate + health check, and a floor chain that
/// deep would mean a release history nobody should be upgrading across in one go;
/// far more likely it means the floors themselves are wrong, and looping forever
/// on bad metadata is the one outcome worse than saying so.
const MAX_WAYPOINTS: usize = 8;

/// Work out the route from the installed version to `to` (or to whatever the
/// current channel points at), walking each candidate image's declared upgrade
/// floor backwards until one this install satisfies.
///
/// Reads floors from the registry, so it costs one `imagetools inspect` per hop
/// considered and pulls nothing. See [`UpgradePlan`] for why this exists at all.
pub fn plan_upgrade(stack: &Stack, to: Option<&str>) -> UpgradePlan {
    let named = to.map(|v| match Version::parse(v) {
        Some(v) => waypoint_image(v),
        // Not a version — take it as a literal image reference, which is how
        // an operator pins a fork or a local build.
        None => v.to_string(),
    });
    // The image an update is heading for is the one that will be configured when
    // it runs — so the pending legacy repairs count (a `:edge` → stable move, a
    // retired repository name), or the route would be planned against a tag the
    // operator is about to leave, or against a repository that cannot answer.
    // An unfinished stepped update continues to where it was going.
    let probe = site_probe(stack);
    let running = probe.as_ref().ok().and_then(|p| p.version.as_deref());
    let (target_image, explicit_target) =
        route_target(named, live_update_target(stack, running), || {
            stack.prospective_image()
        });

    let (from, from_problem) = match probe {
        Ok(p) => (
            p.version.as_deref().and_then(Version::parse),
            p.version.is_none().then(|| {
                format!(
                    "The installed image ({}) carries no version stamp, so we can't tell \
                     how far back this site is. Going straight there; the appliance itself \
                     refuses a migration it can't perform, without touching your data.",
                    stack.image()
                )
            }),
        ),
        Err(e) => (
            None,
            Some(format!(
                "Couldn't read the installed image to see which version it is. ({e})"
            )),
        ),
    };

    let mut plan = match remote_probe(&target_image) {
        Ok(target) => plan_route(from, target_image, target, from_problem),
        Err(e) => {
            // No floors to read → the direct hop, and say why it wasn't verified.
            // Not an error: the appliance's own refusal covers the unsafe case, so
            // an unreadable registry costs the route, not the safety.
            let mut plan = plan_route(from, target_image.clone(), ImageProbe::default(), None);
            plan.problem = Some(registry_problem(&target_image, &e));
            plan
        }
    };
    plan.explicit_target = explicit_target;
    plan
}

/// Build the route from an already-probed target. Split out so [`check_update`]
/// can reuse the probes it just paid for.
fn plan_route(
    from: Option<Version>,
    target_image: String,
    target: ImageProbe,
    problem: Option<String>,
) -> UpgradePlan {
    plan_route_with(from, target_image, target, problem, |image| {
        remote_probe(image).map(|p| p.floor)
    })
}

/// The route walk, over an injected floor lookup.
///
/// The lookup is a parameter purely so this is testable without a registry: the
/// backwards walk, its ordering, and the two guards against metadata that can't
/// resolve are the whole mechanism, and they are exactly what a live-registry test
/// could never exercise (there is no published release with a broken floor chain,
/// and there had better never be one).
fn plan_route_with(
    from: Option<Version>,
    target_image: String,
    target: ImageProbe,
    mut problem: Option<String>,
    floor_of: impl Fn(&str) -> std::result::Result<Option<Version>, String>,
) -> UpgradePlan {
    let target_version = target.version.as_deref().and_then(Version::parse);
    let mut waypoints: Vec<(Version, String)> = Vec::new();
    let mut floor = target.floor;

    // Walk backwards: while the image we intend to run can't migrate from where we
    // are, the version it names becomes a hop, and we ask the same of that one.
    // Terminates on the first floor this install satisfies — or on the guards
    // below, which exist because the chain is built from metadata we don't get to
    // re-verify at read time.
    while let Some(needed) = floor {
        let Some(current) = from else {
            // Unknown installed version: we can't say whether the floor is met.
            // Don't invent a route through versions the operator may already be
            // past — go direct and let the appliance decide, which it does safely.
            break;
        };
        if current >= needed {
            break;
        }
        if waypoints.iter().any(|(v, _)| *v == needed) || waypoints.len() >= MAX_WAYPOINTS {
            problem = Some(format!(
                "The published upgrade requirements don't resolve to a route from \
                 {current} (stuck at {needed}). Upgrading one release at a time, or \
                 restoring onto a current install, is the way through this."
            ));
            break;
        }
        let image = waypoint_image(needed);
        let next_floor = match floor_of(&image) {
            Ok(f) => f,
            Err(e) => {
                problem = Some(format!(
                    "This upgrade has to pass through {needed}, but that release \
                     couldn't be read from the registry, so the rest of the route is \
                     unknown. ({e})"
                ));
                waypoints.push((needed, image));
                break;
            }
        };
        waypoints.push((needed, image));
        // A floor must be strictly older than the release declaring it, or the
        // chain can't converge. Treat a non-decreasing one as absent and stop.
        floor = next_floor.filter(|f| *f < needed);
    }

    // Collected newest-first; applied oldest-first.
    waypoints.reverse();
    let hops = waypoints.len();
    let mut steps: Vec<UpgradeStep> = waypoints
        .into_iter()
        .enumerate()
        .map(|(i, (version, image))| UpgradeStep {
            image,
            version: Some(version),
            is_target: false,
            reason: Some(if i + 1 == hops {
                match target_version {
                    Some(t) => format!("{t} can't migrate a site this old directly"),
                    None => "the version you're heading to can't migrate a site this old \
                             directly"
                        .to_string(),
                }
            } else {
                "needed to reach the next step".to_string()
            }),
        })
        .collect();
    steps.push(UpgradeStep {
        image: target_image.clone(),
        version: target_version,
        is_target: true,
        reason: None,
    });

    UpgradePlan {
        from,
        target_image,
        steps,
        problem,
        explicit_target: false,
    }
}

/// The cause reported when the registry can't even be connected to — the
/// connectivity pre-check in [`remote_probe`] failed, so no registry read was
/// attempted. Also the error [`remote_probe`] returns in that case, which is how
/// [`registry_problem`] recognises it.
const REGISTRY_OFFLINE: &str = "You're offline — couldn't reach the registry.";

/// Turn a failed registry read into advice. A missing `buildx` plugin is the
/// common one — it ships with Docker Desktop but is a separate package on Linux
/// (`docker-buildx-plugin`), and nothing else in the manager needs it, so a host
/// without it works fine right up to the update check.
fn registry_problem(image: &str, error: &str) -> String {
    // Already phrased for the user, and the fix is the user's network, not Docker.
    if error == REGISTRY_OFFLINE {
        return REGISTRY_OFFLINE.to_string();
    }
    let e = error.to_lowercase();
    if e.contains("buildx") && (e.contains("unknown command") || e.contains("not a docker command"))
    {
        return "Docker's buildx plugin is missing — it's what reads the registry. \
                Install it (`docker-buildx-plugin` on Linux; it ships with Docker \
                Desktop), then check again. Everything else works without it."
            .to_string();
    }
    format!("Couldn't reach the registry to look up {image}. ({error})")
}

/// Lay down the stack (if needed), pull the image, and start it. Idempotent:
/// re-running an existing stack is an upgrade — `converge.sh` migrates in place
/// and auto-rolls-back on failure. Returns whether the console came up before
/// the readiness timeout (`false` = started but still booting).
pub fn install(stack: &Stack, opts: &InstallOptions, r: &mut dyn Reporter) -> Result<bool> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Install)?;
    lifecycle_plan().announce(r);
    r.stage(Stage::Preflight, "Checking Docker…", Some(0.04));
    preflight().require()?;
    r.stage(Stage::Scaffold, "Preparing the stack…", Some(0.08));
    if let Some(notice) = stack.ensure_scaffold(opts)?.notice(stack) {
        r.stage(Stage::Scaffold, &notice, Some(0.08));
    }
    if opts.image.is_none() {
        announce_legacy_migrations(stack, r)?;
    }
    pull(stack, r)?;
    // Exempt from the erased-database refusal: an install lays the site down
    // (and a reinstall has just wiped it on purpose).
    up_unguarded(stack, r)?;
    // An unfinished update's target (review 2026-11-14): an image the owner
    // named replaces the route; otherwise only one the site is past goes.
    if opts.image.is_some() {
        stack.clear_update_target();
    } else {
        drop_stale_update_target(stack);
    }
    wait_until_ready(stack, READY_TIMEOUT, r).ready()
}

/// Pull a newer image and recreate the stack — the upgrade path. Plans the route
/// itself and walks every hop it needs; see [`apply_upgrade`] to show the operator
/// the route first. Says how it came out ([`Updated`]).
pub fn update(stack: &Stack, r: &mut dyn Reporter) -> Result<Updated> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Update)?;
    let plan = plan_upgrade(stack, None);
    apply_upgrade(stack, &plan, r)
}

/// How an update came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updated {
    /// The site answered on the new version.
    Ready,
    /// Updated, but the console hadn't answered by the readiness timeout.
    Booting,
    /// The download was the build the site already runs and nothing else was
    /// due: no backup was taken and nothing restarted.
    AlreadyCurrent,
    /// The download was the build the site already runs, but its settings
    /// (`.env`, `compose.yaml`) had changed since its containers were made: the
    /// containers were made again on the same build, without a backup (the
    /// version didn't change), and the site answered.
    SettingsApplied,
}

impl Updated {
    /// Whether the site is serving: an update that found nothing to do left a
    /// site that was serving still serving.
    pub fn ready(self) -> bool {
        self != Updated::Booting
    }

    fn from_ready(ready: bool) -> Self {
        if ready {
            Updated::Ready
        } else {
            Updated::Booting
        }
    }
}

/// Apply a route, hop by hop. Returns whether the console came up before the
/// readiness timeout **on the final hop**.
///
/// EVERY HOP IS A FULL CONVERGE, and it must finish before the next one starts:
/// each image's floor is checked against the version the site RECORDED on its last
/// successful converge, so hopping ahead of a converge would present the next image
/// with the state of two versions ago and earn its refusal. So a hop that doesn't
/// come up stops the route rather than pressing on — the site is left on the last
/// version that did converge, which is a state the appliance guarantees (converge
/// rolls its database back if the migration or the health check fails).
/// Write a single-hop plan's image into the install before running it — but only
/// when the operator named that image. The stepped loop writes each hop's image as
/// it goes; the single-hop path historically wrote nothing, so `update --to X` on
/// an install configured for another tag pulled the configured tag, told the
/// operator it was now pinned to it, and never ran X at all. A channel plan must
/// NOT be written here: its target is `prospective_image()`, and writing that
/// would apply a legacy migration the operator may just have declined.
fn retarget_single_hop(stack: &Stack, plan: &UpgradePlan) -> Result<()> {
    if plan.explicit_target && stack.image() != plan.target_image {
        stack.set_image(&plan.target_image)?;
    }
    Ok(())
}

/// Swap an untouched older shipped `edge.conf` for the current one before an
/// upgrade, and have a running `edge` reload it, so the page visitors see while
/// `app` restarts is this manager's. nginx reads its config only at start, so
/// without the reload the old page would stay until `edge` next restarts.
/// Best-effort: a failed write or reload never blocks the upgrade (the file is
/// then picked up at the next restart, and doctor still sees it as shipped).
fn refresh_edge_conf(stack: &Stack) {
    if let Ok(true) = stack.refresh_shipped_edge_conf() {
        crate::docker::reload_edge(stack);
    }
}

/// Swap an untouched older shipped `compose.yaml` for the current one (the db
/// restart policy, 0.17.0), keeping its images. Returns whether it changed; the
/// containers take it on their next `up`. Best-effort like
/// [`refresh_edge_conf`]: a failed write is logged and the stack runs as it is.
fn refresh_compose(stack: &Stack, r: &mut dyn Reporter) -> bool {
    match stack.refresh_shipped_compose() {
        Ok(changed) => changed,
        Err(e) => {
            r.log(&format!(
                "Couldn't update compose.yaml ({e:#}); carrying on with it as it is."
            ));
            false
        }
    }
}

/// Where a stepped route's first hop starts the bar (the pre-route backup fills
/// the slice before it).
const ROUTE_START: f32 = 0.05;

/// The steps [`apply_upgrade`] will report: the lifecycle phases for one hop, or
/// one block per hop of a stepped route, labelled by the version it lands on.
fn upgrade_plan(plan: &UpgradePlan) -> Plan {
    if !plan.is_stepped() {
        return lifecycle_plan();
    }
    let total = plan.steps.len();
    let span = (1.0 - ROUTE_START) / total as f32;
    let mut steps = vec![PlanStep {
        id: "prepare".into(),
        label: "Prep".into(),
        from: 0.0,
        to: ROUTE_START,
    }];
    for (i, hop) in plan.steps.iter().enumerate() {
        let label = match hop.version {
            Some(v) => v.to_string(),
            None => format!("Step {}", i + 1),
        };
        steps.push(PlanStep {
            id: format!("hop-{}", i + 1),
            label,
            from: ROUTE_START + span * i as f32,
            to: ROUTE_START + span * (i + 1) as f32,
        });
    }
    Plan { steps }
}

pub fn apply_upgrade(stack: &Stack, plan: &UpgradePlan, r: &mut dyn Reporter) -> Result<Updated> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Update)?;
    apply_upgrade_with(stack, plan, &UpdateOptions::default(), r)
}

/// How [`apply_upgrade_with`] treats the backup before an update.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// Update without a backup first (`--no-backup`, "Update without a backup"):
    /// a stopped site is not started for one, a running one is not backed up.
    /// Without it, a stopped site that won't start refuses the update with
    /// [`BackupBeforeUpdateFailed`].
    pub no_backup: bool,
}

/// An update was refused because the site couldn't be backed up first: it is
/// stopped and wouldn't start (or didn't finish starting) on the version it
/// runs. Carried as an error's context, so the cause (a [`boot::BootFailed`] that
/// needs Docker restarted, say) can still be told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupBeforeUpdateFailed {
    /// What stopped the backup, in words.
    pub cause: String,
}

impl std::fmt::Display for BackupBeforeUpdateFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, so it wasn't updated", self.cause)
    }
}

impl std::error::Error for BackupBeforeUpdateFailed {}

/// An update refused before anything was done: the image it would download is
/// a build made on this computer (a developer build, a local tag such as
/// `atelier-cms:zoo-shared`), which no registry has, so `compose pull` could
/// only fail with Docker's "pull access denied". The way forward is following
/// a channel again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBuild {
    /// The image reference the site names (`AINCIENT_IMAGE`, or `--to`).
    pub image: String,
}

impl std::fmt::Display for LocalBuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "This site runs a build made on this computer ({}), so there is nothing to \
             download, and nothing was changed. To follow released versions: `atelier app \
             channel stable` (in the app: System → Updates → Released versions), then \
             update again.",
            self.image
        )
    }
}

impl std::error::Error for LocalBuild {}

impl LocalBuild {
    /// Whether `e` is this refusal.
    pub fn is(e: &anyhow::Error) -> bool {
        e.is::<LocalBuild>()
    }
}

/// Whether `image` names a build only this computer has: its reference names
/// no registry host (so a pull asks Docker Hub, which has no Atelier image)
/// and the local image was never downloaded under that name (no `RepoDigests`
/// entry for its repository: a retag of a downloaded image, such as
/// `atelier-cms:zoo-shared`, carries the source's digest under the source's
/// name only). `pull_policy: never` on `app` is the owner telling compose to
/// use the local image as it is (the scenario harness's local builds), so a
/// pull takes nothing and the update goes ahead. An image that isn't here
/// (`repo_digests` `None`) is left to the pull. A digest reference is never
/// local-only. Pure: [`local_build`] feeds it what Docker says.
fn is_local_only(
    image: &str,
    app_pull_policy: Option<&str>,
    repo_digests: Option<&[String]>,
) -> bool {
    if app_pull_policy == Some("never") || image.contains('@') {
        return false;
    }
    let Some(digests) = repo_digests else {
        return false;
    };
    // The repository: the reference without its tag (a `:` after the last `/`).
    let repo = match image.rsplit_once(':') {
        Some((r, tag)) if !tag.contains('/') => r,
        _ => image,
    };
    let host = repo.split_once('/').map(|(first, _)| first);
    if host.is_some_and(|h| h.contains('.') || h.contains(':') || h == "localhost") {
        return false;
    }
    let same_repo = |d: &String| {
        let name = d.split('@').next().unwrap_or_default();
        name == repo
            || name == format!("docker.io/{repo}")
            || name == format!("docker.io/library/{repo}")
    };
    !digests.iter().any(same_repo)
}

/// `pull_policy` of the `app` service in `compose config --format json`.
fn parse_app_pull_policy(config_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(config_json).ok()?;
    v.get("services")?
        .get("app")?
        .get("pull_policy")?
        .as_str()
        .map(str::to_string)
}

/// [`LocalBuild`] when an update would download `image` for `stack` and no
/// registry has it ([`is_local_only`]). Reads only; `None` when Docker
/// doesn't answer (the pull says what's wrong then).
fn local_build(stack: &Stack, image: &str) -> Option<LocalBuild> {
    let mut config = compose(stack);
    config.args(["config", "--format", "json"]);
    let policy = docker::try_capture(config).and_then(|j| parse_app_pull_policy(&j));
    let mut inspect = docker::docker();
    inspect.args([
        "image",
        "inspect",
        image,
        "--format",
        "{{json .RepoDigests}}",
    ]);
    let digests: Option<Vec<String>> = docker::try_capture(inspect).map(|j| {
        serde_json::from_str::<Option<Vec<String>>>(j.trim())
            .ok()
            .flatten()
            .unwrap_or_default()
    });
    is_local_only(image, policy.as_deref(), digests.as_deref()).then(|| LocalBuild {
        image: image.to_string(),
    })
}

/// [`LocalBuild`] when applying `plan` would download a build only this
/// computer has (a single hop onto it: a stepped route's hops are registry
/// releases). Reads only; the CLI asks before printing the plan, whose
/// registry lookup of such an image fails with Docker's own words.
pub fn plan_local_build(stack: &Stack, plan: &UpgradePlan) -> Option<LocalBuild> {
    if plan.is_stepped() {
        return None;
    }
    let pulls = if plan.explicit_target {
        plan.target_image.clone()
    } else {
        stack.image()
    };
    local_build(stack, &pulls)
}

/// The before-update backup of a running site, or the refusal: an update that
/// migrates with no way back is worse than no update (a full disk is the usual
/// cause, and the same disk would fail the migration's own snapshot). The owner
/// can go on without one ([`UpdateOptions::no_backup`]). Was "Continuing" until
/// the prior-art review (2026-11-14).
fn backup_or_refuse<T>(outcome: Result<T>) -> Result<T> {
    outcome.map_err(|e| {
        let cause = match e.downcast_ref::<BackupsNotWritable>() {
            Some(folder) => format!(
                "Couldn't back up your site first: {} (fix the folder's permissions or free \
                 it up, then try again)",
                folder.cause()
            ),
            None => format!("Couldn't back up your site first ({e:#})"),
        };
        anyhow::Error::new(BackupBeforeUpdateFailed { cause })
    })
}

/// [`update`] with [`UpdateOptions`].
pub fn update_with(stack: &Stack, opts: &UpdateOptions, r: &mut dyn Reporter) -> Result<Updated> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Update)?;
    let plan = plan_upgrade(stack, None);
    apply_upgrade_with(stack, &plan, opts, r)
}

/// [`apply_upgrade`] with [`UpdateOptions`].
pub fn apply_upgrade_with(
    stack: &Stack,
    plan: &UpgradePlan,
    opts: &UpdateOptions,
    r: &mut dyn Reporter,
) -> Result<Updated> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Update)?;
    let done = run_upgrade(stack, plan, opts, r)?;
    if done == Updated::Ready {
        // Only after the site answered on the new version: a failed attempt
        // keeps every backup, so retrying can never prune the last good one.
        prune_automatic_backups(stack, None, r);
        tidy_after_update(r);
    }
    Ok(done)
}

/// After an update the site answered on: remove the old Atelier versions and
/// replaced database/web-server images no container uses (DECISIONS 0527).
/// Best effort and quiet: a leftover only costs disk, never the update.
fn tidy_after_update(r: &mut dyn Reporter) {
    if let Ok(done) = crate::cleanup::free(crate::cleanup::Scope::AfterUpdate, None) {
        if !done.images.is_empty() {
            r.log(&format!(
                "Removed {} old download{}, up to {} freed.",
                done.images.len(),
                if done.images.len() == 1 { "" } else { "s" },
                crate::cleanup::human_bytes(done.total_bytes)
            ));
        }
    }
}

fn run_upgrade(
    stack: &Stack,
    plan: &UpgradePlan,
    opts: &UpdateOptions,
    r: &mut dyn Reporter,
) -> Result<Updated> {
    ensure_installed(stack)?;
    // Set once `up` has touched the containers: a failure from then on saves
    // every service's log before anything restarts them ([`keep_logs`]).
    let touched = std::cell::Cell::new(false);
    // One source for the groups the front end draws and the slices the hops
    // report into (review 2026-11-12: these were computed twice).
    let route = upgrade_plan(plan);
    route.announce(r);
    r.stage(
        Stage::Preflight,
        "Checking your site is ready to update…",
        Some(0.02),
    );
    // Docker missing, or its compose plugin: preflight's own words (an install
    // problem, not a site that isn't ready).
    let pf = preflight();
    if !pf.docker_installed || !pf.compose_available {
        pf.require()?;
    }
    // A build made on this computer has nothing to download: `compose pull`
    // would fail with Docker's "pull access denied". Refused before anything
    // is touched (the zoo's shared-tag sites, a developer build). A stepped
    // route's hops are registry releases.
    if let Some(local) = plan_local_build(stack, plan) {
        return Err(anyhow::Error::new(local));
    }
    // Refuse before any work while the site isn't ready (M2c, the HA model):
    // Docker not answering, too little disk, an erased database, a container
    // off its network, a service out of memory or crash-looping, a locked
    // engine site. Nothing below repairs those mid-update.
    let facts = crate::gate::gather(stack, &pf);
    crate::gate::require_ready(&facts)?;
    let engine_state = facts.engine_state;
    let files_before = stack_files(stack);
    let snapshot_taken = announce_legacy_migrations(stack, r)?;
    refresh_edge_conf(stack);
    // Every route ends in `up`, which hands the containers the new file.
    refresh_compose(stack, r);
    let files_changed = stack_files(stack) != files_before;

    if !plan.is_stepped() {
        retarget_single_hop(stack, plan)?;
        // Download first: the backup and the restart are only worth their time
        // when the download brought something new.
        let running = running_image_id(stack);
        pull(stack, r)?;
        let need = update_need(
            running.as_deref(),
            image_id(&stack.image()).as_deref(),
            settings_changed_now(stack),
            files_changed,
            stack.pending_db_image_migration().is_some(),
        );
        if need == UpdateNeed::Nothing {
            // The site runs the route's target: nothing is unfinished.
            stack.clear_update_target();
            r.stage(
                Stage::Ready,
                "Already up to date: the download is the version your site runs, so nothing \
                 was backed up or restarted.",
                Some(1.0),
            );
            return Ok(Updated::AlreadyCurrent);
        }
        if need == UpdateNeed::Settings {
            stack.clear_update_target();
            // A stopped site starts (a resting one wakes, and rests again
            // after), as for an update; there is nothing to back up for.
            let woke = wake_for_update(stack, engine_state.as_deref(), opts, r)?;
            let done = (|| -> Result<Updated> {
                r.stage(
                    Stage::Starting,
                    "Already up to date. Applying your changed settings…",
                    Some(UP_FROM),
                );
                touched.set(true);
                up_updating(stack, r)?;
                let ready = wait_until_ready(stack, READY_TIMEOUT, r).ready()?;
                if ready {
                    r.stage(Stage::Ready, SETTINGS_APPLIED, Some(1.0));
                    return Ok(Updated::SettingsApplied);
                }
                Ok(Updated::Booting)
            })();
            let done = keep_logs(stack, &touched, done, r);
            return settle_woken(stack, woke, done, r);
        }
        let woke = wake_for_update(stack, engine_state.as_deref(), opts, r)?;
        let done = (|| -> Result<Updated> {
            // The banner promises "Updating keeps all your pages and settings": a
            // one-step update took no backup until 0.17.0, so a failed migration had
            // nothing to go back to. Best effort, like the stepped route's snapshot;
            // skipped when the channel move above just took one, or when the owner
            // said to go without.
            if !snapshot_taken && !opts.no_backup {
                backup_or_refuse(safety_backup(
                    stack,
                    "before-update",
                    "Backing up before the update…",
                    0.03,
                    PULL_FROM - 0.04,
                    r,
                ))?;
            }
            touched.set(true);
            up_updating(stack, r)?;
            let mut ready = wait_until_ready(stack, READY_TIMEOUT, r).ready()?;
            if ready {
                stack.clear_update_target();
                ready = move_db_image(stack, r)?;
            }
            Ok(Updated::from_ready(ready))
        })();
        let done = keep_logs(stack, &touched, done, r);
        return settle_woken(stack, woke, done, r);
    }

    // A stepped route is the case where a snapshot is least optional: it is longer,
    // it crosses releases the operator never chose to run, and it is only ever
    // planned because one of them changes state in a way that can't be reversed by
    // running the old image again. Best effort, like the channel-switch snapshot —
    // converge's own per-hop rollback is the guarantee; this is the belt.
    let total = plan.steps.len();
    let woke = wake_for_update(stack, engine_state.as_deref(), opts, r)?;
    let done = (|| -> Result<Updated> {
        if !opts.no_backup && status(stack).running {
            r.stage(
                Stage::Working,
                "Backing up before the first step…",
                Some(0.03),
            );
            // The backup's own steps land inside this slice; its plan is dropped.
            let mut snap = ScaledReporter {
                inner: r,
                base: 0.03,
                span: ROUTE_START - 0.03,
                prefix: String::new(),
            };
            let path = backup_or_refuse(backup(stack, Some("before-stepped-upgrade"), &mut snap))?;
            r.stage(
                Stage::Working,
                &format!("Snapshot saved: {}", path.display()),
                Some(0.05),
            );
        }

        // Each hop rewrites `.env` to the version it lands on, so a route that
        // stops halfway must remember where it was going, or the next update would
        // find nothing newer than the waypoint ([`Stack::update_target`]).
        stack.set_update_target(&plan.target_image)?;

        // Hops share the bar after the Prep slice, each rescaling the stage fractions
        // the shared pull/up/wait helpers report so a five-minute route doesn't look
        // like five separate runs that each finish at 100%. Slice and label come from
        // the announced plan (step 0 is Prep); the prefix names the hop's version only,
        // since the plan already numbers the steps (`[2/4]` in the terminal).
        for (i, step) in plan.steps.iter().enumerate() {
            let slice = &route.steps()[i + 1];
            let label = match step.version {
                Some(v) => v.to_string(),
                None => step.image.clone(),
            };
            let mut hop = ScaledReporter {
                inner: r,
                base: slice.from,
                span: slice.to - slice.from,
                prefix: format!("{}: ", slice.label),
            };
            hop.stage(
                Stage::Scaffold,
                &format!("switching to {}…", step.image),
                Some(0.0),
            );
            stack.set_image(&step.image)?;
            pull(stack, &mut hop)?;
            touched.set(true);
            up_updating(stack, &mut hop)?;
            let ready = match wait_until_ready(stack, READY_TIMEOUT, &mut hop) {
                Boot::Ready => true,
                Boot::Booting => false,
                // Failed is never "carry on": the next hop would meet a database
                // this one didn't migrate.
                Boot::Failed(why) if step.is_target => {
                    return Boot::Failed(why).ready().map(Updated::from_ready)
                }
                Boot::Failed(why) => {
                    return Err(boot::BootFailed(format!(
                        "Step {} of {total} ({label}) failed, so the rest of the update was not \
                         attempted. {why} Once that is fixed, run the update again: it \
                         continues from {label}.",
                        i + 1
                    ))
                    .into())
                }
            };

            if !ready {
                // Deliberately an error, not a `false`: on a multi-hop route "still
                // booting" and "this step refused or rolled back" look identical from
                // out here, and the difference decides whether the operator should
                // wait or read the logs. Only the LAST hop can honestly report
                // still-booting, and it returns below.
                if step.is_target {
                    return Ok(Updated::Booting);
                }
                bail!(
                    "step {} of {total} ({label}) didn't finish coming up, so the rest of \
                     the upgrade was not attempted. Your site is on {label} — its own \
                     migration either succeeded or rolled itself back, so it is not \
                     half-upgraded. Check `atelier app logs` for what happened, then run \
                     `atelier app update` again (Continue update in the Manager): it \
                     continues from {label} to {}.",
                    i + 1,
                    plan.target_image
                );
            }
        }
        stack.clear_update_target();
        let ready = move_db_image(stack, r)?;
        Ok(Updated::from_ready(ready))
    })();
    let done = keep_logs(stack, &touched, done, r);
    settle_woken(stack, woke, done, r)
}

/// A failed update that had touched the containers saves every service's
/// log first ([`crate::update_logs`]), before [`settle_woken`] (or the owner's
/// next click) can restart them, and its error names the file.
fn keep_logs(
    stack: &Stack,
    touched: &std::cell::Cell<bool>,
    done: Result<Updated>,
    r: &mut dyn Reporter,
) -> Result<Updated> {
    match done {
        Err(e) if touched.get() => Err(crate::update_logs::save_after_failure(stack, e, r)),
        other => other,
    }
}

/// What an update found to do once the download was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateNeed {
    /// The site runs the download, with the settings it has now.
    Nothing,
    /// The site runs the download, but its containers were made from other
    /// settings: `up` on the same build, no backup.
    Settings,
    /// A new build, or a database move: the full update.
    Update,
}

/// What the owner reads when an update only applied changed settings.
pub const SETTINGS_APPLIED: &str = "Already up to date. Applied your changed settings.";

/// [`UpdateNeed`]: an update unless the image just pulled is the one the site's
/// `app` container was made from and no database move is due (unknown on
/// either side is an update, the way it always was). On the same build,
/// `settings_changed` ([`settings_changed`]) decides; when that couldn't be
/// read, whether this run rewrote `compose.yaml`/`.env` does.
fn update_need(
    running: Option<&str>,
    pulled: Option<&str>,
    settings_changed: Option<bool>,
    files_changed: bool,
    db_move_pending: bool,
) -> UpdateNeed {
    let same = matches!((running, pulled), (Some(a), Some(b)) if a == b);
    if !same || db_move_pending {
        UpdateNeed::Update
    } else if settings_changed.unwrap_or(files_changed) {
        UpdateNeed::Settings
    } else {
        UpdateNeed::Nothing
    }
}

/// `service hash` lines (`docker compose config --hash '*'`, or the
/// [`RUNNING_HASHES`] inspect) as a map.
fn service_hashes(out: &str) -> std::collections::HashMap<&str, &str> {
    out.lines()
        .filter_map(|l| l.trim().split_once(' '))
        .map(|(s, h)| (s.trim(), h.trim()))
        .filter(|(s, _)| !s.is_empty())
        .collect()
}

/// Whether any service's settings differ from the container it runs as: the
/// container's `com.docker.compose.config-hash` label (what compose itself
/// compares to decide on a recreate) against `docker compose config --hash
/// '*'` now. A declared service with no container counts as changed. `None`
/// when the declared hashes couldn't be read.
fn settings_changed(declared: &str, running: &str) -> Option<bool> {
    let declared = service_hashes(declared);
    if declared.is_empty() {
        return None;
    }
    let running = service_hashes(running);
    Some(
        declared
            .iter()
            .any(|(service, hash)| running.get(service) != Some(hash)),
    )
}

/// Whether the site's containers were made from other settings than its
/// `.env`/`compose.yaml` hold now ([`settings_changed`]): an update with no
/// newer version still has something to apply. `false` when it can't be told.
pub fn settings_pending(stack: &Stack) -> bool {
    settings_changed_now(stack) == Some(true)
}

/// `docker inspect` format: each container's service and config hash.
const RUNNING_HASHES: &str = "{{index .Config.Labels \"com.docker.compose.service\"}} \
     {{index .Config.Labels \"com.docker.compose.config-hash\"}}";

/// [`settings_changed`] for `stack`, asking compose and Docker; `None` when
/// either didn't answer.
fn settings_changed_now(stack: &Stack) -> Option<bool> {
    let mut declared = compose(stack);
    declared.args(["config", "--hash", "*"]);
    let declared = try_capture(declared)?;
    let mut ids = compose(stack);
    ids.args(["ps", "-aq"]);
    let ids = docker::try_capture_empty_ok(ids)?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    let running = if ids.is_empty() {
        String::new()
    } else {
        let mut c = docker::docker();
        c.args(["inspect", "-f", RUNNING_HASHES]).args(&ids);
        try_capture(c)?
    };
    settings_changed(&declared, &running)
}

/// The image ID (`sha256:…`) `image` names on this computer, if it is here.
fn image_id(image: &str) -> Option<String> {
    let mut c = docker::docker();
    c.args(["image", "inspect", "--format", "{{.Id}}", image]);
    let id = try_capture(c)?.trim().to_string();
    id.starts_with("sha256:").then_some(id)
}

/// The two files an `up` reads, as they stand: compared before and after the
/// pre-update repairs to tell whether the containers need recreating.
fn stack_files(stack: &Stack) -> (Option<String>, Option<String>) {
    (
        std::fs::read_to_string(stack.compose_path()).ok(),
        std::fs::read_to_string(stack.env_path()).ok(),
    )
}

/// The refusal for a locked engine site (the engine's `locked` state): an
/// update or a repair must not start it while its owner has it locked down.
pub(crate) const SITE_LOCKED: &str = "This site is locked. Unlock it first (System → \
     Experimental features, or `atelier site unlock --experimental`).";

/// Refuse when the engine says the site is locked.
pub(crate) fn refuse_locked(engine_state: Option<&str>) -> Result<()> {
    if engine_state == Some("locked") {
        bail!(SITE_LOCKED);
    }
    Ok(())
}

/// How a stopped site was brought up for its before-update backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woke {
    /// It was running already.
    No,
    /// A stopped site, started; it stays up after the update.
    Started,
    /// A resting engine site, woken through the engine; it rests again after.
    FromRest,
}

/// Whether a site has to be started to be backed up before an update: only
/// when every container is there and stopped. A missing one makes Start an
/// `up`, which would already run the new image, so that site goes without a
/// backup, as it did before 0.17.0.
fn start_for_backup_needed(state: ContainerState) -> bool {
    state == ContainerState::Stopped
}

/// A stopped site has no backup to take, so start it first (a resting engine
/// site through its Wake). If it won't start, refuse with
/// [`BackupBeforeUpdateFailed`]: an update with no way back is worse than no
/// update, unless the owner said to go without one (`opts.no_backup`). Then a
/// stopped site is left for the update's own `up` to start, and a resting one
/// still counts as woken, so it rests again after.
fn wake_for_update(
    stack: &Stack,
    engine_state: Option<&str>,
    opts: &UpdateOptions,
    r: &mut dyn Reporter,
) -> Result<Woke> {
    if opts.no_backup {
        r.log("Updating without a backup first (--no-backup).");
        return Ok(woke_without_start(observe_container(stack), engine_state));
    }
    start_for_backup(stack, engine_state, r)
}

/// What an update that doesn't start the site first ([`UpdateOptions::no_backup`])
/// counts as having woken: its `up` starts a stopped site, and a resting one
/// must go back to rest after, as it would had it been woken for a backup.
fn woke_without_start(state: ContainerState, engine_state: Option<&str>) -> Woke {
    if !start_for_backup_needed(state) {
        Woke::No
    } else if engine_state == Some("resting") {
        Woke::FromRest
    } else {
        Woke::Started
    }
}

fn start_for_backup(
    stack: &Stack,
    engine_state: Option<&str>,
    r: &mut dyn Reporter,
) -> Result<Woke> {
    if !start_for_backup_needed(observe_container(stack)) {
        return Ok(Woke::No);
    }
    // A stopped site is started only to be backed up: when the backup can't
    // be saved, refuse before starting it.
    backup_or_refuse(backups_writable(stack).map_err(anyhow::Error::new))?;
    let resting = engine_state == Some("resting");
    r.stage(
        Stage::Starting,
        if resting {
            "Waking the site to back it up before the update…"
        } else {
            "Starting the site to back it up before the update…"
        },
        None,
    );
    // Start's own steps report inside this one; its plan is dropped.
    let mut quiet = ScaledReporter {
        inner: r,
        base: 0.0,
        span: 0.0,
        prefix: String::new(),
    };
    let started = start(stack, &mut quiet).context(BackupBeforeUpdateFailed {
        cause: "The site is stopped and couldn't be started to back it up first".into(),
    })?;
    if !started {
        return Err(anyhow::Error::new(BackupBeforeUpdateFailed {
            cause: "The site is stopped, and it didn't finish starting in time to back it up \
                    first"
                .into(),
        }));
    }
    Ok(if resting {
        Woke::FromRest
    } else {
        Woke::Started
    })
}

/// What happens to a site the update woke, once the update has come out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterWake {
    /// It wasn't resting before: leave it as it is.
    Leave,
    /// Put it back to rest.
    Rest,
    /// It is still booting: resting it now would cut its converge short.
    StayAwake,
}

/// [`AfterWake`] for a site `woke` describes and how the update came out
/// (`Ok(ready)`, or failed). A failed update rests the site again too (review
/// 2026-11-14: it stayed awake); a site that wasn't resting is never rested.
fn after_wake(woke: Woke, outcome: Result<bool, ()>) -> AfterWake {
    match (woke, outcome) {
        (Woke::No | Woke::Started, _) => AfterWake::Leave,
        (Woke::FromRest, Ok(false)) => AfterWake::StayAwake,
        (Woke::FromRest, Ok(true) | Err(())) => AfterWake::Rest,
    }
}

/// Hand back the update's result `done`, after putting a site it woke from rest
/// back to rest ([`after_wake`]), whether the update worked or not. Best effort:
/// a site left awake is still a working site.
fn settle_woken(
    stack: &Stack,
    woke: Woke,
    done: Result<Updated>,
    r: &mut dyn Reporter,
) -> Result<Updated> {
    match after_wake(woke, done.as_ref().map(|u| u.ready()).map_err(|_| ())) {
        AfterWake::Leave => {}
        AfterWake::StayAwake => {
            r.log("The site stays awake until it has finished booting; Stop puts it back to rest.")
        }
        AfterWake::Rest => rest_now(stack, r),
    }
    done
}

/// Put an engine site to rest through its engine.
fn rest_now(stack: &Stack, r: &mut dyn Reporter) {
    let Some(info) = ready_engine(stack) else {
        r.log("Couldn't reach the engine to put the site back to rest; Stop does it.");
        return;
    };
    r.stage(Stage::Working, "Putting the site back to rest…", None);
    match engine_verb(&info, AdminVerb::Rest, "rest") {
        Ok(said) => say(r, &said),
        Err(e) => r.log(&format!(
            "Couldn't put the site back to rest ({e:#}); it's awake. Stop puts it to rest."
        )),
    }
}

/// Where the appliance image keeps its converge script.
const CONVERGE_PATH: &str = "/usr/local/bin/converge.sh";
/// The converge step that rebuilds indexes when the db's C library changed.
const COLLATION_REFRESH_STEP: &str = "refresh_collation_versions";

/// Whether the running `app` image ships the converge self-heal script.
fn has_converge(stack: &Stack) -> bool {
    let mut c = compose(stack);
    c.args(["exec", "-T", "app", "test", "-x", CONVERGE_PATH]);
    docker::try_capture_empty_ok(c).is_some()
}

/// Run after a restore reloads a database. A snapshot dumped from an *older*
/// image carries a stale schema, stale config and a stale library-parse cache,
/// so a bare `cache:rebuild` leaves the console blank — a broken `.libraries.yml`
/// parse drops `type: module` (found reshooting the docs, 2026-11-03). Re-run the
/// appliance's own `converge.sh`: it snapshots, runs `updatedb`, imports config
/// and truncates `cache_file_parsing` behind a health gate — exactly as a boot or
/// an update does, and the same script `doctor` shells to repair a site. Restoring
/// an older snapshot onto a newer image is the forward direction, which converge
/// migrates; its own per-run snapshot is the rollback. Streamed because it is slow
/// and chatty. Images too old to ship the script skip it: their restore target
/// matches their dump, so the cache rebuild the restore already did is enough.
///
/// The caller checks [`has_converge`] and reports the step (it's a step of the
/// restore's plan).
fn converge_restored(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    run_step(converge_cmd(stack), "migrate the restored site", r)
}

/// `compose exec -T app /usr/local/bin/converge.sh` — the appliance's own
/// self-heal, the same script `doctor` runs to repair a site.
fn converge_cmd(stack: &Stack) -> Command {
    let mut c = compose(stack);
    c.args(["exec", "-T", "app", CONVERGE_PATH]);
    c
}

/// Move the `db` service onto [`crate::stack::DB_IMAGE`] once the site runs an app
/// image whose converge rebuilds indexes on a collation change (cms #86). Runs
/// after an update has come up, so it asks the container actually running rather
/// than a version number — an `:edge` build has none worth comparing. Recreates
/// `db` on the new image and `app` with it, so converge runs against the new
/// glibc and reindexes before the site serves. Returns whether the console came
/// back up. Nothing to do (or an app too old to reindex, or not running) is
/// `Ok(true)`: the move waits for the next update.
///
/// The new image is downloaded BEFORE `compose.yaml` names it: a failed
/// download then leaves the file as it was, so the site still starts on the
/// old one and the move is tried again on the next update.
fn move_db_image(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    if stack.pending_db_image_migration().is_none() || !app_rebuilds_indexes(stack) {
        return Ok(true);
    }
    let Some((from, to)) = pull_then_move_db(stack, pull_db_image, r)? else {
        return Ok(true);
    };
    r.stage(
        Stage::Scaffold,
        &format!(
            "Moving your site's database server to {to}. It was on {from}, whose older \
             system libraries carry security advisories. The data stays; its indexes are \
             rebuilt once for the new libraries, which takes a moment longer than a \
             restart."
        ),
        Some(0.9),
    );
    // Only ever inside an update, after its gate ([`Reattach::GatedAway`]).
    run_recovering_with(
        stack,
        // scenario: update_moves_the_db_image_once_it_downloads_and_keeps_the_data
        &["up", "-d", "--force-recreate", "db", "app"],
        "move the database server",
        Reattach::GatedAway,
        r,
    )?;
    wait_until_ready(stack, READY_TIMEOUT, r).ready()
}

/// Download the pending database image with `pull_image`, and only then
/// rewrite `compose.yaml` to it. `Ok(None)` when nothing is pending, or when
/// the download failed (logged as a warning): the file is untouched, so the
/// next update tries again.
fn pull_then_move_db(
    stack: &Stack,
    pull_image: impl FnOnce(&str, &mut dyn Reporter) -> Result<()>,
    r: &mut dyn Reporter,
) -> Result<Option<(String, String)>> {
    let Some((_, to)) = stack.pending_db_image_migration() else {
        return Ok(None);
    };
    if let Err(e) = pull_image(&to, r) {
        r.log(&format!(
            "Warning: couldn't download the database server {to} ({e:#}). Your site keeps \
             its current one; the move is tried again on the next update."
        ));
        return Ok(None);
    }
    stack.migrate_db_image()
}

/// `docker pull <image>` for the database move, relayed like the app pull.
fn pull_db_image(image: &str, r: &mut dyn Reporter) -> Result<()> {
    let mut c = docker::docker();
    // scenario: update_moves_the_db_image_once_it_downloads_and_keeps_the_data
    c.args(["pull", image]);
    run_step(c, "download the database server", r)
}

/// Whether the running `app` container's converge carries the collation step.
fn app_rebuilds_indexes(stack: &Stack) -> bool {
    let mut c = compose(stack);
    c.args([
        "exec",
        "-T",
        "app",
        "grep",
        "-q",
        COLLATION_REFRESH_STEP,
        CONVERGE_PATH,
    ]);
    docker::try_capture_empty_ok(c).is_some()
}

/// Wraps a [`Reporter`] so a sub-run's 0.0–1.0 progress lands inside one slice of
/// the outer bar, and its headlines say which step they belong to.
struct ScaledReporter<'a> {
    inner: &'a mut dyn Reporter,
    base: f32,
    span: f32,
    prefix: String,
}

impl Reporter for ScaledReporter<'_> {
    /// A sub-run's plan never replaces the outer one: its progress is drawn
    /// inside the outer step it runs in.
    fn plan(&mut self, _steps: &[PlanStep]) {}

    fn stage(&mut self, stage: Stage, message: &str, fraction: Option<f32>) {
        self.inner.stage(
            stage,
            &format!("{}{message}", self.prefix),
            // A zero span means "no slice of the outer bar": indeterminate.
            fraction.and_then(|f| (self.span > 0.0).then_some(self.base + f * self.span)),
        );
    }

    fn log(&mut self, line: &str) {
        self.inner.log(line);
    }

    fn pull_progress(&mut self, ev: &docker::PullEvent) {
        self.inner.pull_progress(ev);
    }

    fn captures_output(&self) -> bool {
        self.inner.captures_output()
    }

    fn confirm(&mut self, question: &str) -> bool {
        self.inner.confirm(question)
    }

    fn confirm_docker_restart(&mut self, runtime: &str, detail: &str) -> bool {
        self.inner.confirm_docker_restart(runtime, detail)
    }
}

/// Run the one-time repairs an install made under an older manager needs — the
/// retired-repository rename, the move off an unmaintained or full nginx for `edge`, and the
/// legacy-`:edge` → stable move — and say so out loud.
///
/// None of them can be reached by changing a default: the image names were baked
/// into `.env` and `compose.yaml` at install time and nothing re-reads them. So this
/// is the only path by which those installs are ever corrected, which is why it sits
/// on install and upgrade rather than behind a flag.
///
/// A channel change moves the install to a different image, so it must never be
/// silent — the operator has to be able to connect "I ran update" to "I'm on
/// releases now", and to reverse it. Reported through `stage` rather than `log`
/// because the CLI prints stages and drops log lines.
///
/// It can also be a step *backwards*: `:edge` is built from every merge, so it can
/// sit ahead of the newest release, and Drupal's `hook_update_N` only runs forward.
/// `converge.sh` snapshots the database and rolls back if the migrated site fails
/// its health check, but a rollback leaves the older image in place — so this takes
/// a full portable snapshot (database + files) first, while the *old* image is still
/// the one running. Best effort: if the snapshot can't be taken the switch still
/// proceeds, since converge's own rollback is the real safety net.
/// Returns whether it saved a snapshot, so the update doesn't take a second one.
fn announce_legacy_migrations(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    let mut saved = false;
    // The repository rename first, and unconditionally: it is a dead pointer rather
    // than a preference (see `Stack::migrate_image_repo`), it needs no snapshot
    // because it changes no version, and the channel migration below reads its output
    // — a pre-rename `…/atelier:edge` only becomes the legacy default once renamed.
    if let Some((from, to)) = stack.migrate_image_repo()? {
        r.stage(
            Stage::Scaffold,
            &format!(
                "Repointing this install at {to}. It was set to {from}, the name Atelier \
                 was published under before the repository was renamed — that name no \
                 longer answers, which is why checking for updates had been failing.",
            ),
            Some(0.07),
        );
    }
    // Same kind of repair: a web server line nginx no longer fixes, or the full
    // image we stopped shipping, is nobody's choice, and it changes no Atelier
    // version, so no snapshot and no question.
    // The `pull` + `up` that follow fetch it and recreate `edge`.
    if let Some((from, to)) = stack.migrate_edge_image()? {
        r.stage(
            Stage::Scaffold,
            &format!(
                "Moving the web server in front of your site to {to}. It was on {from}, {}.",
                crate::stack::edge_nginx_upgrade_reason(&from),
            ),
            Some(0.075),
        );
    }
    if !stack.pending_default_channel_migration() {
        return Ok(saved);
    }
    // ASK FIRST. This is the one place the manager changes, on its own initiative,
    // which build somebody's existing site runs on — it fires on ADOPTION of an
    // install it did not create (`pending_default_channel_migration` requires
    // `exists()`), so the site it moves is by definition one that already has data
    // in it. On 2026-08-06 it moved an install six weeks forward in one step and
    // converge reinstalled over the database. Both of those holes are now closed —
    // the pre-pull guard refuses an image missing an installed module, and converge
    // refuses to install over a populated database — but neither makes the move
    // itself the manager's decision to take silently. Declining is free: the
    // install stays on the image it is already running.
    if !r.confirm(&format!(
        "This install still follows {LEGACY_DEFAULT_IMAGE} — the old default, which \
         is unreleased work off main rather than a release. Moving it to released \
         versions changes the image your site runs on, and may step it forward by \
         several versions. A full snapshot is taken first. Switch to released \
         versions now?"
    )) {
        r.stage(
            Stage::Scaffold,
            "Staying on the channel this install was already following. \
             `atelier app channel stable` moves it to released versions whenever you want.",
            Some(0.09),
        );
        return Ok(saved);
    }
    if status(stack).running {
        r.stage(
            Stage::Working,
            "Backing up before switching channels…",
            Some(0.09),
        );
        // The backup's own steps land inside this slice; its plan is dropped so it
        // never replaces the install/update plan already announced.
        let mut snap = ScaledReporter {
            inner: r,
            base: 0.09,
            span: 0.01,
            prefix: String::new(),
        };
        match backup(stack, Some("before-channel-switch"), &mut snap) {
            Ok(path) => {
                saved = true;
                r.stage(
                    Stage::Working,
                    &format!("Snapshot saved: {}", path.display()),
                    Some(0.1),
                )
            }
            Err(e) => r.stage(
                Stage::Working,
                &format!(
                    "Couldn't take a snapshot first ({e:#}) — continuing; the appliance \
                     rolls its own database back if the upgrade fails."
                ),
                Some(0.1),
            ),
        }
    }
    if let Some((from, to)) = stack.migrate_default_channel()? {
        r.stage(
            Stage::Scaffold,
            &format!(
                "Switching you to released versions ({to}). You were on {from}, which \
                 was the old default. To follow unreleased builds again: \
                 `atelier app channel edge`.",
            ),
            Some(0.11),
        );
    }
    Ok(saved)
}

/// Point the install at a different channel and, unless `apply` is false, pull it
/// and converge onto it straight away.
///
/// Returns the image now configured plus (when applied) whether the console came
/// back up before the readiness timeout. Without `apply` nothing has actually
/// changed yet in Docker's eyes — the new tag is only fetched on the next update.
pub fn switch_channel(
    stack: &Stack,
    channel: Channel,
    apply: bool,
    r: &mut dyn Reporter,
) -> Result<(String, Option<bool>)> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::ChannelSwitch)?;
    ensure_installed(stack)?;
    let image = stack.set_channel(channel)?;
    // Another stream of images: an unfinished route to the old one is moot.
    stack.clear_update_target();
    if !apply {
        return Ok((image, None));
    }
    preflight().require()?;
    pull(stack, r)?;
    up(stack, r)?;
    let ready = wait_until_ready(stack, READY_TIMEOUT, r).ready()?;
    Ok((image, Some(ready)))
}

/// `docker compose pull`, with a registry-login hint when the registry refused
/// the credentials (and only then: a network error or a missing tag is not a
/// login problem).
fn pull(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    guard_missing_extensions(stack, r)?;
    r.stage(
        Stage::Pull,
        "Pulling the latest appliance image…",
        Some(PULL_FROM),
    );
    // A capturing reporter (the GUI) gets structured per-layer progress via
    // compose's JSON progress stream; the CLI keeps inheriting docker's own TTY
    // bars, and a compose too old for `--progress json` falls back to the plain
    // text feed rather than failing on the flag.
    let structured = r.captures_output() && docker::compose_supports_json_progress();
    let mut c = compose(stack);
    if structured {
        c.args(["--progress", "json"]);
    }
    // scenario: update_applies_changed_settings_on_the_same_image_without_a_backup, update_of_a_running_site_whose_backup_fails_is_refused_then_proceeds_without_a_backup
    c.arg("pull");
    // What Docker said, when it passes through here (the terminal's own output
    // goes straight to the screen, where the operator reads it anyway).
    let mut refused = false;
    let run = if structured {
        run_streaming(c, "pull the image", |line| {
            refused |= registry_refused(line);
            match serde_json::from_str::<docker::PullEvent>(line) {
                Ok(ev) => r.pull_progress(&ev),
                Err(_) => r.log(line),
            }
        })
    } else if r.captures_output() {
        run_streaming(c, "pull the image", |line| {
            refused |= registry_refused(line);
            r.log(line);
        })
    } else {
        run_inherited(c, "pull the image")
    };
    // A pulled image is a new local digest, so the cached update check (if any)
    // now describes a machine that no longer exists — drop it rather than keep a
    // stale "update available" banner up after the update itself.
    if run.is_ok() {
        crate::update_cache::invalidate(stack);
    }
    run.map_err(|e| {
        if refused && stack.image().starts_with("ghcr.io/") {
            e.context(
                "If this is an authentication error, log in to the registry first:\n  \
                 echo \"<TOKEN>\" | docker login ghcr.io -u aincient-labs --password-stdin",
            )
        } else {
            e
        }
    })
}

/// Whether a line of Docker's pull output says the registry refused the
/// credentials: `denied` (GHCR's word for a package you can't read) or
/// `unauthorized`. Not the Docker socket's "permission denied", which is no
/// registry's doing.
fn registry_refused(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("unauthorized") || l.replace("permission denied", "").contains("denied")
}

// --- The pre-pull extension guard --------------------------------------------
//
// The host-side twin of converge's `check_installed_code_present()`, asked early
// enough to be advice instead of a refusal at boot. On 2026-08-06 an update
// carried a user onto a release that no longer shipped fourteen modules their
// site had installed; Drupal could not resolve their paths, could not boot, and
// converge reinstalled over the database (DECISIONS 0348/0353). Converge no
// longer does that — but the update should never have been pulled at all, and
// the fix for the user is on the OLD image, which is still running while this
// check happens.
//
// Sits inside `pull()` because that is the one chokepoint every path to a new
// image goes through: update, stepped upgrade hop, channel switch. On a fresh
// install nothing is running, so it stands aside.

/// Refuse to pull an image that is missing code the site has installed.
///
/// Every input is optional and every absence stands aside — this may only ever
/// turn a *known* problem into a refusal, never a gap in knowledge:
///
/// - the target's `dev.atelier.extensions` label (absent on images built before
///   it existed, and on anything that isn't in the registry),
/// - the same label on the image running now, which is what makes the comparison
///   core-safe: both lists are non-core, so `installed ∩ current` drops every core
///   module before the diff and no core module can look "missing",
/// - the site's own installed-extension list, unreadable if it isn't running.
fn guard_missing_extensions(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    // Cheapest and most often decisive first: a fresh install has no site to
    // protect, and this keeps the registry out of the install path entirely.
    let Some(installed) = site_extensions(stack) else {
        return Ok(());
    };
    let Some(target) = remote_probe(&stack.image()).ok().and_then(|p| p.extensions) else {
        return Ok(());
    };
    // From the RUNNING CONTAINER, not from `stack.image()`: by the time we get
    // here the configured image is already the target (a stepped hop calls
    // `set_image` before pulling), so asking the config would compare the target
    // against itself and find nothing missing, every time.
    let Some(current) = running_image_extensions(stack) else {
        return Ok(());
    };

    let mut missing: Vec<String> = installed
        .into_iter()
        .filter(|e| current.contains(e) && !target.contains(e))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    missing.dedup();

    r.log("Checked the new image against the modules this site has installed.");
    bail!(
        "The image you're updating to no longer contains {} your site has installed:\n  \
         {}\n\
         Drupal can't start without an installed module's code, so this update would \
         leave the site unable to boot. Nothing has been pulled and nothing has \
         changed.\n\n\
         Uninstall {} on the version you're running now — where the code still \
         exists — then update:\n  \
         atelier app open   (Extend → uninstall)\n\
         Take a backup first: `atelier data backup`.",
        if missing.len() == 1 {
            "a module"
        } else {
            "modules"
        },
        missing.join(", "),
        if missing.len() == 1 { "it" } else { "them" },
    )
}

/// The [`EXTENSIONS_LABEL`] of the image the `app` container is actually running,
/// read off the container so it survives `stack.set_image()` having already moved
/// on. A container inherits its image's labels, so this needs no image lookup.
/// `None` when the container isn't there or the label isn't (an older image).
fn running_image_extensions(stack: &Stack) -> Option<Vec<String>> {
    let mut ps = compose(stack);
    ps.args(["ps", "-q", "app"]);
    let cid = try_capture(ps)?;
    let cid = cid.lines().next()?.trim();
    if cid.is_empty() {
        return None;
    }
    let mut c = docker::docker();
    c.args([
        "inspect",
        cid,
        "--format",
        &format!("{{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{EXTENSIONS_LABEL}\"}}}}{{{{end}}}}"),
    ]);
    parse_extension_list(try_capture(c)?.trim())
}

/// The [`VERSION_LABEL`] of the image the `app` container was made from,
/// running or stopped, read off the container the same way (and for the same
/// reason) as [`running_image_extensions`]: it survives `stack.set_image()`
/// having moved the tag on. `None` when the container isn't there or the label
/// isn't (an image built before the stamp existed).
fn app_image_version(stack: &Stack) -> Option<String> {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "-q", "app"]);
    let cid = try_capture(ps)?;
    let cid = cid.lines().next()?.trim();
    if cid.is_empty() {
        return None;
    }
    let mut c = docker::docker();
    c.args([
        "inspect",
        cid,
        "--format",
        &format!(
            "{{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{VERSION_LABEL}\"}}}}{{{{end}}}}"
        ),
    ]);
    let v = try_capture(c)?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// The extensions the running site has INSTALLED, read the way converge reads
/// them: `core.extension` straight out of the config table, with no bootstrap.
/// `None` whenever the answer can't be had — not running, not Postgres, an empty
/// result — because "unknown" must never be mistaken for "nothing installed".
fn site_extensions(stack: &Stack) -> Option<Vec<String>> {
    if !status(stack).running {
        return None;
    }
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"]).args(DRUSH).args([
        "sql:query",
        "SELECT convert_from(data, 'UTF8') FROM config WHERE name = 'core.extension';",
    ]);
    // No probe deadline: this feeds a safety guard on the (user-initiated) update
    // path, and a slow drush timing out would silently skip the guard.
    let out = docker::probe_unbounded(c).ok().filter(|s| !s.is_empty())?;
    let names = parse_serialized_extensions(&out);
    (!names.is_empty()).then_some(names)
}

/// Pull extension names out of a serialized `core.extension` blob.
///
/// It serializes as `{module: {name: weight}, theme: {name: weight}, profile:
/// "name"}`, so every `s:<len>:"<name>";i:<weight>;` pair in it is an extension
/// and nothing else has that shape — the same rule `converge.sh` greps for.
/// Themes are included deliberately: they fatal an unbootable site exactly the
/// way modules do.
/// Every slice here goes through `str::get`, never `[a..b]`. The lengths come
/// from inside the blob — a database value — and a length that lands mid-UTF-8
/// would panic the manager on an indexing slice. `get` yields `None` instead, and
/// the parse stops or skips. Progress is guaranteed regardless: `i` advances past
/// the `s:` that started each iteration, so no input can loop forever.
fn parse_serialized_extensions(blob: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(found) = blob.get(i..).and_then(|rest| rest.find("s:")) {
        let start = i + found + 2;
        let Some(colon) = blob.get(start..).and_then(|rest| rest.find(':')) else {
            break;
        };
        i = start;
        let Ok(len) = blob
            .get(start..start + colon)
            .unwrap_or("")
            .parse::<usize>()
        else {
            continue;
        };
        let name_start = start + colon + 2; // past `:"`
        let name_end = name_start + len;
        let (Some(name), Some(rest)) = (blob.get(name_start..name_end), blob.get(name_end..))
        else {
            continue;
        };
        // Only a name followed by an integer WEIGHT is an extension entry. The
        // profile is serialized as `s:7:"profile";s:7:"minimal";` — a string pair,
        // so neither half may be mistaken for a module.
        if !rest.starts_with("\";i:") {
            continue;
        }
        if !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            out.push(name.to_string());
        }
        i = name_end;
    }
    out.sort();
    out.dedup();
    out
}

/// `docker compose up -d`, refused when the site's database was erased
/// ([`refuse_erased_database`]).
fn up(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    refuse_erased_database(stack)?;
    up_unguarded(stack, r)
}

/// [`up`] inside an update, after [`crate::gate`] said the site was ready: it
/// doesn't reconnect a detached container first ([`Reattach::GatedAway`]).
/// The erased-database refusal stays: a refusal changes nothing, and it is
/// the last word before `up` would create an empty database.
fn up_updating(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    refuse_erased_database(stack)?;
    r.stage(Stage::Starting, "Starting the containers…", Some(UP_FROM));
    run_recovering_with(
        stack,
        // scenario: update_applies_changed_settings_on_the_same_image_without_a_backup, update_of_a_running_site_whose_backup_fails_is_refused_then_proceeds_without_a_backup
        &["up", "-d"],
        "start the appliance",
        Reattach::GatedAway,
        r,
    )?;
    clear_database_wiped(stack);
    Ok(())
}

/// `docker compose up -d`, for the paths that may start an empty database: an
/// install, and a restore into a site whose database was erased.
fn up_unguarded(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    r.stage(Stage::Starting, "Starting the containers…", Some(UP_FROM));
    // scenario: start_and_doctor_refuse_an_erased_database_and_a_restore_brings_it_back
    run_recovering(stack, &["up", "-d"], "start the appliance", r)?;
    // The database exists again, so an owner's wipe has been acted on.
    clear_database_wiped(stack);
    Ok(())
}

/// Drop the owner's-wipe marker ([`crate::stack::DATABASE_WIPED_FILE`]) once
/// the database is back, so a later accidental loss is refused again.
fn clear_database_wiped(stack: &Stack) {
    // Best effort: a marker left behind only lets one more fresh start through.
    // scenario: start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks, checkup_names_a_detached_db_and_start_reconnects_it
    let _ = std::fs::remove_file(stack.database_wiped_path());
}

/// A start was refused: this site has been backed up before (it has a
/// `site-id`), yet Docker no longer has its database volume. `compose up` would
/// create an empty one and the appliance would install a blank site over the
/// lost one, so nothing starts until the owner restores a backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseErased {
    /// The newest backup of this site on this computer, if any.
    pub latest_backup: Option<Backup>,
}

impl std::fmt::Display for DatabaseErased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.latest_backup {
            Some(b) => write!(
                f,
                "Your site's database was removed from Docker. Restore your latest backup \
                 (Backups tab): {}. On the command line: `atelier data restore {}`.",
                b.name, b.path
            ),
            None => f.write_str(
                "Your site's database was removed from Docker, and there is no backup of this \
                 site on this computer. Restore a backup you saved elsewhere (Backups tab), \
                 or `atelier data restore <file>`.",
            ),
        }
    }
}

impl std::error::Error for DatabaseErased {}

/// Whether to refuse a start: `site_id` is set and Docker answered that the
/// database volume is gone. A site never backed up (a first install), an
/// unanswered probe, and a database the owner wiped on purpose (`wiped`, see
/// [`crate::stack::DATABASE_WIPED_FILE`]) are let through.
fn database_erased(site_id: Option<&str>, db_volume: Option<bool>, wiped: bool) -> bool {
    site_id.is_some() && db_volume == Some(false) && !wiped
}

/// [`database_erased`] for `stack`, asking Docker. A database volume that
/// exists again also clears a leftover owner's-wipe marker.
pub(crate) fn stack_database_erased(stack: &Stack) -> bool {
    let volume = docker::db_volume_exists(stack);
    if volume == Some(true) {
        clear_database_wiped(stack);
    }
    database_erased(stack.site_id().as_deref(), volume, stack.database_wiped())
}

/// Refuse with [`DatabaseErased`] when this site's database volume is gone.
pub(crate) fn refuse_erased_database(stack: &Stack) -> Result<()> {
    if stack_database_erased(stack) {
        return Err(DatabaseErased {
            latest_backup: list_backups(stack).into_iter().next(),
        }
        .into());
    }
    Ok(())
}

/// What the owner reads when a start fails even after clearing Docker's
/// leftovers: the one step they can always take, and that their data is safe.
/// Docker's own lines follow in brackets for a bug report.
const START_STUCK: &str = "Docker couldn't start your site, even after clearing what it had left \
     behind and trying again. Restart Docker (`atelier docker restart`), then try again. Your \
     site's data is safe.";

/// [`START_STUCK`] with what Docker said each time, as the error the app
/// offers its Restart button for.
fn start_stuck(first: &anyhow::Error, then: &anyhow::Error) -> anyhow::Error {
    crate::docker_runtime::NeedsDockerRestart(format!("{START_STUCK} ({first:#}; then {then:#})"))
        .into()
}

/// Run a compose start step (`up …`, `start`), and recover once from Docker's
/// own leftovers.
///
/// Docker Desktop sometimes keeps a stopped container's network endpoint after
/// a sleep, restart or crash. The container then can't start again ("endpoint
/// with name atelier-db-1 already exists in network atelier_default") and
/// compose reports only "dependency failed to start: container atelier-db-1
/// exited (1)" — an update stopped there on 2026-11-14. Only that exact state
/// is recovered: a container of the site (any service) that isn't running but
/// still holds an endpoint. Its endpoint is force-disconnected (what fixed it by hand; no
/// container, volume or data is touched) and the same step runs again. Any
/// other failure keeps its own error.
pub(crate) fn run_recovering(
    stack: &Stack,
    args: &[&str],
    action: &str,
    r: &mut dyn Reporter,
) -> Result<()> {
    run_recovering_with(stack, args, action, Reattach::First, r)
}

/// Whether [`run_recovering_with`] reconnects a container that lost its
/// network before it runs the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reattach {
    /// Every start, repair and channel move: compose never reconnects one.
    First,
    /// An update's `up`: [`crate::gate`] refused the update while any container
    /// was detached, and nothing the update does before `up` detaches one, so
    /// it never repairs that mid-update (M2c).
    GatedAway,
}

fn run_recovering_with(
    stack: &Stack,
    args: &[&str],
    action: &str,
    reattach: Reattach,
    r: &mut dyn Reporter,
) -> Result<()> {
    ensure_engine_image(stack, r)?;
    if reattach == Reattach::First {
        reattach_detached(stack, r);
    }
    // Docker's stale endpoint (the 2026-11-14 incident) appears only when
    // the step itself fails to start a container, so no gate can see it
    // first: this recovery stays on every path, the update's included.
    let mut cmd = compose(stack);
    cmd.args(args);
    let first = match run_step(cmd, action, r) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let stale = stale_endpoints(stack);
    if stale.is_empty() {
        return Err(first);
    }
    clear_stale_endpoints(stack, &stale, r);
    let mut again = compose(stack);
    again.args(args);
    run_step(again, action, r).map_err(|e| start_stuck(&first, &e))
}

/// Force-disconnect each `(network, container)` leftover [`stale_endpoints`]
/// found, saying so, then connect each one back as a fresh endpoint under the
/// names it had there. No container, volume or data is touched.
fn clear_stale_endpoints(stack: &Stack, stale: &[(String, String)], r: &mut dyn Reporter) {
    r.stage(
        Stage::Starting,
        "Docker left part of your site half-started. Clearing that and starting again (your data stays)…",
        None,
    );
    // Read before the disconnect, which drops the network (and its aliases)
    // from a stopped container's own settings: it would start without it, and
    // stay that way. Every network counts, not only the one it was created on
    // (an engine site's edge is on two).
    let mut names: Vec<&str> = stale.iter().map(|(_, c)| c.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let restore = crate::boot::endpoints_to_restore(stale, &crate::boot::inspect(&names));
    for (network, container) in stale {
        let mut c = docker::docker();
        // scenario: start_clears_a_held_endpoint_keeps_every_network_and_then_asks_for_a_docker_restart
        c.args(["network", "disconnect", "-f", network, container]);
        match docker::probe_unbounded(c) {
            Ok(_) => r.log(&format!(
                "Cleared a leftover network entry: {container} in {network}"
            )),
            Err(e) => r.log(&format!("Couldn't clear {container} from {network}: {e}")),
        }
    }
    for back in &restore {
        let mut c = docker::docker();
        c.args(reconnect_args(back));
        match docker::probe_unbounded(c) {
            Ok(_) => r.log(&format!(
                "Reconnected {} to {}",
                back.container, back.network
            )),
            // Still connected (the disconnect failed): nothing lost.
            Err(e) => r.log(&format!(
                "Couldn't reconnect {} to {}: {e}",
                back.container, back.network
            )),
        }
    }
    // Anything else that lost a network it declares.
    reattach_detached(stack, r);
}

/// `docker network connect` for `back`, under each name it had there.
fn reconnect_args(back: &crate::boot::Reconnect) -> Vec<String> {
    // scenario: start_clears_a_held_endpoint_keeps_every_network_and_then_asks_for_a_docker_restart
    let mut args = vec!["network".to_string(), "connect".to_string()];
    for alias in &back.aliases {
        args.push("--alias".into());
        args.push(alias.clone());
    }
    args.push(back.network.clone());
    args.push(back.container.clone());
    args
}

/// Reconnect each of the site's containers that lost its network
/// ([`crate::boot::Detached`]), under its service name so the others reach it
/// again. No container, volume or data is touched. The ones reconnected.
pub(crate) fn reattach_detached(stack: &Stack, r: &mut dyn Reporter) -> Vec<crate::boot::Detached> {
    let Some(found) = crate::boot::detached_now(stack) else {
        return Vec::new();
    };
    let mut done = Vec::new();
    for d in found {
        let mut c = docker::docker();
        c.args(reattach_args(&d));
        match docker::probe_unbounded(c) {
            Ok(_) => {
                r.log(&format!("Reconnected {} to {}", d.container, d.network));
                done.push(d);
            }
            Err(e) => r.log(&format!(
                "Couldn't reconnect {} to {}: {e}",
                d.container, d.network
            )),
        }
    }
    done
}

/// `docker network connect` for `d`: by its service name, the alias compose
/// gave it, so `db` resolves again.
fn reattach_args(d: &crate::boot::Detached) -> Vec<String> {
    // scenario: start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks, checkup_names_a_detached_db_and_start_reconnects_it
    [
        "network",
        "connect",
        "--alias",
        &d.service,
        &d.network,
        &d.container,
    ]
    .iter()
    .map(|a| (*a).to_string())
    .collect()
}

/// [`reattach_detached`], and when something was reconnected while the app
/// runs, restart the app: its boot ran without what it needed (the database)
/// and stays failed until it boots again. Whether anything was reconnected.
pub(crate) fn reconnect_site(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    let fixed = reattach_detached(stack, r);
    if fixed.is_empty() {
        return Ok(false);
    }
    if is_running(stack) {
        r.stage(
            Stage::Starting,
            "Part of your site had lost its network connection. Reconnected it; restarting the site (your data stays)…",
            None,
        );
        let mut c = compose(stack);
        // scenario: start_reconnects_a_running_db_and_restarts_the_app_that_lost_it
        c.args(["restart", "app"]);
        run_step(c, "restart the site", r)?;
    }
    Ok(true)
}

/// The steps of [`wake_recovering`], so its order is tested against a fake.
/// [`LocalWake`] is the real one.
trait WakeSteps {
    /// The engine's Wake (`admin wake`: `compose start db app`); its answer.
    fn wake(&mut self) -> Result<String>;
    /// Docker's leftover endpoints ([`stale_endpoints`]).
    fn stale_endpoints(&mut self) -> Vec<(String, String)>;
    /// Force-disconnect them ([`clear_stale_endpoints`]).
    fn clear(&mut self, stale: &[(String, String)], r: &mut dyn Reporter);
    /// The containers as `compose ps -a` sees them ([`observe_container`]).
    fn containers(&mut self) -> ContainerState;
    /// Create the missing containers, recovering once ([`up_missing`]: never
    /// recreates an existing one).
    fn up(&mut self, r: &mut dyn Reporter) -> Result<()>;
}

/// The engine's Wake with the same recovery a plain start gets (M6):
///
/// - Refused while locked down: nothing is recovered, the refusal stands.
/// - Docker kept a stopped container's endpoint: clear it, Wake once more.
/// - A container is gone (`compose start`, the engine's Wake, can't create
///   one): `compose up -d --no-recreate` creates it (and only it), then Wake
///   once more. The failed Wake
///   put the engine's resting mark back on, so edge would keep serving the
///   resting copy over a running site; the Wake now (the containers exist)
///   takes the mark off, and the engine's state is the site's again.
///
/// Any other failure keeps the engine's own error.
fn wake_recovering(steps: &mut dyn WakeSteps, r: &mut dyn Reporter) -> Result<()> {
    let first = match steps.wake() {
        Ok(said) => {
            say(r, &said);
            return Ok(());
        }
        Err(e) => e,
    };
    if format!("{first:#}").contains("locked down") {
        return Err(first);
    }
    let stale = steps.stale_endpoints();
    let mut last = None;
    if !stale.is_empty() {
        steps.clear(&stale, r);
        match steps.wake() {
            Ok(said) => {
                say(r, &said);
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    if steps.containers() == ContainerState::Missing {
        r.stage(
            Stage::Starting,
            "Part of your site's containers are gone. Creating them again (your data stays)…",
            None,
        );
        steps.up(r)?;
        let said = steps
            .wake()
            .context("the site's containers were created again, but the engine couldn't wake it")?;
        say(r, &said);
        return Ok(());
    }
    match last {
        Some(e) => Err(start_stuck(&first, &e)),
        None => Err(first),
    }
}

/// [`WakeSteps`] for a site on this computer.
struct LocalWake<'a> {
    stack: &'a Stack,
    info: &'a crate::engine_admin::EngineInfo,
}

impl WakeSteps for LocalWake<'_> {
    fn wake(&mut self) -> Result<String> {
        engine_verb(self.info, AdminVerb::Wake, "wake")
    }
    fn stale_endpoints(&mut self) -> Vec<(String, String)> {
        stale_endpoints(self.stack)
    }
    fn clear(&mut self, stale: &[(String, String)], r: &mut dyn Reporter) {
        clear_stale_endpoints(self.stack, stale, r);
    }
    fn containers(&mut self) -> ContainerState {
        observe_container(self.stack)
    }
    fn up(&mut self, r: &mut dyn Reporter) -> Result<()> {
        up_missing(self.stack, r)
    }
}

/// Read `docker image inspect`'s answer about the engine's image: there, or
/// "No such image" (gone: fetch it). Any other failure (Docker not answering, a
/// timeout) is an error: fetching on it would download for nothing and then
/// fail on the same Docker.
fn engine_image_present(inspect: std::result::Result<String, String>) -> Result<bool> {
    match inspect {
        Ok(_) => Ok(true),
        Err(e) if e.to_lowercase().contains("no such image") => Ok(false),
        Err(e) => bail!("Couldn't check your site's engine image with Docker ({e})"),
    }
}

/// What a start needs to do about the engine's pinned image (0487).
#[derive(Debug, PartialEq, Eq)]
enum EngineImage {
    /// No engine, or its image is on this computer.
    Ready,
    /// Pinned by digest but gone from this computer: fetch exactly that digest.
    Fetch(String),
    /// A local developer build (a bare image ID) that is gone: nothing to fetch.
    LocalBuildGone,
}

fn engine_image_need(pinned: Option<&str>, present: bool) -> EngineImage {
    match pinned {
        None => EngineImage::Ready,
        Some(_) if present => EngineImage::Ready,
        Some(p) if crate::engine::is_image_id(p) => EngineImage::LocalBuildGone,
        Some(p) => EngineImage::Fetch(p.to_string()),
    }
}

const LOCAL_ENGINE_GONE: &str = "This site's engine was a local build that is no longer on this \
     computer, so it can't start. Turn the engine off and on again (System → Experimental \
     features, or `atelier engine enable`).";

/// Before a start: make sure the engine's pinned image is on this computer.
///
/// `compose.engine.yaml` sets `pull_policy: never` so only `atelier engine
/// enable` decides what the engine runs. An image prune or a Docker reset can
/// still remove that image, and the start then dies on "No such image"
/// (owner's test, 2026-11-14). Fetching the pinned digest again brings back the
/// same bytes `enable` chose, so the rule holds. A local build can't be fetched:
/// say so before Docker does.
fn ensure_engine_image(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    if !crate::engine::is_enabled(stack) {
        return Ok(());
    }
    let pinned = crate::engine::pinned_image(stack);
    let present = match pinned.as_deref() {
        None => false,
        Some(p) => {
            let mut c = docker::docker();
            c.args(["image", "inspect", "--format", "{{.Id}}", p]);
            engine_image_present(docker::probe(c))?
        }
    };
    match engine_image_need(pinned.as_deref(), present) {
        EngineImage::Ready => {
            // Sites pinned before 0.17.0 have no local name yet: add it, so the
            // next prune can't take the image.
            if let Some(p) = pinned.as_deref() {
                crate::engine::protect_image(stack, p);
            }
            Ok(())
        }
        EngineImage::LocalBuildGone => bail!(LOCAL_ENGINE_GONE),
        EngineImage::Fetch(image) => {
            r.stage(
                Stage::Pull,
                "Downloading your site's engine again (it was removed from this computer)…",
                None,
            );
            let mut c = docker::docker();
            // scenario: none — engine sites only; engine mode (the experimental engine sidecar) has no scenario
            c.args(["pull", &image]);
            run_step(c, "download the site's engine", r).map_err(|e| {
                anyhow::anyhow!(
                    "Couldn't download your site's engine, which is no longer on this computer. \
                     If you're offline, connect and try again. ({e:#})"
                )
            })?;
            crate::engine::protect_image(stack, &image);
            Ok(())
        }
    }
}

/// `(network, container)` for each of this site's containers (any service:
/// db, app, edge, the engine) that isn't running yet still holds an endpoint in one of its networks — Docker's
/// leftover that [`run_recovering`] clears. Best effort: any failed probe
/// counts as "none found", so the original error stands.
fn stale_endpoints(stack: &Stack) -> Vec<(String, String)> {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "--format", "json"]);
    let Some(out) = docker::try_capture_empty_ok(ps) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for name in stopped_service_containers(&out) {
        let mut nets = docker::docker();
        nets.args([
            "inspect",
            "-f",
            "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}\n{{end}}",
            &name,
        ]);
        let Some(nets) = docker::try_capture(nets) else {
            continue;
        };
        for network in nets.lines().map(str::trim).filter(|n| !n.is_empty()) {
            let mut members = docker::docker();
            members.args([
                "network",
                "inspect",
                "-f",
                "{{range .Containers}}{{.Name}}\n{{end}}",
                network,
            ]);
            if docker::try_capture(members).is_some_and(|m| holds_endpoint(&m, &name)) {
                found.push((network.to_string(), name.clone()));
            }
        }
    }
    found
}

/// Names of the containers `compose ps -a` lists as not running, whatever
/// their service: Docker leaves an `edge` or engine endpoint behind as readily
/// as a `db` one.
fn stopped_service_containers(ps_json: &str) -> Vec<String> {
    parse_ps(ps_json)
        .into_iter()
        .filter(|e| !e.state.to_lowercase().contains("running") && !e.name.is_empty())
        .map(|e| e.name)
        .collect()
}

/// Whether `network inspect`'s endpoint names (one per line) include `name`.
fn holds_endpoint(members: &str, name: &str) -> bool {
    members.lines().any(|l| l.trim() == name)
}

/// Stop the site. With the engine up, Stop is its Rest (DECISIONS 0488):
/// freeze, then stop `app` and `db`, edge serving the snapshot; returns the
/// engine's answer. Without it, [`stop_containers`].
pub fn stop(stack: &Stack, r: &mut dyn Reporter) -> Result<Option<String>> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Stop)?;
    ensure_installed(stack)?;
    // Docker first: without it, the engine probe and `compose stop` fail with
    // Docker's own words instead of "Start Docker".
    preflight().require()?;
    if let Some(info) = ready_engine(stack) {
        r.stage(Stage::Working, "Putting the site to rest…", None);
        let said = engine_verb(&info, AdminVerb::Rest, "rest")?;
        say(r, &said);
        return Ok(Some(said));
    }
    stop_containers(stack, r)?;
    Ok(None)
}

/// `docker compose stop` — stop every container, the engine and edge too, and
/// keep data. For freeing the port (a site leaving the list), not the owner's
/// Stop ([`stop`]).
pub fn stop_containers(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Stop)?;
    ensure_installed(stack)?;
    r.stage(Stage::Working, "Stopping the appliance…", None);
    let mut c = compose(stack);
    // scenario: start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks, checkup_names_a_detached_db_and_start_reconnects_it
    c.arg("stop");
    run_step(c, "stop the appliance", r)
}

/// The engine's info when its container answers `docker exec`: the sign that
/// Start and Stop go through it (DECISIONS 0488).
fn ready_engine(stack: &Stack) -> Option<crate::engine_admin::EngineInfo> {
    crate::engine_admin::probe(stack).filter(|i| i.ready().is_some())
}

fn engine_verb(
    info: &crate::engine_admin::EngineInfo,
    verb: AdminVerb,
    what: &str,
) -> Result<String> {
    use crate::engine_admin::{EngineAdmin, LocalDocker};
    EngineAdmin::new(&LocalDocker, Some(info), what)?.run(verb)
}

/// The engine's answer, line by line, into the reporter's feed.
fn say(r: &mut dyn Reporter, said: &str) {
    for line in said.lines().filter(|l| !l.trim().is_empty()) {
        r.log(line);
    }
}

/// What was observed of the stack's containers before starting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    /// The container exists and is running.
    Running,
    /// The container exists but is stopped.
    Stopped,
    /// A `db`, `app` or `edge` container doesn't exist (e.g. `pack down` /
    /// `down` removed them, or one was removed by hand).
    Missing,
    /// The probe itself failed — nothing is known.
    Unknown,
}

/// Which compose verb brings the stack up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    /// `compose start` — revive existing containers.
    Start,
    /// `compose up -d --no-recreate` — create the missing containers and leave
    /// the existing ones as they are ([`start_args`]).
    Up,
}

/// The `compose` arguments for `action`. A Start never recreates a container:
/// sites share moving tags (`atelier-cms:latest`), so after another site's
/// update a plain `up` would recreate this site's app on the new image — an
/// update with no backup and no stepped route. Only an update does that.
pub fn start_args(action: StartAction) -> &'static [&'static str] {
    match action {
        // scenario: start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks, checkup_names_a_detached_db_and_start_reconnects_it
        StartAction::Start => &["start"],
        // scenario: start_recreates_only_the_removed_container_and_keeps_the_others, app_down_keeps_the_data_and_down_wipe_starts_a_fresh_site
        StartAction::Up => &["up", "-d", "--no-recreate"],
    }
}

/// What a Start does: the compose verb, and whether the existing `db`
/// container is handed the restart policy of a `compose.yaml` this start just
/// rewrote ([`refresh_compose`]) with `docker update`, since neither verb
/// recreates it to pick the new file up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartPlan {
    pub action: StartAction,
    pub set_db_restart: bool,
}

/// [`StartPlan`] from the containers as found and whether the compose file was
/// just rewritten. The rewrite never turns a start into an `up` (it once did,
/// and recreated containers on whatever image the shared tag named by then).
pub fn start_plan(state: ContainerState, compose_refreshed: bool) -> StartPlan {
    StartPlan {
        action: start_action(state),
        set_db_restart: compose_refreshed,
    }
}

/// The restart policy the current [`crate::stack::COMPOSE_TEMPLATE`] gives `db`.
const DB_RESTART: &str = "unless-stopped";

/// Give the site's existing `db` container [`DB_RESTART`] in place (`docker
/// update`): no restart, no recreate. Best effort: without it the database
/// only doesn't come back by itself after Docker restarts, as before.
fn apply_db_restart_policy(stack: &Stack, r: &mut dyn Reporter) {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "--format", "json"]);
    let Some(out) = docker::try_capture_empty_ok(ps) else {
        return;
    };
    let Some(db) = parse_ps(&out)
        .into_iter()
        .find(|e| e.service == "db" && !e.name.is_empty())
    else {
        return;
    };
    let mut c = docker::docker();
    // scenario: start_after_the_shared_tag_moved_keeps_the_sites_image
    c.args(["update", "--restart", DB_RESTART, &db.name]);
    match docker::probe(c) {
        Ok(_) => r.log(&format!(
            "The database now restarts with Docker ({} → {DB_RESTART}).",
            db.name
        )),
        Err(e) => r.log(&format!(
            "Couldn't set the database's restart policy ({e}); it applies at the next update."
        )),
    }
}

/// [`StartAction::Up`]: create the containers that are missing, refused when
/// the site's database was erased ([`refuse_erased_database`]).
fn up_missing(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    refuse_erased_database(stack)?;
    r.stage(Stage::Starting, "Starting the containers…", Some(UP_FROM));
    run_recovering(stack, start_args(StartAction::Up), "start the appliance", r)?;
    clear_database_wiped(stack);
    Ok(())
}

/// Pure decision: `compose start` only works on containers that exist (and
/// starts only those), so a removed container needs `up -d`. When the probe
/// failed, keep the old behaviour (`start`).
pub fn start_action(state: ContainerState) -> StartAction {
    match state {
        ContainerState::Missing => StartAction::Up,
        ContainerState::Running | ContainerState::Stopped | ContainerState::Unknown => {
            StartAction::Start
        }
    }
}

/// The services every site has; `compose start` can't bring back one whose
/// container is gone.
const CORE_SERVICES: [&str; 3] = ["db", "app", "edge"];

/// Classify `compose ps -a --format json` output: [`Missing`](ContainerState::Missing)
/// when any of [`CORE_SERVICES`] has no container (a `docker rm` of `db` alone
/// left `compose start` starting app and edge against no database), otherwise
/// by the `app` container's state.
fn container_state_from_ps(out: &str) -> ContainerState {
    let entries = parse_ps(out);
    if CORE_SERVICES
        .iter()
        .any(|s| !entries.iter().any(|e| e.service == *s))
    {
        return ContainerState::Missing;
    }
    match entries.iter().find(|e| e.service == "app") {
        Some(e) if e.state.to_lowercase().contains("running") => ContainerState::Running,
        _ => ContainerState::Stopped,
    }
}

fn observe_container(stack: &Stack) -> ContainerState {
    let mut c = compose(stack);
    c.args(["ps", "-a", "--format", "json"]);
    // An empty list means no containers (Missing → `up`), not a failed ask:
    // a site whose containers are gone must not get `compose start`.
    match docker::try_capture_empty_ok(c) {
        Some(out) => container_state_from_ps(&out),
        None => ContainerState::Unknown,
    }
}

/// Start the appliance: `docker compose start` for existing containers, or
/// `docker compose up -d` when none exist (after `pack down`). Returns whether
/// the console came up before the readiness timeout.
pub fn start(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Start)?;
    ensure_installed(stack)?;
    // Docker first, so a stopped Docker reads "Start Docker", not a probe's error.
    preflight().require()?;
    // Before anything starts a container: an erased database must not come
    // back empty (the engine's Wake starts db too).
    refuse_erased_database(stack)?;
    // Before the engine's Wake too: a resting engine site starts that way, and
    // its pinned image needs its local name as much as on a cold start.
    ensure_engine_image(stack, r)?;
    Plan::ranged(&[
        ("start", "Start", START_FROM, BOOT_FROM),
        ("boot", "Boot", BOOT_FROM, 1.0),
    ])
    .announce(r);
    // A container that lost its network (Docker's leftovers, an earlier
    // recovery) never gets it back from `compose start`/`up` or a Docker
    // restart: reconnect it first, and reboot an app that booted without it.
    reconnect_site(stack, r)?;
    // DECISIONS 0488: with the engine up, Start is its Wake (refused while
    // locked down), so the engine's state stays the site's state.
    if let Some(info) = ready_engine(stack) {
        r.stage(Stage::Starting, "Waking the site…", Some(START_FROM));
        wake_recovering(&mut LocalWake { stack, info: &info }, r).map_err(|e| {
            if format!("{e:#}").contains("locked down") {
                anyhow::anyhow!(
                    "the site is locked down, so it can't be started. Unlock it first \
                     (System → Experimental features, or `atelier site unlock --experimental`)."
                )
            } else {
                e
            }
        })?;
        return wait_until_ready(stack, START_READY_TIMEOUT, r).ready();
    }
    r.stage(Stage::Starting, "Starting the appliance…", Some(START_FROM));
    // An untouched older compose.yaml gets this manager's (the db restart
    // policy). The containers keep running what they were made from: the new
    // file reaches them at the next update, and the one setting it adds is
    // handed to `db` in place ([`start_plan`]).
    let plan = start_plan(observe_container(stack), refresh_compose(stack, r));
    match plan.action {
        StartAction::Start => {
            run_recovering(stack, start_args(plan.action), "start the appliance", r)?;
        }
        StartAction::Up => up_missing(stack, r)?,
    }
    if plan.set_db_restart {
        apply_db_restart_policy(stack, r);
    }
    wait_until_ready(stack, START_READY_TIMEOUT, r).ready()
}

/// `docker compose down`. With `wipe_data`, also removes the volumes — a
/// destructive reset (database, files, and the saved admin password). Callers
/// must confirm with the user first.
///
/// A wipe first leaves [`crate::stack::DATABASE_WIPED_FILE`], so the next
/// start sets up a fresh site rather than refusing with [`DatabaseErased`]: the
/// owner erased it, it wasn't lost. Written before `down -v`, so a wipe that
/// fails halfway still counts; a database that survives clears it again.
///
/// Runs through [`run_step`] like stop and start, so the GUI gets Docker's
/// output in its feed and a failure says why.
pub fn down(stack: &Stack, wipe_data: bool, r: &mut dyn Reporter) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Remove)?;
    ensure_installed(stack)?;
    let mut c = compose(stack);
    // scenario: app_down_keeps_the_data_and_down_wipe_starts_a_fresh_site
    c.arg("down");
    if wipe_data {
        mark_database_wiped(stack)?;
        c.arg("-v");
    }
    run_step(c, "stop and remove the appliance", r)
}

/// Leave the owner's-wipe marker in `stack`'s directory.
fn mark_database_wiped(stack: &Stack) -> Result<()> {
    let path = stack.database_wiped_path();
    // scenario: app_down_keeps_the_data_and_down_wipe_starts_a_fresh_site
    std::fs::write(
        &path,
        "The database was wiped on purpose; the next start sets up a fresh site.\n",
    )
    .with_context(|| format!("could not write {}", path.display()))
}

/// Wipe everything and install from scratch. Destructive — confirm first.
/// Returns whether the console came up before the readiness timeout.
pub fn reinstall(stack: &Stack, opts: &InstallOptions, r: &mut dyn Reporter) -> Result<bool> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Reinstall)?;
    preflight().require()?;
    if stack.exists() {
        lifecycle_plan().announce(r);
        r.stage(Stage::Scaffold, "Removing the old appliance…", Some(0.02));
        down(stack, true, r)?;
    }
    install(stack, opts, r)
}

/// The uploaded-files tree inside the `app` container — the `files:` volume from
/// `compose.yaml`, holding user uploads and generated image derivatives.
pub(crate) const FILES_DIR: &str = "/opt/drupal/web/sites/default/files";

/// Back up the whole appliance to a single portable `.tar.gz` snapshot on the
/// host: the database (`drush sql:dump --gzip`, converge's format) **plus** the
/// uploaded-files tree, alongside a `manifest.json`. Self-contained, so the
/// archive can be shared over the wire and [`restore`]d onto another host.
/// Returns the path to the created archive.
pub fn backup(stack: &Stack, label: Option<&str>, r: &mut dyn Reporter) -> Result<PathBuf> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Backup)?;
    let plan = Plan::weighted(&[
        ("check", "Check", 1.0),
        ("dump", "Database", 4.0),
        ("pack", "Files", 4.0),
        ("copy", "Save", 1.0),
    ]);
    plan.announce(r);
    let site_id = plan.run(r, "check", "Checking the site…", |_| {
        ensure_running(stack)?;
        // Before the dump: a folder the copy can't land in fails here in
        // plain words, not as Docker's "permission denied" after the dump.
        std::fs::create_dir_all(stack.backups_dir()).map_err(|_| BackupsNotWritable {
            dir: stack.backups_dir(),
        })?;
        backups_writable(stack)?;
        sweep_stale_parts(&stack.backups_dir());
        // Which site this is (m2-6, D-04): restore refuses another site's archive.
        stack.ensure_site_id()
    })?;

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let stem = backup_stem(label, &ts);
    // Per-run paths: two backups at once (the GUI and the CLI, or a stepped
    // upgrade's own snapshot) must never pack each other's staging or write the
    // same file. The copy lands in a hidden `.part` the backup list ignores; the
    // final name is only claimed once the archive is complete (see [`claim_name`]).
    let run = run_id();
    let stage = format!("/tmp/aincient-snapshot-{run}");
    let part = stack.backups_dir().join(format!(".{stem}-{run}.part"));
    let archive = format!("{stage}.tar.gz");

    let drush = DRUSH.join(" ");
    // A manifest identifying the archive as an AIncient snapshot and pinning the
    // image it was taken from — so a future restore can warn on a version skew.
    //
    // The DIGEST is the load-bearing field, not the reference. Until 2026-08-06
    // this recorded `stack.image()` alone, which on a moving tag (`:edge`,
    // `:latest`) names no build at all — and a moving tag is exactly where skew is
    // most likely. During the incident of that date, recovering the user's site
    // meant resolving their local digests against the registry by hand, because
    // their manifest said `…/atelier-cms:edge` and nothing more. A digest is
    // content-addressable: it identifies the one image this database is known to
    // run on, forever, and `install --image repo@sha256:…` restores onto it.
    //
    // Both extra fields are best-effort: an image that isn't inspectable (locally
    // built, or already gone) records what we do know rather than failing a backup
    // over provenance. Version 3 adds the site's identity (m2-6, D-04); the
    // label is free text, so the JSON is built by serde and quoted for the shell
    // by `backup_script`.
    let probe = site_probe(stack).unwrap_or_default();
    let manifest = serde_json::json!({
        "format": "aincient-snapshot",
        "version": 3,
        "created": ts,
        "image": stack.image(),
        "digest": probe.digest.clone().unwrap_or_default(),
        "atelier_version": probe.version.clone().unwrap_or_default(),
        "site_id": site_id,
        "site_slug": stack.slug.clone().unwrap_or_default(),
        "site_label": stack.label.clone().unwrap_or_default(),
    })
    .to_string();

    // Build the bundle inside the container, then copy it out (mirrors the
    // dump→cp→rm pattern). Dump and pack are separate execs so a failure names
    // the half that broke (DECISIONS 0515).
    plan.run(r, "dump", "Dumping the database…", |_| {
        let mut c = compose(stack);
        c.args([
            "exec",
            "-T",
            "app",
            "sh",
            "-c",
            &backup_dump_script(&drush, &manifest, &stage),
        ]);
        run_capture(c, "dump the database")
    })
    .inspect_err(|_| remove_in_container(stack, &[&stage, &archive]))?;
    plan.run(r, "pack", "Packing the uploaded files…", |_| {
        let mut c = compose(stack);
        c.args(["exec", "-T", "app", "sh", "-c", &backup_pack_script(&stage)]);
        run_capture(c, "pack the uploaded files")
    })
    .inspect_err(|_| remove_in_container(stack, &[&stage, &archive]))?;
    let host_path = plan
        .run(
            r,
            "copy",
            "Copying the backup out of the container…",
            |_| {
                let mut cp = compose(stack);
                cp.args(["cp", &format!("app:{archive}"), &part.to_string_lossy()]);
                // The docker CLI writes this copy on this computer: a full
                // disk here is the computer's, not Docker's.
                crate::docker::run_capture_writing_to(
                    cp,
                    "copy the snapshot out of the container",
                    &stack.backups_dir(),
                )?;
                // A backup that isn't on disk in full is no backup: never report one.
                let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                if size == 0 {
                    bail!("the backup copied out of the container came out empty");
                }
                claim_name(&part, &stack.backups_dir(), &stem, ".tar.gz")
            },
        )
        .inspect_err(|_| {
            remove_in_container(stack, &[&stage, &archive]);
            // Never leave a partial archive behind.
            // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without, update_of_a_running_site_whose_backup_fails_is_refused_then_proceeds_without_a_backup
            let _ = std::fs::remove_file(&part);
        })?;

    // Best-effort cleanup of the in-container temp files.
    remove_in_container(stack, &[&stage, &archive]);
    // The first automatic backup this version takes starts the count of the
    // ones kept ([`AUTOMATIC_SINCE_FILE`]); older ones are never pruned.
    let name = host_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Some(stamp) = automatic_backup_stamp(name) {
        note_automatic_backups_since(stack, stamp);
    }

    r.log(&format!("Snapshot written to {}", host_path.display()));
    plan.finish(r, "Backup saved.");
    Ok(host_path)
}

/// A backup refused before any work: Atelier can't write in the site's backups
/// folder (its permissions, a read-only or locked-up disk). Found by trying,
/// before the dump, rather than by the copy out failing with Docker's
/// "permission denied" once the dump is done (state zoo, `ro-backups`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupsNotWritable {
    /// The backups folder.
    pub dir: PathBuf,
}

impl BackupsNotWritable {
    /// The cause alone, for a sentence that goes on ([`backup_or_refuse`]).
    pub fn cause(&self) -> String {
        format!(
            "Atelier can't save backups in {}: it isn't writable",
            self.dir.display()
        )
    }
}

impl std::fmt::Display for BackupsNotWritable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}. Fix the folder's permissions (or free it up), then try again.",
            self.cause()
        )
    }
}

impl std::error::Error for BackupsNotWritable {}

/// Whether a backup could be saved in `stack`'s backups folder right now: a
/// file is created there (and removed) as the copy out would. A folder not
/// made yet is judged by the site's folder it would be made in; nothing is
/// created then (the checkup calls this, and a diagnosis changes nothing).
pub fn backups_writable(stack: &Stack) -> std::result::Result<(), BackupsNotWritable> {
    let dir = stack.backups_dir();
    let probe_in = if dir.is_dir() { &dir } else { &stack.home };
    let probe = probe_in.join(format!(".atelier-write-check-{}", std::process::id()));
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe);
    match made {
        Ok(_) => {
            // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        // Left by a run that died between the two lines: the folder took it.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(BackupsNotWritable { dir }),
    }
}

/// A backup's file name without its extension: `aincient-[<label>-]<ts>`, the
/// label sanitized. [`claim_name`] adds `-N` when the name is taken.
fn backup_stem(label: Option<&str>, ts: &str) -> String {
    match label {
        Some(l) if !l.is_empty() => format!("aincient-{}-{ts}", sanitize(l)),
        _ => format!("aincient-{ts}"),
    }
}

/// Best-effort removal of an op's temp paths inside the `app` container. Every
/// failure path calls it: per-run names never overwrite each other, so a
/// leftover would otherwise stay (a multi-GB archive, once per failed attempt).
fn remove_in_container(stack: &Stack, paths: &[&str]) {
    let mut rm = compose(stack);
    // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without, a_restore_keeps_the_three_newest_counted_backups_and_never_prunes_older_ones
    rm.args(["exec", "-T", "app", "rm", "-rf"]).args(paths);
    let _ = rm.output();
}

/// A token unique to one run of an op, for its temp paths inside the container.
fn run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

/// A `.part` nobody has written to for this long is an interrupted copy (the app
/// quit or crashed mid-backup), never one in flight: `compose cp` keeps writing.
const STALE_PART: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Remove interrupted backup copies: hidden `.aincient-*.part` files the backup
/// list never shows, each as large as an archive. Best effort.
fn sweep_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(".aincient-") && name.ends_with(".part")) {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_PART);
        if stale {
            // scenario: none — removes .part files older than the stale age; no scenario leaves one
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Move the finished `part` to `dir/stem.ext`, or `dir/stem-2.ext`, `-3`, … —
/// the first name nobody holds. The claim is atomic (a hard link fails when the
/// name exists), so two backups finishing in the same second never overwrite
/// each other: checking `exists()` first and writing later raced (both picked
/// the same name, the second archive replaced the first). A filesystem without
/// hard links falls back to check-then-rename.
fn claim_name(part: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    let candidates = std::iter::once(dir.join(format!("{stem}{ext}")))
        .chain((2u32..1000).map(|n| dir.join(format!("{stem}-{n}{ext}"))));
    for name in candidates {
        match std::fs::hard_link(part, &name) {
            Ok(()) => {
                // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without, a_restore_keeps_the_three_newest_counted_backups_and_never_prunes_older_ones
                let _ = std::fs::remove_file(part);
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) if !name.exists() => {
                // scenario: none — the fallback when claiming the backup name by hard link fails; not reached
                std::fs::rename(part, &name)
                    .with_context(|| format!("failed to save the backup as {}", name.display()))?;
                return Ok(name);
            }
            Err(_) => continue,
        }
    }
    bail!("no free backup name for {stem}{ext} in {}", dir.display())
}

/// Prefix of the per-run directory a static export is staged in inside the `app`
/// container before it's copied out to the host (see [`export_static`]).
const EXPORT_CONTAINER_PREFIX: &str = "/tmp/aincient-site-export";

/// Folder name the GUI exports into, under the folder the user picks. Exposed so
/// the manager's picker can promise the same name the engine writes.
pub const EXPORT_DIR_NAME: &str = "aincient-site-export";

/// Marker the appliance's exporter writes at the root of every export
/// (`aincient_export`'s `Exporter::MARKER`). Its presence is the only proof
/// that a host directory is ours to replace.
const EXPORT_MARKER: &str = ".aincient-export.json";

/// Refuse to use `target` as an export destination unless it is absent, empty,
/// or a previous export (carries [`EXPORT_MARKER`]). Everything else is
/// somebody's data — `~/Downloads`, `~/Desktop`, a project folder — and the
/// export must never touch it. (v0.9.0 and earlier ran `remove_dir_all` on the
/// picked folder; a user who picked `~/Downloads` lost it.)
fn check_export_target(target: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(target) else {
        return Ok(());
    };
    if !meta.is_dir() {
        bail!(
            "{} exists and is not a folder — choose a different export location",
            target.display()
        );
    }
    if target.join(EXPORT_MARKER).is_file() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(target)
        .with_context(|| format!("failed to read {}", target.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".DS_Store");
    if entries.next().is_none() {
        return Ok(());
    }
    bail!(
        "Refusing to export into {}: the folder is not empty and was not created by an \
         Atelier export (no {} inside). Pick an empty folder, or move the folder's contents \
         out of the way first. Nothing was changed.",
        target.display(),
        EXPORT_MARKER
    )
}

/// Move a freshly copied export at `staged` into place at `target`, replacing a
/// previous export. `target` must already have passed [`check_export_target`];
/// it is re-checked here because time has passed. The staged copy must itself
/// carry the marker, or the copy went somewhere unexpected and nothing is removed.
fn install_export(staged: &Path, target: &Path) -> Result<()> {
    if !staged.join(EXPORT_MARKER).is_file() {
        bail!(
            "the copied export at {} is missing {} — leaving {} untouched",
            staged.display(),
            EXPORT_MARKER,
            target.display()
        );
    }
    check_export_target(target)?;
    if target.exists() {
        // scenario: none — static export (atelier export) has no scenario; replaces the export target
        std::fs::remove_dir_all(target).with_context(|| {
            format!(
                "failed to replace the previous export at {}",
                target.display()
            )
        })?;
    }
    // scenario: none — static export (atelier export) has no scenario
    std::fs::rename(staged, target)
        .with_context(|| format!("failed to move the export into {}", target.display()))
}

/// A not-yet-existing sibling of `target` (same parent, so the final rename is
/// a same-filesystem move) for `docker cp` to create.
fn staging_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| EXPORT_DIR_NAME.to_string());
    let parent = target.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut n = 0u32;
    loop {
        let candidate = parent.join(format!(".{name}.atelier-export-{}-{n}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

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
    let plan = Plan::weighted(&[
        ("prepare", "Prepare", 1.0),
        ("render", "Render", 6.0),
        ("copy", "Save", 2.0),
    ]);
    plan.announce(r);
    r.stage(Stage::Working, "Checking the site…", Some(0.0));
    ensure_running(stack)?;

    let host_out = match &opts.out {
        Some(p) => p.clone(),
        None => std::env::current_dir()
            .context("failed to read the current directory")?
            .join("aincient-export"),
    };
    // Fail before rendering anything if the destination isn't ours to write.
    check_export_target(&host_out)?;
    if let Some(parent) = host_out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let host_zip = host_out.with_extension("zip");
    if opts.zip && host_zip.is_dir() {
        bail!(
            "{} is a folder, so the zip can't be written there — choose a different export location",
            host_zip.display()
        );
    }
    // Per-run staging inside the container: two exports at once (the GUI and the
    // CLI) must never clear or copy out each other's pages.
    let stage = format!("{EXPORT_CONTAINER_PREFIX}-{}", run_id());
    let container_zip = format!("{stage}.zip");
    let result = export_static_in(
        stack,
        opts,
        &plan,
        r,
        &stage,
        &container_zip,
        &host_out,
        &host_zip,
    );
    // Success or failure, the staging never stays behind (a large site's render,
    // once per attempt).
    remove_in_container(stack, &[&stage, &container_zip]);
    result?;

    r.log(&format!("Static site exported to {}", host_out.display()));
    plan.finish(r, "Exported.");
    Ok(host_out)
}

/// The in-container half of [`export_static`]: render into `stage`, copy it (and
/// the zip) out. The caller removes `stage` + `container_zip` afterwards.
#[allow(clippy::too_many_arguments)]
fn export_static_in(
    stack: &Stack,
    opts: &ExportOptions,
    plan: &Plan,
    r: &mut dyn Reporter,
    stage: &str,
    container_zip: &str,
    host_out: &Path,
    host_zip: &Path,
) -> Result<()> {
    // Assemble the drush invocation, rendering into a clean container staging dir.
    let mut export_args: Vec<String> = DRUSH.iter().map(|s| (*s).to_string()).collect();
    export_args.push("aincient:export".into());
    export_args.push(format!("--out={stage}"));
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

    plan.run(r, "render", "Rendering pages to static HTML…", |r| {
        let mut build = compose(stack);
        build.args(["exec", "-T", "app"]);
        build.args(&export_args);
        // The exporter reports page/asset counts and any broken links — surface them.
        run_step(build, "export the static site", r)
    })?;

    plan.run(
        r,
        "copy",
        "Copying the exported site out of the container…",
        |_| {
            // `docker compose cp` copies the source dir *as* the destination when
            // the destination doesn't exist, and *into* it when it does. Copy into
            // a fresh staging sibling, then swap it into place — the previous
            // export is only removed once the new one is verified on disk, and
            // never anything else.
            let staged = staging_sibling(host_out);
            let mut cp = compose(stack);
            cp.args(["cp", &format!("app:{stage}"), &staged.to_string_lossy()]);
            if let Err(e) = run_capture(cp, "copy the exported site out of the container") {
                // scenario: none — static export (atelier export) has no scenario
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
            if let Err(e) = install_export(&staged, host_out) {
                // scenario: none — static export (atelier export) has no scenario
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }

            if opts.zip {
                let mut cpz = compose(stack);
                cpz.args([
                    "cp",
                    &format!("app:{container_zip}"),
                    &host_zip.to_string_lossy(),
                ]);
                run_capture(cpz, "copy the export zip out of the container")?;
            }
            Ok(())
        },
    )
}

/// Options for [`freeze`] — a passthrough onto `drush aincient:freeze`.
#[derive(Debug, Default, Clone)]
pub struct FreezeOptions {
    pub label: Option<String>,
    pub keep: bool,
    pub no_serve: bool,
    pub force: bool,
    pub base_url: Option<String>,
}

/// One frozen snapshot as `drush aincient:snapshots --format=json` reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub frozen_at: Option<String>,
    #[serde(default)]
    pub frozen_by: Option<String>,
    #[serde(default)]
    pub atelier_version: Option<String>,
    #[serde(default)]
    pub pages: u64,
    #[serde(default)]
    pub assets: u64,
    #[serde(default)]
    pub keep: bool,
}

/// What visitors are served plus every snapshot on disk, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotList {
    /// A snapshot id, or `"live"`.
    pub serving: String,
    pub snapshots: Vec<SnapshotRow>,
}

/// Freeze & Live (DECISIONS 0416). Export the published site into a new
/// snapshot inside the appliance and serve it to anonymous visitors; the
/// appliance's web server does the switching, so nothing restarts.
pub fn freeze(stack: &Stack, opts: &FreezeOptions, r: &mut dyn Reporter) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Snapshot)?;
    r.stage(Stage::Working, "Freezing the site…", None);
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .arg("aincient:freeze");
    if let Some(label) = &opts.label {
        c.arg(format!("--label={label}"));
    }
    if opts.keep {
        c.arg("--keep");
    }
    if opts.no_serve {
        c.arg("--no-serve");
    }
    if opts.force {
        c.arg("--force");
    }
    if let Some(base) = &opts.base_url {
        c.arg(format!("--base-url={base}"));
    }
    run_step(c, "freeze the site", r)
}

/// List snapshots and what is served. Numeric fields arrive as strings from the
/// table-oriented drush formatter, so parse leniently.
pub fn snapshots(stack: &Stack) -> Result<SnapshotList> {
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["aincient:snapshots", "--format=json"]);
    let out = run_capture(c, "list the snapshots")?;
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(out.trim()).context("could not parse the snapshot list")?;
    let mut serving = "live".to_string();
    let mut snapshots = Vec::new();
    for row in rows {
        let id = row["id"].as_str().unwrap_or_default().to_string();
        let is_serving = row["serving"].as_bool().unwrap_or(false);
        if is_serving {
            serving = id.clone();
        }
        if id == "live" {
            continue;
        }
        let num = |v: &serde_json::Value| -> u64 {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0)
        };
        let opt = |v: &serde_json::Value| -> Option<String> {
            v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
        };
        snapshots.push(SnapshotRow {
            id,
            label: opt(&row["label"]),
            frozen_at: opt(&row["frozen_at"]),
            frozen_by: opt(&row["frozen_by"]),
            atelier_version: opt(&row["atelier_version"]),
            pages: num(&row["pages"]),
            assets: num(&row["assets"]),
            keep: row["keep"].as_bool().unwrap_or(false),
        });
    }
    Ok(SnapshotList { serving, snapshots })
}

/// Serve a snapshot id, or `"live"`. The appliance validates the id.
pub fn use_snapshot(stack: &Stack, id: &str, r: &mut dyn Reporter) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Snapshot)?;
    r.stage(Stage::Working, "Switching what visitors see…", None);
    ensure_running(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["aincient:use", id]);
    run_capture(c, "switch the served snapshot")?;
    r.log(if id == "live" { "Live." } else { "Frozen." });
    Ok(())
}

/// Delete old snapshots; returns the ids removed.
pub fn prune_snapshots(stack: &Stack, keep: u32, r: &mut dyn Reporter) -> Result<Vec<String>> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Snapshot)?;
    r.stage(Stage::Working, "Pruning snapshots…", None);
    ensure_running(stack)?;
    let before = snapshots(stack)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"])
        .args(DRUSH)
        .args(["aincient:prune", &format!("--keep={keep}")]);
    run_capture(c, "prune the snapshots")?;
    let after = snapshots(stack)?;
    let kept: std::collections::HashSet<&str> =
        after.snapshots.iter().map(|s| s.id.as_str()).collect();
    Ok(before
        .snapshots
        .into_iter()
        .filter(|s| !kept.contains(s.id.as_str()))
        .map(|s| s.id)
        .collect())
}

/// Restore the appliance from a host backup file. Destructive — confirm first.
///
/// A `.tar.gz` **snapshot bundle** (from [`backup`]) restores the database
/// *and* the uploaded-files tree; a legacy `.sql`/`.sql.gz` dump restores the
/// database only. Both mirror converge's `restore_snapshot` for the DB: drop,
/// load, rebuild caches.
pub fn restore(stack: &Stack, file: &Path, r: &mut dyn Reporter) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Restore)?;
    ensure_running(stack)?;
    if !file.is_file() {
        bail!("backup file not found: {}", file.display());
    }
    // Loud, but not a refusal: restoring onto a different image is sometimes
    // exactly the intent (recovering onto a rescue stack), and the appliance now
    // refuses to install over a populated database rather than wiping it. What
    // was missing was the operator knowing which image the data came from.
    if let Some(warning) = restore_skew(stack, file) {
        for line in warning.lines() {
            r.log(line);
        }
    }
    let name = file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let migrate = has_converge(stack);
    if is_snapshot_bundle(name) {
        let mut steps = vec![
            ("copy", "Copy", 1.0),
            ("unpack", "Unpack", 1.0),
            ("database", "Database", 4.0),
            ("files", "Files", 3.0),
        ];
        if migrate {
            steps.push(("migrate", "Migrate", 3.0));
        }
        let plan = Plan::weighted(&steps);
        plan.announce(r);
        restore_bundle(stack, file, &plan, r)?;
        if migrate {
            plan.run(r, "migrate", "Migrating the restored site…", |r| {
                converge_restored(stack, r)
            })?;
        }
        plan.finish(r, "Restored.");
        return Ok(());
    }

    let mut steps = vec![
        ("copy", "Copy", 1.0),
        ("database", "Database", 4.0),
        ("caches", "Caches", 1.0),
    ];
    if migrate {
        steps.push(("migrate", "Migrate", 3.0));
    }
    let plan = Plan::weighted(&steps);
    plan.announce(r);
    let gzipped = file
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gz"))
        .unwrap_or(false);
    let container_tmp = format!(
        "/tmp/aincient-restore-{}.{}",
        run_id(),
        if gzipped { "sql.gz" } else { "sql" }
    );
    let container_tmp = container_tmp.as_str();

    plan.run(
        r,
        "copy",
        "Copying the backup into the container…",
        |_| {
            let mut cp = compose(stack);
            // scenario: none — restoring a plain .sql dump; the scenarios restore backup bundles
            cp.args([
                "cp",
                &file.to_string_lossy(),
                &format!("app:{container_tmp}"),
            ]);
            run_capture(cp, "copy the backup into the container")
        },
    )
    .inspect_err(|_| remove_in_container(stack, &[container_tmp]))?;

    plan.run(r, "database", "Restoring the database…", |r| {
        // Nothing is dropped until the dump is known to be readable: a truncated
        // or empty file must fail here, with the current database untouched.
        let check = if gzipped {
            format!("test -s {container_tmp} && gzip -t {container_tmp}")
        } else {
            format!("test -s {container_tmp}")
        };
        let mut c = compose(stack);
        c.args(["exec", "-T", "app", "sh", "-c", &check]);
        run_capture(c, "check the backup file (your database was not touched)")?;

        r.log("Dropping the current database…");
        let mut drop = compose(stack);
        drop.args(["exec", "-T", "app"])
            .args(DRUSH)
            .args(["sql:drop", "-y"]);
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
        run_capture(load, "load the backup")
    })
    .inspect_err(|_| remove_in_container(stack, &[container_tmp]))?;

    plan.run(r, "caches", "Rebuilding caches…", |_| {
        let mut cr = compose(stack);
        cr.args(["exec", "-T", "app"])
            .args(DRUSH)
            .arg("cache:rebuild");
        let _ = cr.output();

        remove_in_container(stack, &[container_tmp]);
        Ok(())
    })?;

    if migrate {
        plan.run(r, "migrate", "Migrating the restored site…", |r| {
            converge_restored(stack, r)
        })?;
    }
    plan.finish(r, "Restored.");
    Ok(())
}

/// Restore a full snapshot bundle (`.tar.gz` from [`backup`]): database + files.
/// The whole thing runs as one in-container script so the DB load and the files
/// swap stay together. Files are rewritten as root, so we re-assert
/// `www-data` ownership afterwards — exactly as `entrypoint.sh` does on boot —
/// or restored uploads and their derivatives would be unwritable/unreadable.
fn restore_bundle(stack: &Stack, file: &Path, plan: &Plan, r: &mut dyn Reporter) -> Result<()> {
    let work = format!("/tmp/aincient-restore-{}", run_id());
    let archive = format!("{work}.tar.gz");
    // The files step removes both on success; any failure removes them here.
    restore_bundle_in(stack, file, plan, r, &work, &archive)
        .inspect_err(|_| remove_in_container(stack, &[&work, &archive]))
}

fn restore_bundle_in(
    stack: &Stack,
    file: &Path,
    plan: &Plan,
    r: &mut dyn Reporter,
    work: &str,
    archive: &str,
) -> Result<()> {
    plan.run(
        r,
        "copy",
        "Copying the snapshot into the container…",
        |_| {
            let mut cp = compose(stack);
            // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without, a_restore_keeps_the_three_newest_counted_backups_and_never_prunes_older_ones
            cp.args(["cp", &file.to_string_lossy(), &format!("app:{archive}")]);
            run_capture(cp, "copy the snapshot into the container")
        },
    )?;

    let drush = DRUSH.join(" ");
    let [unpack, database, files] = restore_bundle_scripts(&drush, work);
    let exec = |script: &str, action: &str| {
        let mut c = compose(stack);
        c.args(["exec", "-T", "app", "sh", "-c", script]);
        run_capture(c, action)
    };
    plan.run(r, "unpack", "Unpacking the snapshot…", |_| {
        exec(&unpack, "unpack the snapshot")
    })?;
    plan.run(r, "database", "Restoring the database…", |_| {
        exec(&database, "restore the database")
    })?;
    plan.run(r, "files", "Restoring the uploaded files…", |_| {
        exec(&files, "restore the uploaded files")
    })?;
    Ok(())
}

/// What a snapshot bundle's `manifest.json` records about where it came from.
/// Every field is optional in practice: v1 archives (before 2026-08-06) carry
/// only `image`, and a locally built image has no digest to record.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct SnapshotManifest {
    /// The image reference configured when the backup was taken — possibly a
    /// moving tag, which is why it is not enough on its own.
    #[serde(default)]
    pub image: String,
    /// The content-addressable digest of that image. The field that actually
    /// identifies the build this database is known to run on.
    #[serde(default)]
    pub digest: String,
    /// `org.opencontainers.image.version` of that image, when it was stamped.
    #[serde(default)]
    pub atelier_version: String,
    #[serde(default)]
    pub created: String,
    /// The identity of the site it was taken from (v3, m2-6). Empty in older
    /// archives: their origin is unknown.
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub site_slug: String,
    #[serde(default)]
    pub site_label: String,
}

impl SnapshotManifest {
    /// `Label (slug)` of the site it came from, as far as it says.
    pub fn site_name(&self) -> String {
        match (self.site_label.as_str(), self.site_slug.as_str()) {
            ("", "") => "an unnamed site".to_string(),
            (label, slug) if label.is_empty() || label == slug => slug.to_string(),
            (label, "") => label.to_string(),
            (label, slug) => format!("{label} ({slug})"),
        }
    }
}

/// An operation that needs the site up found it stopped. Typed so each
/// front-end says how to start it: this wording names the CLI command; the
/// window matches the type and points at its own Start button instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotRunning;

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the appliance isn't running — start it with `atelier app start`")
    }
}

impl std::error::Error for NotRunning {}

/// A restore was refused: the archive belongs to another site (m2-6, D-04).
/// Typed so a front-end can offer the override instead of only showing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityMismatch {
    pub archive_site_id: String,
    pub archive_site_name: String,
    /// `None`: the target has no identity yet (it never made a backup).
    pub target_site_id: Option<String>,
    pub target_site_name: String,
}

impl std::fmt::Display for IdentityMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this backup is from another site — {} (id {}) — and would replace {} (id {}). \
             Nothing was changed. To restore it here anyway, pass --other-site.",
            self.archive_site_name,
            self.archive_site_id,
            self.target_site_name,
            self.target_site_id.as_deref().unwrap_or("none yet"),
        )
    }
}

impl std::error::Error for IdentityMismatch {}

/// What the identity check found for an allowed restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityCheck {
    /// The archive was taken from this very site.
    Same,
    /// The archive records no site (taken before identities existed, a legacy
    /// `.sql` dump, or an unreadable manifest): allowed, with this warning.
    Unknown(String),
    /// Another site's archive, allowed by `--other-site`: this note.
    OtherAllowed(String),
}

impl IdentityCheck {
    /// The line to show the user, if any.
    pub fn message(&self) -> Option<&str> {
        match self {
            IdentityCheck::Same => None,
            IdentityCheck::Unknown(m) | IdentityCheck::OtherAllowed(m) => Some(m),
        }
    }
}

/// Decide whether an archive with `manifest` may be restored onto the site
/// identified by `target_id`/`target_name` (m2-6, D-04). Pure: the local and
/// remote paths feed it what they read. A mismatch is an [`IdentityMismatch`]
/// unless `other_site`.
pub fn check_identity(
    manifest: Option<&SnapshotManifest>,
    target_id: Option<&str>,
    target_name: &str,
    other_site: bool,
) -> Result<IdentityCheck> {
    let Some(m) = manifest.filter(|m| !m.site_id.is_empty()) else {
        return Ok(IdentityCheck::Unknown(
            "WARNING: this backup doesn't say which site it was taken from (it predates \
             site identities) — make sure it is one of this site's own."
                .to_string(),
        ));
    };
    if target_id == Some(m.site_id.as_str()) {
        return Ok(IdentityCheck::Same);
    }
    if other_site {
        return Ok(IdentityCheck::OtherAllowed(format!(
            "Restoring a backup of another site — {} (id {}) — onto {target_name}, as \
             --other-site asked.",
            m.site_name(),
            m.site_id
        )));
    }
    Err(anyhow::anyhow!(IdentityMismatch {
        archive_site_id: m.site_id.clone(),
        archive_site_name: m.site_name(),
        target_site_id: target_id.map(str::to_string),
        target_site_name: target_name.to_string(),
    }))
}

/// [`check_identity`] for restoring `file` onto `stack`.
pub fn restore_identity(stack: &Stack, file: &Path, other_site: bool) -> Result<IdentityCheck> {
    check_identity(
        snapshot_manifest(file).as_ref(),
        stack.site_id().as_deref(),
        &stack.display_name(),
        other_site,
    )
}

/// How [`restore_with`] treats an archive from another site.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOptions {
    /// Restore another site's archive anyway (`--other-site`).
    pub other_site: bool,
    /// Restore without first backing up the site as it is now
    /// (`--no-safety-backup`). Without it, a backup that fails stops the restore.
    pub no_safety_backup: bool,
    /// The caller already showed the archive's identity note ([`restore_identity`]):
    /// the CLI prints it before its confirmation, so the restore doesn't say
    /// it a second time ("Restoring a backup of another site — …" twice).
    pub identity_shown: bool,
}

/// A restore was refused because the backup of the site as it is now failed:
/// the restore drops the database, and without that copy nothing could undo it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyBackupFailed {
    /// Why the backup failed, as its error chain reads.
    pub cause: String,
}

impl std::fmt::Display for SafetyBackupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "couldn't back up your site as it is now, so nothing was restored and your \
             site is unchanged. The backup failed: {}",
            self.cause
        )
    }
}

impl std::error::Error for SafetyBackupFailed {}

/// A refusal the owner can override, knowing what it costs: a restore whose
/// safety backup failed ([`SafetyBackupFailed`]; `--no-safety-backup`) or an
/// update whose site couldn't be backed up first ([`BackupBeforeUpdateFailed`];
/// `--no-backup`). The Manager offers the override behind a confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overridable {
    /// Restore again without the safety backup.
    RestoreWithoutSafetyBackup,
    /// Update again without the backup first.
    UpdateWithoutBackup,
}

impl Overridable {
    /// The exit code `atelier` ends with for this refusal, so a Manager driving
    /// it over SSH can tell it apart without reading its words.
    pub const fn exit_code(self) -> i32 {
        match self {
            Overridable::RestoreWithoutSafetyBackup => 3,
            Overridable::UpdateWithoutBackup => 4,
        }
    }

    fn from_exit_code(code: i32) -> Option<Self> {
        [
            Overridable::RestoreWithoutSafetyBackup,
            Overridable::UpdateWithoutBackup,
        ]
        .into_iter()
        .find(|o| o.exit_code() == code)
    }

    /// Which refusal `e` is, if any: the typed error here, or a server's
    /// `atelier` that ended with its exit code.
    pub fn of(e: &anyhow::Error) -> Option<Self> {
        if e.is::<SafetyBackupFailed>() {
            Some(Overridable::RestoreWithoutSafetyBackup)
        } else if e.is::<BackupBeforeUpdateFailed>() {
            Some(Overridable::UpdateWithoutBackup)
        } else {
            e.downcast_ref::<crate::remote::RemoteExit>()
                .and_then(|x| Self::from_exit_code(x.code))
        }
    }
}

/// The restore's safety backup must have worked, or been skipped because the
/// site isn't running (no running database to lose): a failure stops the
/// restore before anything is dropped.
fn require_safety_backup(outcome: Result<Option<PathBuf>>) -> Result<Option<PathBuf>> {
    outcome.map_err(|e| {
        anyhow::Error::new(SafetyBackupFailed {
            cause: format!("{e:#}"),
        })
    })
}

/// [`restore`] behind the site-identity check (m2-6, D-04): another site's
/// archive is refused with an [`IdentityMismatch`] unless `opts.other_site`;
/// one that records no site is restored with a warning through `r`.
pub fn restore_with(
    stack: &Stack,
    file: &Path,
    opts: &RestoreOptions,
    r: &mut dyn Reporter,
) -> Result<()> {
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Restore)?;
    if !file.is_file() {
        bail!("backup file not found: {}", file.display());
    }
    // Checked either way: the refusal of another site's archive is here.
    let identity = restore_identity(stack, file, opts.other_site)?;
    if let Some(note) = identity.message().filter(|_| !opts.identity_shown) {
        r.log(note);
    }
    let erased = restore_into_erased(stack, r)?;
    // A restore drops the database before loading the backup: without a copy of
    // the site as it is now, a wrong pick or a failed load loses the work since.
    // So a backup that fails stops the restore (review 2026-11-14), unless the
    // owner said to go without one.
    let before = if opts.no_safety_backup {
        r.log("Restoring without backing up your site as it is now (--no-safety-backup).");
        None
    } else if erased {
        r.log("Your site's database was removed, so there is nothing to back up first.");
        None
    } else {
        require_safety_backup(safety_backup(
            stack,
            "before-restore",
            "Backing up your site as it is now…",
            0.0,
            0.0,
            r,
        ))?
    };
    restore(stack, file, r).map_err(|e| match &before {
        Some(path) => e.context(format!(
            "the restore didn't finish. Your site as it was just before is saved in {}; restore that to go back",
            path.display()
        )),
        None => e,
    })?;
    // The site may now run past an unfinished update's target.
    drop_stale_update_target(stack);
    // Only after the restore worked, and never the backup just restored.
    prune_automatic_backups(stack, Some(file), r);
    Ok(())
}

/// The way back from [`DatabaseErased`]: Start refuses such a site, so a
/// restore starts it with an empty database first (what an install does) and
/// then loads the backup into it. Returns whether it did. A running site, or one
/// whose database is still there, is left to [`restore`]'s own checks.
fn restore_into_erased(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    ensure_installed(stack)?;
    preflight().require()?;
    if status(stack).running || !stack_database_erased(stack) {
        return Ok(false);
    }
    r.stage(
        Stage::Starting,
        "Your site's database was removed from Docker. Starting the site with an empty one to restore into…",
        None,
    );
    up_unguarded(stack, r)?;
    if !wait_until_ready(stack, READY_TIMEOUT, r).ready()? {
        bail!(
            "The site is still setting up its empty database, so the backup wasn't restored yet. \
             Try the restore again in a few minutes."
        );
    }
    Ok(true)
}

/// A backup before a change the owner can't undo by running the old image again
/// (a one-step update, a restore). `Ok(None)` when the site isn't running (there
/// is nothing new to save); a failure is returned for the caller to weigh (an
/// update goes ahead, a restore stops). Prunes nothing: older automatic backups
/// go only after the change succeeded. `base`/`span` place the backup's own
/// progress inside the caller's bar (`span` 0 = indeterminate).
fn safety_backup(
    stack: &Stack,
    label: &str,
    headline: &str,
    base: f32,
    span: f32,
    r: &mut dyn Reporter,
) -> Result<Option<PathBuf>> {
    if !status(stack).running {
        return Ok(None);
    }
    let at = |f: f32| (span > 0.0).then_some(f);
    r.stage(Stage::Working, headline, at(base));
    let mut snap = ScaledReporter {
        inner: r,
        base,
        span,
        prefix: String::new(),
    };
    let path = backup(stack, Some(label), &mut snap)?;
    r.stage(
        Stage::Working,
        &format!("Backup saved: {}", path.display()),
        at(base + span),
    );
    Ok(Some(path))
}

/// Describe a backup archive the way `data backup --json` reports it: size and
/// SHA-256 of the file, and the site it came from per its manifest.
pub fn describe_backup(file: &Path) -> Result<BackupInfo> {
    let (size, sha256) = sha256_file(file)?;
    let manifest = snapshot_manifest(file).filter(|m| !m.site_id.is_empty());
    Ok(BackupInfo {
        path: file.to_string_lossy().into_owned(),
        name: file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size,
        sha256,
        site_id: manifest.as_ref().map(|m| m.site_id.clone()),
        site_name: manifest.as_ref().map(SnapshotManifest::site_name),
    })
}

/// Size and lowercase hex SHA-256 of `file`, read in chunks (archives can be
/// gigabytes).
pub fn sha256_file(file: &Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut f =
        std::fs::File::open(file).with_context(|| format!("could not read {}", file.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("could not read {}", file.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, hex))
}

/// Read `manifest.json` out of a snapshot bundle without unpacking it. `tar -O`
/// streams the one member to stdout; BSD tar (macOS) and GNU tar both take these
/// flags. `None` for a legacy `.sql` dump, an unreadable archive, or a manifest
/// this version can't parse — the caller treats all of those as "unknown
/// provenance", never as "matches".
pub fn snapshot_manifest(file: &Path) -> Option<SnapshotManifest> {
    let name = file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if !is_snapshot_bundle(name) {
        return None;
    }
    let out = Command::new("tar")
        .arg("-xzOf")
        .arg(file)
        .arg("manifest.json")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<SnapshotManifest>(&out.stdout).ok()
}

/// A human-readable warning when a snapshot was taken on a different image than
/// the one installed here, or `None` when they match or we can't tell.
///
/// The point is naming the image the data is known to work on. Recovering the
/// 2026-08-06 incident took hours mostly because that fact existed nowhere: the
/// manifest said `:edge`, which by then pointed at the build that had just
/// destroyed the site.
pub fn restore_skew(stack: &Stack, file: &Path) -> Option<String> {
    let manifest = snapshot_manifest(file)?;
    if manifest.digest.is_empty() {
        return None;
    }
    let current = site_probe(stack).ok()?.digest?;
    if current == manifest.digest {
        return None;
    }

    let taken_on = if manifest.atelier_version.is_empty() {
        manifest.image.clone()
    } else {
        format!("{} ({})", manifest.atelier_version, manifest.image)
    };
    // The bare repository, so the hint can re-pin it by digest. Strip a digest,
    // then a tag — but only a real tag: the colon in a `host:5000/repo` registry
    // is followed by a slash, and stripping there would name a different image.
    let repo = stack
        .image()
        .split('@')
        .next()
        .unwrap_or_default()
        .to_string();
    let repo = match repo.rsplit_once(':') {
        Some((head, tail)) if !tail.contains('/') => head.to_string(),
        _ => repo,
    };
    Some(format!(
        "WARNING: this snapshot was taken on a DIFFERENT image than the one installed here.\n\
         \x20 snapshot taken on: {taken_on}\n\
         \x20   digest:          {snap}\n\
         \x20 installed now:     {now}\n\
         \x20   digest:          {cur}\n\
         The restore will proceed, but if the site does not come up afterwards, run it\n\
         on the image the data came from instead of changing anything else:\n\
         \x20 atelier app install --image {repo}@{snap}\n",
        snap = manifest.digest,
        now = stack.image(),
        cur = current,
    ))
}

/// The in-container shell that builds a snapshot bundle. `drush --gzip` appends
/// `.gz` to --result-file (see converge.sh). One `tar` with two `-C` changes
/// packs the staged DB dump + manifest, then the live files tree, so `files/`
/// lands at the archive root. `manifest` is JSON, shell-quoted here as one word.
/// The in-container script that builds the snapshot bundle: DB dump + manifest +
/// the uploaded files tree + frozen snapshots.
///
/// `files/styles` is excluded on purpose. It is Drupal's image-derivative
/// cache: a live site regenerates a missing derivative on first request, and a
/// static export/freeze generates the ones it links on its own (the exporter's
/// kernel-replay fallback). Packing it buys nothing on restore and it dominated
/// the archive — 246 MB of a 261 MB local backup on 2026-09-11, most of it PNG
/// derivatives no page referenced. Frozen snapshots under `private/frozen/`
/// carry their own derivatives and are NOT affected by the exclude (the pattern
/// is anchored to the `files` member).
///
/// Two scripts, run as two execs: [`backup_dump_script`] stages the dump and
/// manifest, [`backup_pack_script`] tars them with the files tree.
fn backup_dump_script(drush: &str, manifest: &str, stage: &str) -> String {
    let manifest = crate::remote::sh_quote(manifest);
    format!(
        "set -e\n\
         STAGE={stage}\n\
         ARCHIVE={stage}.tar.gz\n\
         rm -rf \"$STAGE\" \"$ARCHIVE\"\n\
         mkdir -p \"$STAGE\"\n\
         {drush} sql:dump --gzip --result-file=\"$STAGE/database.sql\" >/dev/null\n\
         test -s \"$STAGE/database.sql.gz\"\n\
         gzip -t \"$STAGE/database.sql.gz\"\n\
         printf '%s\\n' {manifest} > \"$STAGE/manifest.json\"\n",
    )
}

/// The second half of the snapshot: pack the staged dump + manifest with the
/// files tree, frozen snapshots and chat attachments (see [`backup_dump_script`]).
///
/// Chat attachments live in `private/atelier-context` (cms `ContextStore`, one
/// ledger row each in the database): a backup without them restored chats whose
/// images were gone. Packed only when the folder exists (a site that never had
/// an attachment has none), so it is not created here as root.
fn backup_pack_script(stage: &str) -> String {
    // The last two lines read the archive back: a truncated or unreadable archive,
    // or one missing the dump or manifest, fails the backup instead of being
    // reported as saved.
    format!(
        "set -e\n\
         STAGE={stage}\n\
         ARCHIVE={stage}.tar.gz\n\
         mkdir -p /opt/drupal/private/frozen\n\
         CONTEXT=\n\
         if [ -d /opt/drupal/private/atelier-context ]; then CONTEXT=atelier-context; fi\n\
         tar czf \"$ARCHIVE\" --exclude=files/styles -C \"$STAGE\" manifest.json database.sql.gz \
         -C /opt/drupal/web/sites/default files \
         -C /opt/drupal/private frozen $CONTEXT\n\
         tar tzf \"$ARCHIVE\" > \"$STAGE/members\"\n\
         grep -qx database.sql.gz \"$STAGE/members\" && grep -qx manifest.json \"$STAGE/members\"\n"
    )
}

/// The in-container shell that restores a snapshot bundle: drop + reload the DB,
/// then swap the files tree. `find -mindepth 1 -delete` clears the files dir
/// (dotfiles included) without removing the volume mount point; `cp -a`
/// preserves the tree; the `chown` re-asserts `www-data` ownership (files are
/// written as root) exactly as `entrypoint.sh` does. cache:rebuild and cleanup
/// are best-effort so a hiccup there doesn't fail the restore.
///
/// Three scripts, one exec each, so a failure names its step (DECISIONS 0515):
/// unpack, database, files (files also rebuilds caches and cleans up).
fn restore_bundle_scripts(drush: &str, work: &str) -> [String; 3] {
    let head = format!(
        "set -e\n\
         ARCHIVE={work}.tar.gz\n\
         WORK={work}\n"
    );
    let unpack = format!(
        "{head}\
         rm -rf \"$WORK\"\n\
         mkdir -p \"$WORK\"\n\
         tar xzf \"$ARCHIVE\" -C \"$WORK\"\n"
    );
    // Nothing is dropped until the snapshot's dump is there and readable.
    let database = format!(
        "{head}\
         test -s \"$WORK/database.sql.gz\"\n\
         gzip -t \"$WORK/database.sql.gz\"\n\
         {drush} sql:drop -y\n\
         zcat \"$WORK/database.sql.gz\" | {drush} sql:cli\n"
    );
    let files = format!(
        "{head}\
         DEST={FILES_DIR}\n\
         if [ -d \"$WORK/files\" ]; then\n\
         \x20 find \"$DEST\" -mindepth 1 -delete 2>/dev/null || true\n\
         \x20 cp -a \"$WORK/files/.\" \"$DEST/\"\n\
         \x20 chown -R www-data:www-data \"$DEST\"\n\
         fi\n\
         if [ -d \"$WORK/frozen\" ]; then\n\
         \x20 rm -rf /opt/drupal/private/frozen\n\
         \x20 cp -a \"$WORK/frozen\" /opt/drupal/private/frozen\n\
         \x20 chown -R www-data:www-data /opt/drupal/private/frozen\n\
         fi\n\
         if [ -d \"$WORK/atelier-context\" ]; then\n\
         \x20 rm -rf /opt/drupal/private/atelier-context\n\
         \x20 cp -a \"$WORK/atelier-context\" /opt/drupal/private/atelier-context\n\
         \x20 chown -R www-data:www-data /opt/drupal/private/atelier-context\n\
         fi\n\
         {drush} cache:rebuild || true\n\
         rm -rf \"$WORK\" \"$ARCHIVE\" || true\n"
    );
    [unpack, database, files]
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

/// Labels Atelier gives the backups it takes by itself. Only these are ever
/// pruned; a backup the owner made (no label, or their own) is never touched.
const AUTOMATIC_BACKUP_LABELS: [&str; 4] = [
    "before-update",
    "before-restore",
    "before-stepped-upgrade",
    "before-channel-switch",
];

/// How many automatic backups to keep (owner, 2026-11-14).
const AUTOMATIC_BACKUPS_KEPT: usize = 3;

/// Where a site records when its automatic backups started counting toward
/// [`AUTOMATIC_BACKUPS_KEPT`]: `<home>/.automatic-backups-since`, holding a
/// `YYYYMMDD-HHMMSS` stamp as backup names carry it.
///
/// Manager versions before 0.17.0 took automatic backups and never removed
/// any. Owner, 2026-11-14: only those taken from 0.17.0 on count toward the
/// three kept; older ones are never removed automatically (Free up space
/// offers them, [`older_automatic_backups`]). The stamp is that of the first
/// automatic backup this version took, so every one it takes counts and none
/// before it does.
pub const AUTOMATIC_SINCE_FILE: &str = ".automatic-backups-since";

/// The `YYYYMMDD-HHMMSS` stamp of a backup Atelier took by itself, as
/// [`backup`] names it: `aincient-<label>-<YYYYMMDD>-<HHMMSS>[-N].tar.gz`.
/// `None` for any other file. The `aincient-` prefix comes first (review
/// 2026-11-14: matching the label against the whole name meant nothing was
/// ever pruned).
fn automatic_backup_stamp(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("aincient-")?;
    AUTOMATIC_BACKUP_LABELS.iter().find_map(|l| {
        let stamp = rest.strip_prefix(l)?.strip_prefix('-')?;
        is_stamp(stamp.get(..15)?).then(|| &stamp[..15])
    })
}

/// `YYYYMMDD-HHMMSS`.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..].iter().all(u8::is_ascii_digit)
}

/// The stamp in [`AUTOMATIC_SINCE_FILE`], if this site has one yet.
pub fn automatic_backups_since(stack: &Stack) -> Option<String> {
    let raw = std::fs::read_to_string(stack.home.join(AUTOMATIC_SINCE_FILE)).ok()?;
    let stamp = raw.trim();
    is_stamp(stamp).then(|| stamp.to_string())
}

/// Record `stamp` as the start of counted automatic backups, unless the site
/// has a start already. Best effort: without it nothing is pruned.
fn note_automatic_backups_since(stack: &Stack, stamp: &str) {
    if automatic_backups_since(stack).is_none() {
        // scenario: restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without, start_and_doctor_refuse_an_erased_database_and_a_restore_brings_it_back
        let _ = std::fs::write(stack.home.join(AUTOMATIC_SINCE_FILE), format!("{stamp}\n"));
    }
}

/// The same file, however each path spells it (a relative path, `..`, a
/// symlinked folder such as macOS's `/tmp` → `/private/tmp`).
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Automatic backups that count toward the newest `keep`: those stamped at or
/// after `since` ([`AUTOMATIC_SINCE_FILE`]), newest first. None without a
/// `since`.
fn counted_automatic_backups<'a>(backups: &'a [Backup], since: Option<&str>) -> Vec<&'a Backup> {
    let Some(since) = since else {
        return Vec::new();
    };
    let mut auto: Vec<&Backup> = backups
        .iter()
        .filter(|b| automatic_backup_stamp(&b.name).is_some_and(|s| s >= since))
        .collect();
    auto.sort_by(|a, b| {
        b.modified_unix
            .cmp(&a.modified_unix)
            .then_with(|| automatic_backup_stamp(&b.name).cmp(&automatic_backup_stamp(&a.name)))
            .then_with(|| b.name.cmp(&a.name))
    });
    auto
}

/// The counted automatic backups ([`counted_automatic_backups`]) beyond the
/// newest `keep`, oldest last — what a prune removes. Older automatic backups
/// are never among them, and neither is `protect` (the backup a restore just
/// loaded), however its path is spelled.
fn automatic_backups_to_prune<'a>(
    backups: &'a [Backup],
    keep: usize,
    protect: Option<&Path>,
    since: Option<&str>,
) -> Vec<&'a Backup> {
    counted_automatic_backups(backups, since)
        .into_iter()
        .skip(keep)
        // Never the backup a restore loaded (review 2026-11-14: a prune after
        // the before-restore backup could delete the owner's pick).
        .filter(|b| !protect.is_some_and(|p| same_file(Path::new(&b.path), p)))
        .collect()
}

/// Automatic backups older than `since` (all of them without one): taken
/// before automatic backups counted, never pruned, and what Free up space
/// offers to remove. Never an owner's backup, never a counted one.
fn older_automatic_backups<'a>(backups: &'a [Backup], since: Option<&str>) -> Vec<&'a Backup> {
    backups
        .iter()
        .filter(|b| {
            automatic_backup_stamp(&b.name).is_some_and(|s| since.is_none_or(|since| s < since))
        })
        .collect()
}

/// [`older_automatic_backups`] for `stack`, newest first by the stamp in
/// their names (a copied file's date says nothing).
pub fn older_backups(stack: &Stack) -> Vec<Backup> {
    let backups = list_backups(stack);
    let since = automatic_backups_since(stack);
    let mut older: Vec<Backup> = older_automatic_backups(&backups, since.as_deref())
        .into_iter()
        .cloned()
        .collect();
    older.sort_by(|a, b| {
        automatic_backup_stamp(&b.name)
            .cmp(&automatic_backup_stamp(&a.name))
            .then_with(|| b.name.cmp(&a.name))
    });
    older
}

/// Keep the newest [`AUTOMATIC_BACKUPS_KEPT`] counted automatic backups;
/// delete older counted ones. Runs only after the update or restore that took
/// one succeeded, so a failed attempt (and its retries) never costs a backup. A
/// file that won't delete is logged, not fatal. A site with no start for
/// counting yet gets one now, and nothing goes.
fn prune_automatic_backups(stack: &Stack, protect: Option<&Path>, r: &mut dyn Reporter) {
    let Some(since) = automatic_backups_since(stack) else {
        note_automatic_backups_since(
            stack,
            &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
        );
        return;
    };
    let backups = list_backups(stack);
    for b in automatic_backups_to_prune(&backups, AUTOMATIC_BACKUPS_KEPT, protect, Some(&since)) {
        // scenario: a_restore_keeps_the_three_newest_counted_backups_and_never_prunes_older_ones
        match std::fs::remove_file(&b.path) {
            Ok(()) => r.log(&format!("Removed an older automatic backup: {}", b.name)),
            Err(e) => r.log(&format!("Couldn't remove the older backup {}: {e}", b.name)),
        }
    }
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
    out.sort_by_key(|a| std::cmp::Reverse(a.modified_unix));
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
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Settings)?;
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
    let _op = crate::oplock::hold(stack, crate::oplock::Operation::Settings)?;
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

/// Capture the most recent appliance log lines (no follow) as plain text — the
/// snapshot the GUI's activity view shows and re-fetches on demand. `--no-color`
/// keeps the webview text clean; `--tail` bounds how much we read back.
pub fn tail_logs(stack: &Stack, service: Option<&str>, lines: usize) -> Result<String> {
    let mut c = compose(stack);
    c.args(["logs", "--no-color", "--tail", &lines.to_string()]);
    if let Some(svc) = service {
        c.arg(svc);
    }
    run_capture(c, "read the appliance logs")
}

/// Open the console (the chat workspace at `/atelier`) in the default browser.
pub fn open_console(stack: &Stack) -> Result<()> {
    open_url(&stack.console_url())
}

/// Open the console **already signed in**. A fresh appliance mints a random
/// admin password (see `converge.sh`) that the manager deliberately never
/// shows, so navigating to `/atelier` as an anonymous user just access-denies.
/// This mints a one-time login link via `drush user:login` inside the `app`
/// container — redirecting to `/atelier` — and opens it, dropping the operator
/// straight into the console authenticated, without a password ever surfacing.
///
/// The link is a short-lived, single-use capability URL: fine for the
/// localhost appliance, but worth revisiting if the port is ever exposed
/// beyond the local machine.
pub fn open_console_authed(stack: &Stack) -> Result<()> {
    ensure_running(stack)?;

    // `--uri` fixes the host/port drush renders into the absolute login URL so
    // it targets the host-mapped port the browser can reach (not the
    // container-internal one); the positional path is where to land after login.
    let mut c = compose(stack);
    c.args(["exec", "-T", "app"]);
    c.args(DRUSH);
    c.args([
        "user:login",
        &format!("--uri={}", stack.site_url()),
        "/atelier",
    ]);
    let out = run_capture(c, "generate a one-time login link")?;

    // drush prints the URL (occasionally alongside notices) — take the first
    // line that looks like one.
    let url = out.lines().map(str::trim).find(|l| l.starts_with("http"));
    match url {
        Some(url) => open_url(url),
        None => bail!("drush did not return a login link:\n{}", out.trim()),
    }
}

/// Open Drupal's login form (`/user/login`) in the default browser so the
/// operator can sign in directly — the manager never reveals the password.
pub fn open_login(stack: &Stack) -> Result<()> {
    open_url(&stack.login_url())
}

/// Open a URL in the user's default browser.
pub fn open_url(url: &str) -> Result<()> {
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
        return Err(NotRunning.into());
    }
    Ok(())
}

pub(crate) fn parse_ps(out: &str) -> Vec<PsEntry> {
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

    // `/user/login`, NOT `/`. The front page is anonymously page-cacheable, so it
    // is the one URL that stays warm on a site whose every uncached route is
    // throwing — which is how "the console is up" was announced to the reporter of
    // manager#2 moments before every page they clicked returned a 500. A route that
    // carries a form token cannot be served from the page cache, so a 200 here means
    // the container actually rendered something.
    let req = "GET /user/login HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
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

/// The OCI label the image build stamps with the version it is (DECISIONS 0308).
/// The image label every appliance build is stamped with: its version.
pub const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// The label carrying an image's upgrade floor — the oldest version whose database
/// its `converge.sh` will migrate. Baked from `docker/upgrade-floor`.
///
/// Read from the REGISTRY, which is the reason it is a label at all: the manager
/// has to know an image's floor before deciding to pull it, and the same value in
/// a file inside the image is only readable once the image is already downloaded.
const FLOOR_LABEL: &str = "dev.atelier.upgrade.min-from";

/// The label listing the NON-CORE extensions an image ships, comma-separated.
/// Registry-readable for the same reason as [`FLOOR_LABEL`]: the decision it
/// informs — is this image missing code the site has installed — has to be made
/// before the ~500 MB pull, not after. Absent on images built before it existed
/// and on local builds; absence is "unknown", never "ships nothing".
const EXTENSIONS_LABEL: &str = "dev.atelier.extensions";

/// What an image says about itself, from either side of the pull.
#[derive(Debug, Default, Clone)]
struct ImageProbe {
    /// Registry digest — absent on a locally built image.
    digest: Option<String>,
    /// `org.opencontainers.image.version` — absent on builds predating the stamp.
    version: Option<String>,
    /// [`FLOOR_LABEL`] as a parsed version. Absent means "declares no floor", and
    /// is the correct reading for every image built before this existed.
    floor: Option<Version>,
    /// [`EXTENSIONS_LABEL`], split. `None` means the image doesn't say.
    extensions: Option<Vec<String>>,
}

/// The image id (`sha256:…`) the site's `app` container was created from, running
/// or stopped. Survives the tag moving on: the old image stays on disk, with its
/// `RepoDigests`, as long as a container uses it. `None` without an `app`
/// container.
fn running_image_id(stack: &Stack) -> Option<String> {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "-q", "app"]);
    let cid = try_capture(ps)?;
    let cid = cid.lines().next()?.trim().to_string();
    if cid.is_empty() {
        return None;
    }
    let mut c = docker::docker();
    c.args(["inspect", &cid, "--format", "{{.Image}}"]);
    let id = try_capture(c)?.trim().to_string();
    id.starts_with("sha256:").then_some(id)
}

/// [`local_probe`] of the build this site runs (see [`running_image_id`]),
/// falling back to the configured tag when there's no container.
///
/// Every question about "what is this site on" goes through here, never through
/// the tag: sites share the moving tag, so another site's update re-points it
/// while this one keeps running its old build. Read off the tag, the update
/// check said "up to date", the route planner started from the wrong version,
/// and a backup's manifest pinned a digest its database never ran on.
fn site_probe(stack: &Stack) -> std::result::Result<ImageProbe, String> {
    match running_image_id(stack) {
        Some(id) => local_probe(&id),
        None => local_probe(&stack.image()),
    }
}

fn site_digest(stack: &Stack) -> Option<String> {
    site_probe(stack).ok().and_then(|p| p.digest)
}

/// Read the pulled image's registry digest **and** version label in one
/// `docker image inspect` — no container start needed, which is why the stamp is
/// carried as a label and not only as a runtime env var.
///
/// Both fields are guarded with `{{if}}`: a locally built image has an empty
/// `RepoDigests` (bare `index` on it is a template error, not an empty string)
/// and may carry no labels at all. Those are missing values, not failures — the
/// `Err` arm means inspect itself failed, i.e. the image isn't here.
fn local_probe(image: &str) -> std::result::Result<ImageProbe, String> {
    let mut c = docker::docker();
    c.args([
        "image",
        "inspect",
        image,
        "--format",
        &format!(
            "{{{{if .RepoDigests}}}}{{{{index .RepoDigests 0}}}}{{{{end}}}}|\
             {{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{VERSION_LABEL}\"}}}}{{{{end}}}}|\
             {{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{FLOOR_LABEL}\"}}}}{{{{end}}}}|\
             {{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{EXTENSIONS_LABEL}\"}}}}{{{{end}}}}"
        ),
    ]);
    Ok(parse_local_probe(&docker::probe(c)?))
}

fn parse_local_probe(out: &str) -> ImageProbe {
    let mut parts = out.split('|');
    let repo_digest = parts.next().unwrap_or_default();
    ImageProbe {
        digest: repo_digest.split('@').nth(1).map(str::to_string),
        version: non_empty(parts.next().unwrap_or_default().trim()),
        floor: parts.next().and_then(|f| Version::parse(f.trim())),
        extensions: parts.next().and_then(parse_extension_list),
    }
}

/// Split the comma-separated [`EXTENSIONS_LABEL`] value. An empty label is `None`
/// — "this image doesn't say", which every caller must treat as unknown rather
/// than as an image that ships no extensions at all.
fn parse_extension_list(raw: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    (!names.is_empty()).then_some(names)
}

/// Read the registry's digest and version label for the same tag, without
/// pulling. `.Image` is keyed by platform for a multi-arch tag, so the version is
/// collected by ranging over it (identical across arches — verified: two
/// differently-stamped builds share byte-identical layers) rather than by naming
/// one platform the host may not be.
///
/// Fails fast when the registry can't be reached at all: a TCP connect to it
/// (bounded by [`docker::CONNECTIVITY_TIMEOUT`]) runs first, and if that fails no
/// buildx process is spawned and the error is [`REGISTRY_OFFLINE`]. The probe
/// itself is bounded by [`docker::REMOTE_PROBE_TIMEOUT`] (manager#6). Every
/// registry read — the update check, the route walk's per-hop floor lookups, the
/// missing-extension guard — goes through here, so all of them inherit both.
fn remote_probe(image: &str) -> std::result::Result<ImageProbe, String> {
    let (host, port) = docker::registry_endpoint(image);
    remote_probe_with(image, || {
        docker::can_reach(&host, port, docker::CONNECTIVITY_TIMEOUT)
    })
}

/// [`remote_probe`] over an injected connectivity check, so the offline path is
/// testable without a network.
fn remote_probe_with(
    image: &str,
    online: impl FnOnce() -> bool,
) -> std::result::Result<ImageProbe, String> {
    if !online() {
        return Err(REGISTRY_OFFLINE.to_string());
    }
    let mut c = docker::docker();
    c.args([
        "buildx",
        "imagetools",
        "inspect",
        image,
        "--format",
        &format!(
            "{{{{.Manifest.Digest}}}}|\
             {{{{range $img := .Image}}}}{{{{if $img.Config.Labels}}}}\
             {{{{index $img.Config.Labels \"{VERSION_LABEL}\"}}}};\
             {{{{index $img.Config.Labels \"{FLOOR_LABEL}\"}}}};\
             {{{{index $img.Config.Labels \"{EXTENSIONS_LABEL}\"}}}}\
             {{{{end}}}}|{{{{end}}}}"
        ),
    ]);
    Ok(parse_remote_probe(&docker::probe_within(
        c,
        docker::REMOTE_PROBE_TIMEOUT,
    )?))
}

/// Split `digest|version;floor;extensions|version;floor;extensions|` — one triple
/// per platform in a multi-arch tag. The triples are identical across arches
/// (verified: two differently-stamped builds share byte-identical layers), so the
/// first populated one answers for the tag; ranging rather than naming a platform
/// keeps this working on a host whose arch we didn't guess. `;` is the separator
/// because none of the three values can contain one — the extension list is comma
/// separated.
fn parse_remote_probe(out: &str) -> ImageProbe {
    let mut parts = out.split('|');
    let digest = parts.next().unwrap_or_default();
    let (version, floor, extensions) = parts
        .map(|triple| {
            let mut fields = triple.splitn(3, ';');
            (
                non_empty(fields.next().unwrap_or_default().trim()),
                Version::parse(fields.next().unwrap_or_default().trim()),
                fields.next().and_then(parse_extension_list),
            )
        })
        .find(|(v, f, e)| v.is_some() || f.is_some() || e.is_some())
        .unwrap_or((None, None, None));
    ImageProbe {
        digest: non_empty(digest.trim()),
        version,
        floor,
        extensions,
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    // `compose config --format json` and `image inspect --format '{{json
    // .RepoDigests}}'`, verbatim (Docker 28, compose v2).
    const CONFIG_NEVER: &str = r#"{"name":"atelier-scn-cfgprobe","networks":{"default":{"name":"atelier-scn-cfgprobe_default","ipam":{}}},"services":{"app":{"command":null,"entrypoint":null,"image":"atelier-scn-app:x","networks":{"default":null},"pull_policy":"never"}}}"#;
    const CONFIG_PLAIN: &str = r#"{"name":"atelier-scn-cfgprobe","networks":{"default":{"name":"atelier-scn-cfgprobe_default","ipam":{}}},"services":{"app":{"command":null,"entrypoint":null,"image":"atelier-cms:zoo","networks":{"default":null}}}}"#;

    #[test]
    fn the_app_pull_policy_is_read_from_compose_config() {
        assert_eq!(
            super::parse_app_pull_policy(CONFIG_NEVER).as_deref(),
            Some("never")
        );
        assert_eq!(super::parse_app_pull_policy(CONFIG_PLAIN), None);
        assert_eq!(super::parse_app_pull_policy("not json"), None);
    }

    #[test]
    fn a_build_made_on_this_computer_has_nothing_to_download() {
        use super::is_local_only;
        let ghcr = "ghcr.io/aincient-labs/atelier-cms@sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f".to_string();
        // The zoo's shared tag: a retag of the downloaded release, whose only
        // digest is under the ghcr name. `docker.io/library/atelier-cms`
        // has none: refused.
        assert!(is_local_only(
            "atelier-cms:zoo-shared",
            None,
            Some(&[ghcr.clone()])
        ));
        // A developer build: no digests at all.
        assert!(is_local_only("atelier-cms:dev", None, Some(&[])));
        assert!(is_local_only("atelier-scn-app:x-app", None, Some(&[])));
        // Told to never pull (the harness's local builds): the update goes on.
        assert!(!is_local_only(
            "atelier-scn-app:x-app",
            Some("never"),
            Some(&[])
        ));
        // A registry reference is the registry's to answer, digests or not.
        assert!(!is_local_only(
            "ghcr.io/aincient-labs/atelier-cms:latest",
            None,
            Some(&[])
        ));
        assert!(!is_local_only(
            "localhost:5000/atelier-cms:x",
            None,
            Some(&[])
        ));
        // Downloaded from Docker Hub under this very name.
        assert!(!is_local_only(
            "nginx:1.30-alpine-slim",
            None,
            Some(&[
                "nginx@sha256:e2355aa95529a7999dad94ddcb45a8768abf6fb0b204c93b1a3ffce477d7b893"
                    .to_string()
            ])
        ));
        assert!(!is_local_only(
            "someone/atelier-fork:1",
            None,
            Some(&["someone/atelier-fork@sha256:aa".to_string()])
        ));
        // Not here at all: left to the pull. A digest reference never.
        assert!(!is_local_only("atelier-cms:gone", None, None));
        assert!(!is_local_only(&ghcr, None, Some(&[])));
    }

    #[test]
    fn the_local_build_refusal_says_why_and_the_way_back_to_a_channel() {
        let said = super::LocalBuild {
            image: "atelier-cms:zoo-shared".into(),
        }
        .to_string();
        assert!(said.contains("build made on this computer (atelier-cms:zoo-shared)"));
        assert!(said.contains("nothing to download"));
        assert!(said.contains("`atelier app channel stable`"));
        assert!(said.contains("nothing was changed"));
    }

    #[test]
    fn a_detached_container_is_reconnected_under_its_service_name() {
        let d = crate::boot::Detached {
            network: "atelier-review-tmp_default".into(),
            container: "atelier-review-tmp-db-1".into(),
            service: "db".into(),
            running: true,
        };
        assert_eq!(
            super::reattach_args(&d),
            [
                "network",
                "connect",
                "--alias",
                "db",
                "atelier-review-tmp_default",
                "atelier-review-tmp-db-1"
            ]
        );
    }

    #[test]
    fn start_action_falls_back_to_up_only_when_no_container() {
        assert_eq!(start_action(ContainerState::Missing), StartAction::Up);
        assert_eq!(start_action(ContainerState::Stopped), StartAction::Start);
        assert_eq!(start_action(ContainerState::Running), StartAction::Start);
        assert_eq!(start_action(ContainerState::Unknown), StartAction::Start);
    }

    #[test]
    fn a_start_never_recreates_a_container_even_after_a_compose_refresh() {
        // A refreshed compose.yaml once turned every Start into `up -d`, which
        // recreated app on whatever image the shared tag named by then.
        for state in [
            ContainerState::Running,
            ContainerState::Stopped,
            ContainerState::Unknown,
        ] {
            assert_eq!(
                start_plan(state, true),
                StartPlan {
                    action: StartAction::Start,
                    set_db_restart: true
                },
                "{state:?}"
            );
            assert!(!start_plan(state, false).set_db_restart);
        }
        // Only a missing container makes it an `up`, and that `up` creates the
        // missing ones only.
        assert_eq!(
            start_plan(ContainerState::Missing, true).action,
            StartAction::Up
        );
        assert_eq!(start_args(StartAction::Start), ["start"]);
        assert_eq!(start_args(StartAction::Up), ["up", "-d", "--no-recreate"]);
    }

    /// `docker compose ps -a --format json` lines as Compose 5.1 prints them
    /// (hardening-test, 2026-11-14; labels trimmed).
    const PS_DB: &str = r#"{"Command":"\"docker-entrypoint.s…\"","CreatedAt":"2026-11-14 12:47:23 +0100 CET","ExitCode":0,"Health":"healthy","ID":"561110aa7976","Image":"pgvector/pgvector:pg16-trixie","LocalVolumes":"1","Name":"atelier-hardening-test-db-1","Names":"atelier-hardening-test-db-1","Networks":"atelier-hardening-test_default","Ports":"5432/tcp","Project":"atelier-hardening-test","Service":"db","State":"running","Status":"Up 6 minutes (healthy)"}"#;
    const PS_APP: &str = r#"{"Command":"\"/usr/local/bin/entr…\"","CreatedAt":"2026-11-14 12:47:23 +0100 CET","ExitCode":0,"Health":"healthy","ID":"9772e359133d","Image":"ghcr.io/aincient-labs/atelier-cms:latest","LocalVolumes":"2","Name":"atelier-hardening-test-app-1","Names":"atelier-hardening-test-app-1","Networks":"atelier-hardening-test_default","Ports":"80/tcp","Project":"atelier-hardening-test","Service":"app","State":"running","Status":"Up 6 minutes (healthy)"}"#;
    const PS_EDGE: &str = r#"{"Command":"\"/docker-entrypoint.…\"","CreatedAt":"2026-11-14 12:47:23 +0100 CET","ExitCode":0,"Health":"","ID":"3ca00c913a8c","Image":"nginx:1.30-alpine-slim","LocalVolumes":"1","Name":"atelier-hardening-test-edge-1","Names":"atelier-hardening-test-edge-1","Networks":"atelier-hardening-test_default","Ports":"0.0.0.0:41227->80/tcp, [::]:41227->80/tcp","Project":"atelier-hardening-test","Service":"edge","State":"running","Status":"Up 6 minutes"}"#;

    fn stopped(line: &str) -> String {
        line.replace(r#""State":"running""#, r#""State":"exited""#)
    }

    #[test]
    fn container_state_from_compose_ps() {
        assert_eq!(container_state_from_ps(""), ContainerState::Missing);
        assert_eq!(container_state_from_ps("[]"), ContainerState::Missing);
        let all = format!("{PS_APP}\n{PS_DB}\n{PS_EDGE}");
        assert_eq!(container_state_from_ps(&all), ContainerState::Running);
        let all_stopped = format!(
            "{}\n{}\n{}",
            stopped(PS_APP),
            stopped(PS_DB),
            stopped(PS_EDGE)
        );
        assert_eq!(
            container_state_from_ps(&all_stopped),
            ContainerState::Stopped
        );
        assert_eq!(container_state_from_ps(PS_DB), ContainerState::Missing);
    }

    #[test]
    fn a_removed_db_or_edge_container_needs_up() {
        // `docker rm -f` of db alone: `compose start` started app and edge
        // and left the site with no database.
        let no_db = format!("{PS_APP}\n{PS_EDGE}");
        assert_eq!(container_state_from_ps(&no_db), ContainerState::Missing);
        assert_eq!(
            start_action(container_state_from_ps(&no_db)),
            StartAction::Up
        );
        let no_edge = format!("{}\n{}", stopped(PS_APP), stopped(PS_DB));
        assert_eq!(container_state_from_ps(&no_edge), ContainerState::Missing);
        // As a JSON array (older Compose) too.
        let array = format!("[{PS_APP},{PS_EDGE}]");
        assert_eq!(container_state_from_ps(&array), ContainerState::Missing);
    }

    #[test]
    fn an_erased_database_is_refused_only_for_a_site_backed_up_before() {
        let id = Some("5f0c2a7e-3b1d-4c8e-9a6f-2d7b1e4c9a03");
        assert!(database_erased(id, Some(false), false));
        // A first install has no site-id yet; an unanswered probe proves nothing.
        assert!(!database_erased(None, Some(false), false));
        assert!(!database_erased(id, None, false));
        assert!(!database_erased(id, Some(true), false));
    }

    #[test]
    fn a_database_the_owner_wiped_starts_fresh_until_it_exists_again() {
        let dir = Scratch::new("wiped-db");
        let stack = Stack::at(dir.0.clone());
        let id = stack.ensure_site_id().unwrap();
        // Lost by accident: refused.
        assert!(database_erased(
            Some(&id),
            Some(false),
            stack.database_wiped()
        ));
        // `app down --wipe` / "Remove and erase" leaves the marker first (and keeps
        // the site-id), so the next start sets up a fresh site instead.
        super::mark_database_wiped(&stack).unwrap();
        assert!(stack.database_wiped_path().ends_with(".database-wiped"));
        assert_eq!(stack.site_id().as_deref(), Some(id.as_str()));
        assert!(!database_erased(
            Some(&id),
            Some(false),
            stack.database_wiped()
        ));
        // Once the database is back the marker goes, and a later loss is refused again.
        super::clear_database_wiped(&stack);
        assert!(!stack.database_wiped());
        assert!(database_erased(
            Some(&id),
            Some(false),
            stack.database_wiped()
        ));
    }

    #[test]
    fn the_erased_database_refusal_names_the_latest_backup() {
        let dir = Scratch::new("erased-db");
        let stack = Stack::at(dir.0.clone());
        let backups = stack.backups_dir();
        std::fs::create_dir_all(&backups).unwrap();
        // Names exactly as `backup()` writes them; the newest wins whatever its label.
        let old = backups.join("aincient-before-update-20261113-091500.tar.gz");
        let new = backups.join("aincient-20261114-101010.tar.gz");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&new, b"x").unwrap();
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(earlier)
            .unwrap();

        let refusal = DatabaseErased {
            latest_backup: list_backups(&stack).into_iter().next(),
        };
        let text = refusal.to_string();
        assert!(
            text.starts_with("Your site's database was removed from Docker. Restore your latest backup (Backups tab): aincient-20261114-101010.tar.gz."),
            "{text}"
        );
        assert!(text.contains(&new.display().to_string()), "{text}");

        let none = DatabaseErased {
            latest_backup: None,
        }
        .to_string();
        assert!(
            none.contains("no backup of this site on this computer"),
            "{none}"
        );
    }

    #[test]
    fn only_no_such_image_fetches_the_engine_again() {
        // `docker image inspect` stderr, Docker 29.4.
        let gone = "Error response from daemon: No such image: ghcr.io/aincient-labs/atelier-engine@sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f";
        assert!(!engine_image_present(Err(gone.into())).unwrap());
        assert!(engine_image_present(Ok("sha256:435e2d22".into())).unwrap());
        let down = "failed to connect to the docker API at unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /var/run/docker.sock: connect: no such file or directory";
        let e = engine_image_present(Err(down.into())).unwrap_err();
        assert!(format!("{e:#}").contains("failed to connect"), "{e:#}");
    }

    /// When the backups in the prune tests started counting: before all of them.
    const SINCE: Option<&str> = Some("20261101-000000");

    #[test]
    fn only_automatic_backups_beyond_the_newest_three_are_pruned() {
        // Names exactly as `backup()` writes them (review 2026-11-14: the old
        // test used names without the `aincient-` prefix, so it passed while
        // the real prune never matched anything).
        let name = |label: Option<&str>, ts: &str| format!("{}.tar.gz", backup_stem(label, ts));
        assert_eq!(
            name(Some("before-update"), "20261101-100000"),
            "aincient-before-update-20261101-100000.tar.gz"
        );
        let b = |name: String, t: u64| Backup {
            path: format!("/b/{name}"),
            name,
            size_bytes: 1,
            modified_unix: t,
        };
        let all = vec![
            b(name(Some("before-update"), "20261101-100000"), 1),
            b(name(Some("before-restore"), "20261102-100000"), 2),
            b(name(Some("before-update"), "20261103-100000"), 3),
            b(name(Some("before-stepped-upgrade"), "20261104-100000"), 4),
            // A second backup in the same second, as `claim_name` names it.
            b("aincient-before-update-20261105-100000-2.tar.gz".into(), 5),
            b(name(Some("before-channel-switch"), "20261106-100000"), 6),
            // The owner's own: never pruned, however old.
            b(name(None, "20261001-090000"), 0),
            b(name(Some("before-redesign"), "20261001-090000"), 0),
            b(name(Some("before update notes"), "20261001-090000"), 0),
            b("aincient-before-update-notes.tar.gz".into(), 0),
            b("aincient-before-update-éé-20261101.tar.gz".into(), 0),
            // A file brought in from elsewhere, named like ours without the prefix.
            b("before-update-20261001-090000.tar.gz".into(), 0),
        ];
        let gone: Vec<&str> = automatic_backups_to_prune(&all, 3, None, SINCE)
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(
            gone,
            [
                "aincient-before-update-20261103-100000.tar.gz",
                "aincient-before-restore-20261102-100000.tar.gz",
                "aincient-before-update-20261101-100000.tar.gz"
            ]
        );
        assert!(automatic_backups_to_prune(&all[..3], 3, None, SINCE).is_empty());
    }

    #[test]
    fn the_backup_a_restore_loaded_is_never_pruned_however_its_path_is_spelled() {
        // `data restore ./backups/x` or a path through a symlinked folder names
        // the same file as the backup list's absolute path.
        let dir = Scratch::new("prune-protect");
        let backups = dir.0.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let all: Vec<Backup> = (1..=4)
            .map(|d| {
                let name = format!(
                    "{}.tar.gz",
                    backup_stem(Some("before-update"), &format!("2026110{d}-100000"))
                );
                let path = backups.join(&name);
                std::fs::write(&path, "x").unwrap();
                Backup {
                    name,
                    path: path.to_string_lossy().into_owned(),
                    size_bytes: 1,
                    modified_unix: d,
                }
            })
            .collect();
        let oldest = "aincient-before-update-20261101-100000.tar.gz";
        let spelled = backups.join("..").join("backups").join(oldest);
        assert_ne!(spelled, Path::new(&all[0].path));
        assert!(automatic_backups_to_prune(&all, 3, Some(&spelled), SINCE).is_empty());
        assert_eq!(
            automatic_backups_to_prune(&all, 3, None, SINCE)[0].name,
            oldest,
            "without the protect it is the one pruned"
        );
    }

    #[test]
    fn a_failed_safety_backup_stops_the_restore_and_names_the_cause() {
        let err = require_safety_backup(Err(anyhow::anyhow!(
            "failed to save the backup as /home/x/.atelier/backups/aincient-before-restore-20261114-101500.tar.gz: Permission denied (os error 13)"
        )))
        .unwrap_err();
        let typed = err.downcast_ref::<SafetyBackupFailed>().expect("typed");
        assert!(typed.cause.contains("Permission denied"));
        let shown = format!("{err:#}");
        assert!(shown.contains("nothing was restored"), "{shown}");
        assert!(shown.contains("Permission denied"), "{shown}");
        // A stopped site has no running database to lose: the restore goes on.
        assert_eq!(require_safety_backup(Ok(None)).unwrap(), None);
        let saved = PathBuf::from("/b/aincient-before-restore-20261114-101500.tar.gz");
        assert_eq!(
            require_safety_backup(Ok(Some(saved.clone()))).unwrap(),
            Some(saved)
        );
    }

    /// Backups as `backup()` names them, in a site's backups folder.
    fn backups_in(stack: &Stack, names: &[&str]) {
        let dir = stack.backups_dir();
        std::fs::create_dir_all(&dir).unwrap();
        for n in names {
            std::fs::write(dir.join(n), "x").unwrap();
        }
    }

    fn names_left(stack: &Stack) -> Vec<String> {
        let mut v: Vec<String> = list_backups(stack).into_iter().map(|b| b.name).collect();
        v.sort();
        v
    }

    // Automatic backups an earlier Manager took and never pruned (B6), and the
    // owner's own: the same file names `backup()` writes.
    const OLD_STEPPED: &str = "aincient-before-stepped-upgrade-20260101-120000.tar.gz";
    const OLD_SWITCH: &str = "aincient-before-channel-switch-20260301-090000.tar.gz";
    const OLD_UPDATE: &str = "aincient-before-update-20261020-080000.tar.gz";
    const OWNERS: &str = "aincient-20250101-000000.tar.gz";
    const OWNERS_LABELLED: &str = "aincient-before-redesign-20250101-000000.tar.gz";

    #[test]
    fn automatic_backups_from_before_counting_started_are_never_pruned() {
        let dir = Scratch::new("prune-since");
        let stack = Stack::at(dir.0.clone());
        let new = [
            "aincient-before-update-20261114-100000.tar.gz",
            "aincient-before-restore-20261114-110000.tar.gz",
            "aincient-before-update-20261114-120000.tar.gz",
            "aincient-before-update-20261114-130000.tar.gz",
        ];
        backups_in(
            &stack,
            &[OLD_STEPPED, OLD_SWITCH, OLD_UPDATE, OWNERS, OWNERS_LABELLED],
        );
        backups_in(&stack, &new);

        // No start recorded yet (a site whose first 0.17 update took no backup):
        // nothing goes, and counting starts now.
        super::prune_automatic_backups(&stack, None, &mut Silent);
        assert_eq!(names_left(&stack).len(), 9);
        let since = super::automatic_backups_since(&stack).expect("recorded");
        assert!(since.as_str() > "20261114-130000", "{since}");

        // As `backup()` records it: the first automatic backup's own stamp.
        std::fs::remove_file(stack.home.join(super::AUTOMATIC_SINCE_FILE)).unwrap();
        super::note_automatic_backups_since(&stack, "20261114-100000");
        // Never moved by a later one.
        super::note_automatic_backups_since(&stack, "20261114-130000");
        assert_eq!(
            super::automatic_backups_since(&stack).as_deref(),
            Some("20261114-100000")
        );
        super::prune_automatic_backups(&stack, None, &mut Silent);
        let left = names_left(&stack);
        // Only the oldest of the four counted ones; every older one stays.
        assert!(!left.contains(&new[0].to_string()), "{left:?}");
        for keep in [OLD_STEPPED, OLD_SWITCH, OLD_UPDATE, OWNERS, OWNERS_LABELLED] {
            assert!(left.contains(&keep.to_string()), "{keep} gone: {left:?}");
        }
        assert_eq!(left.len(), 8);
    }

    #[test]
    fn free_up_space_offers_only_older_automatic_backups() {
        let dir = Scratch::new("older-backups");
        let stack = Stack::at(dir.0.clone());
        backups_in(
            &stack,
            &[
                OLD_STEPPED,
                OLD_SWITCH,
                OLD_UPDATE,
                OWNERS,
                OWNERS_LABELLED,
                "aincient-before-update-20261114-100000.tar.gz",
                "aincient-before-update-20261114-100000-2.tar.gz",
            ],
        );
        let names = |v: Vec<Backup>| {
            let mut n: Vec<String> = v.into_iter().map(|b| b.name).collect();
            n.sort();
            n
        };
        // Before counting started, every automatic backup is an older one.
        assert_eq!(
            names(super::older_backups(&stack)),
            [
                "aincient-before-channel-switch-20260301-090000.tar.gz",
                "aincient-before-stepped-upgrade-20260101-120000.tar.gz",
                "aincient-before-update-20261020-080000.tar.gz",
                "aincient-before-update-20261114-100000-2.tar.gz",
                "aincient-before-update-20261114-100000.tar.gz",
            ]
        );
        // Once it has, only those before it: never a counted one.
        super::note_automatic_backups_since(&stack, "20261114-100000");
        assert_eq!(
            names(super::older_backups(&stack)),
            [OLD_SWITCH, OLD_STEPPED, OLD_UPDATE]
        );
    }

    #[test]
    fn an_update_target_the_site_has_moved_past_is_dropped() {
        use super::{live_update_target_with, tag_version, waypoint_image};
        let v = |s: &str| Version::parse(s).unwrap();
        assert_eq!(tag_version(&waypoint_image(v("0.21.0"))), Some(v("0.21.0")));
        assert_eq!(
            tag_version("ghcr.io/aincient-labs/atelier-cms:latest"),
            None
        );
        assert_eq!(tag_version("localhost:5000/atelier-cms"), None);
        assert_eq!(
            tag_version("ghcr.io/aincient-labs/atelier-cms@sha256:0f93c730bf54a025610ea0c4fafb1a80012773152cd22ce5227bd968e6e96bae"),
            None
        );

        let dir = Scratch::new("stale-target");
        let stack = Stack::at(dir.0.clone());
        let no_registry = |_: &str| -> Option<Version> { panic!("tag says it") };
        // A route to 0.20.0, and the site now runs 0.21.0 (a restore, an install).
        stack
            .set_update_target(&waypoint_image(v("0.20.0")))
            .unwrap();
        assert_eq!(
            live_update_target_with(&stack, Some("0.21.0"), no_registry),
            None
        );
        assert!(stack.update_target().is_none(), "deleted");
        // Reached exactly: done.
        stack
            .set_update_target(&waypoint_image(v("0.21.0")))
            .unwrap();
        assert_eq!(
            live_update_target_with(&stack, Some("v0.21.0"), no_registry),
            None
        );
        // Still ahead: kept.
        let ahead = waypoint_image(v("0.22.0"));
        stack.set_update_target(&ahead).unwrap();
        assert_eq!(
            live_update_target_with(&stack, Some("0.21.0"), no_registry),
            Some(ahead.clone())
        );
        // Unknown running version (an `edge+sha` build, no container): kept.
        assert_eq!(
            live_update_target_with(&stack, Some("edge+1a2b3c4"), no_registry),
            Some(ahead.clone())
        );
        assert_eq!(
            live_update_target_with(&stack, None, no_registry),
            Some(ahead)
        );
        // A channel target: its version comes from the registry.
        let channel = "ghcr.io/aincient-labs/atelier-cms:latest";
        stack.set_update_target(channel).unwrap();
        assert_eq!(
            live_update_target_with(&stack, Some("0.21.0"), |_| Some(v("0.22.0"))),
            Some(channel.to_string())
        );
        assert_eq!(
            live_update_target_with(&stack, Some("0.21.0"), |_| None),
            Some(channel.to_string())
        );
        assert_eq!(
            live_update_target_with(&stack, Some("0.21.0"), |_| Some(v("0.21.0"))),
            None
        );
        assert!(stack.update_target().is_none());
    }

    #[test]
    fn a_site_woken_from_rest_rests_again_whether_the_update_worked_or_not() {
        use super::{after_wake, AfterWake, Woke};
        assert_eq!(after_wake(Woke::FromRest, Ok(true)), AfterWake::Rest);
        // The failed update (review 2026-11-14: it stayed awake).
        assert_eq!(after_wake(Woke::FromRest, Err(())), AfterWake::Rest);
        // Still converging: resting it now would cut that short.
        assert_eq!(after_wake(Woke::FromRest, Ok(false)), AfterWake::StayAwake);
        // A site that wasn't resting is never rested.
        for outcome in [Ok(true), Ok(false), Err(())] {
            assert_eq!(after_wake(Woke::No, outcome), AfterWake::Leave);
            assert_eq!(after_wake(Woke::Started, outcome), AfterWake::Leave);
        }
    }

    #[test]
    fn an_update_without_a_backup_does_not_start_the_site_but_rests_it_again() {
        use super::{woke_without_start, Woke};
        let stopped = format!(
            "{}\n{}\n{}",
            stopped(PS_APP),
            stopped(PS_DB),
            stopped(PS_EDGE)
        );
        let state = container_state_from_ps(&stopped);
        assert_eq!(woke_without_start(state, Some("resting")), Woke::FromRest);
        assert_eq!(woke_without_start(state, None), Woke::Started);
        let running = format!("{PS_APP}\n{PS_DB}\n{PS_EDGE}");
        assert_eq!(
            woke_without_start(container_state_from_ps(&running), Some("awake")),
            Woke::No
        );
    }

    #[test]
    fn a_running_site_whose_backup_fails_is_not_updated() {
        let e = super::backup_or_refuse::<()>(Err(anyhow::anyhow!(
            "failed to copy the snapshot out of the container: write /backups/x.part: no space left on device"
        )))
        .unwrap_err();
        let refusal = e
            .downcast_ref::<super::BackupBeforeUpdateFailed>()
            .expect("a typed refusal the app offers \"Update without a backup\" for");
        assert!(refusal.cause.contains("no space left on device"));
        assert!(e.to_string().ends_with("so it wasn't updated"));
        assert_eq!(super::backup_or_refuse(Ok(7)).unwrap(), 7);
    }

    #[test]
    fn refusals_with_an_override_are_told_apart_by_type_and_exit_code() {
        use super::{BackupBeforeUpdateFailed, Overridable};
        use anyhow::Context;
        // As `start_for_backup` builds it: the cause stays readable (a failed
        // boot that needs Docker restarted keeps its button).
        let e = Err::<(), _>(anyhow::Error::new(crate::boot::BootFailed(
            "The database didn't start.".into(),
        )))
        .context(BackupBeforeUpdateFailed {
            cause: "The site is stopped and couldn't be started to back it up first".into(),
        })
        .unwrap_err();
        assert_eq!(Overridable::of(&e), Some(Overridable::UpdateWithoutBackup));
        assert!(e.downcast_ref::<crate::boot::BootFailed>().is_some());
        assert_eq!(
            format!("{e:#}"),
            "The site is stopped and couldn't be started to back it up first, so it wasn't \
             updated: The database didn't start."
        );
        let e = require_safety_backup(Err(anyhow::anyhow!("No space left on device"))).unwrap_err();
        assert_eq!(
            Overridable::of(&e),
            Some(Overridable::RestoreWithoutSafetyBackup)
        );
        // A server's `atelier` says it by its exit code.
        for o in [
            Overridable::RestoreWithoutSafetyBackup,
            Overridable::UpdateWithoutBackup,
        ] {
            let remote = anyhow::Error::new(crate::remote::RemoteExit {
                host: "deploy@example.com".into(),
                what: "`atelier app update`".into(),
                code: o.exit_code(),
            });
            assert_eq!(Overridable::of(&remote), Some(o));
        }
        let other = anyhow::Error::new(crate::remote::RemoteExit {
            host: "deploy@example.com".into(),
            what: "`atelier app update`".into(),
            code: 1,
        });
        assert_eq!(Overridable::of(&other), None);
        assert_eq!(Overridable::of(&anyhow::anyhow!("anything else")), None);
    }

    #[test]
    fn a_missing_pinned_engine_is_fetched_by_digest_and_a_missing_local_build_is_named() {
        // The owner's test, 2026-11-14: the Engine POC's pinned engine was gone.
        let digest = "ghcr.io/aincient-labs/atelier-engine@sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f";
        assert_eq!(
            engine_image_need(Some(digest), false),
            EngineImage::Fetch(digest.into())
        );
        assert_eq!(engine_image_need(Some(digest), true), EngineImage::Ready);
        let local = "sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f";
        assert_eq!(
            engine_image_need(Some(local), false),
            EngineImage::LocalBuildGone
        );
        assert_eq!(engine_image_need(Some(local), true), EngineImage::Ready);
        assert_eq!(engine_image_need(None, false), EngineImage::Ready);
        assert!(LOCAL_ENGINE_GONE.contains("Turn the engine off and on again"));
    }

    #[test]
    fn only_a_stopped_container_still_holding_its_endpoint_is_recovered() {
        // The 2026-11-14 update: db exited, its endpoint left in atelier_default,
        // and the new app container created but not running.
        let ps = "{\"Name\":\"atelier-db-1\",\"Service\":\"db\",\"State\":\"exited\"}\n\
                  {\"Name\":\"atelier-app-1\",\"Service\":\"app\",\"State\":\"created\"}\n\
                  {\"Name\":\"atelier-edge-1\",\"Service\":\"edge\",\"State\":\"exited\"}";
        // Every service: edge (and the engine) can be left half attached too.
        assert_eq!(
            stopped_service_containers(ps),
            ["atelier-db-1", "atelier-app-1", "atelier-edge-1"]
        );
        let engine = format!(
            "{PS_APP}\n{}",
            stopped(PS_EDGE)
                .replace("edge", "engine")
                .replace("nginx:1.30-alpine-slim", "atelier-engine")
        );
        assert_eq!(
            stopped_service_containers(&engine),
            ["atelier-hardening-test-engine-1"]
        );
        assert!(stopped_service_containers(
            r#"[{"Name":"atelier-db-1","Service":"db","State":"running"}]"#
        )
        .is_empty());
        // Only a container the network still lists is a leftover; a cleanly
        // stopped one holds no endpoint, so `compose start` errors stay as they are.
        assert!(holds_endpoint(
            "atelier-edge-1\natelier-db-1\n",
            "atelier-db-1"
        ));
        assert!(!holds_endpoint("atelier-edge-1\n", "atelier-db-1"));
        assert!(!holds_endpoint("atelier-db-10\n", "atelier-db-1"));
        assert!(START_STUCK.contains("Restart Docker (`atelier docker restart`)"));
        assert!(!START_STUCK.contains("Docker Desktop"));
        assert!(START_STUCK.contains("data is safe"));
    }

    /// A scripted engine site for [`super::wake_recovering`]: each Wake takes
    /// the next answer; the rest is what Docker shows.
    struct FakeWake {
        wakes: std::collections::VecDeque<std::result::Result<&'static str, &'static str>>,
        stale: Vec<(String, String)>,
        ps: String,
        up: std::result::Result<(), &'static str>,
        calls: Vec<String>,
    }

    impl FakeWake {
        fn new(wakes: &[std::result::Result<&'static str, &'static str>], ps: String) -> Self {
            FakeWake {
                wakes: wakes.iter().copied().collect(),
                stale: Vec::new(),
                ps,
                up: Ok(()),
                calls: Vec::new(),
            }
        }
    }

    impl super::WakeSteps for FakeWake {
        fn wake(&mut self) -> Result<String> {
            self.calls.push("wake".into());
            match self.wakes.pop_front().expect("an unscripted wake") {
                Ok(s) => Ok(s.to_string()),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
        fn stale_endpoints(&mut self) -> Vec<(String, String)> {
            self.calls.push("stale".into());
            self.stale.clone()
        }
        fn clear(&mut self, stale: &[(String, String)], _r: &mut dyn Reporter) {
            for (n, c) in stale {
                self.calls.push(format!("disconnect {n} {c}"));
            }
        }
        fn containers(&mut self) -> ContainerState {
            self.calls.push("ps".into());
            container_state_from_ps(&self.ps)
        }
        fn up(&mut self, _r: &mut dyn Reporter) -> Result<()> {
            self.calls.push("up -d".into());
            self.up.map_err(|e| anyhow::anyhow!(e))
        }
    }

    const WOKEN: &str = "Woken: db and app started.";
    /// The engine's `compose start db app` on the 2026-11-14 leftover.
    const WAKE_STALE: &str = "start the site: Error response from daemon: endpoint with name \
        atelier-hardening-test-db-1 already exists in network atelier-hardening-test_default";
    /// The engine's `compose start db app` with db's container removed.
    const WAKE_NO_DB: &str = "start the site: service \"db\" has no container to start";

    /// What a resting engine site's `compose ps -a` lists: db and app stopped.
    fn resting_ps() -> String {
        format!("{}\n{}\n{PS_EDGE}", stopped(PS_DB), stopped(PS_APP))
    }

    #[test]
    fn a_wake_that_works_is_not_touched() {
        let mut f = FakeWake::new(&[Ok(WOKEN)], resting_ps());
        super::wake_recovering(&mut f, &mut Silent).unwrap();
        assert_eq!(f.calls, ["wake"]);
    }

    #[test]
    fn a_wake_stuck_on_a_leftover_endpoint_clears_it_and_wakes_again() {
        let mut f = FakeWake::new(&[Err(WAKE_STALE), Ok(WOKEN)], resting_ps());
        f.stale = vec![(
            "atelier-hardening-test_default".into(),
            "atelier-hardening-test-db-1".into(),
        )];
        super::wake_recovering(&mut f, &mut Silent).unwrap();
        assert_eq!(
            f.calls,
            [
                "wake",
                "stale",
                "disconnect atelier-hardening-test_default atelier-hardening-test-db-1",
                "wake"
            ]
        );
    }

    #[test]
    fn a_wake_still_stuck_after_clearing_says_restart_docker_once() {
        let mut f = FakeWake::new(&[Err(WAKE_STALE), Err(WAKE_STALE)], resting_ps());
        f.stale = vec![(
            "atelier-hardening-test_default".into(),
            "atelier-hardening-test-db-1".into(),
        )];
        let e = format!(
            "{:#}",
            super::wake_recovering(&mut f, &mut Silent).unwrap_err()
        );
        assert!(e.starts_with(START_STUCK), "{e}");
        assert!(e.contains("already exists in network"), "{e}");
        let mut again = FakeWake::new(&[Err(WAKE_STALE), Err(WAKE_STALE)], resting_ps());
        again.stale = f.stale.clone();
        let typed = super::wake_recovering(&mut again, &mut Silent).unwrap_err();
        assert!(
            crate::docker_runtime::needs_docker_restart(&typed),
            "{typed:#}"
        );
        // One retry, never a loop; the containers all exist, so no `up`.
        assert_eq!(f.calls.iter().filter(|c| *c == "wake").count(), 2);
        assert!(!f.calls.contains(&"up -d".to_string()));
    }

    #[test]
    fn a_wake_with_a_container_gone_creates_it_and_wakes_through_the_engine() {
        // `docker rm` of db on a resting site: only app and edge are listed.
        let ps = format!("{}\n{PS_EDGE}", stopped(PS_APP));
        let mut f = FakeWake::new(&[Err(WAKE_NO_DB), Ok(WOKEN)], ps);
        super::wake_recovering(&mut f, &mut Silent).unwrap();
        // The last Wake takes the engine's resting mark off, which its failed
        // Wake put back on, so edge stops serving the resting copy.
        assert_eq!(f.calls, ["wake", "stale", "ps", "up -d", "wake"]);
    }

    #[test]
    fn a_wake_with_a_leftover_and_a_container_gone_recovers_both() {
        // db removed, app's endpoint left behind: clearing alone isn't enough.
        let ps = format!("{}\n{PS_EDGE}", stopped(PS_APP));
        let mut f = FakeWake::new(&[Err(WAKE_STALE), Err(WAKE_NO_DB), Ok(WOKEN)], ps);
        f.stale = vec![(
            "atelier-hardening-test_default".into(),
            "atelier-hardening-test-app-1".into(),
        )];
        super::wake_recovering(&mut f, &mut Silent).unwrap();
        assert_eq!(
            f.calls,
            [
                "wake",
                "stale",
                "disconnect atelier-hardening-test_default atelier-hardening-test-app-1",
                "wake",
                "ps",
                "up -d",
                "wake"
            ]
        );
    }

    #[test]
    fn a_wake_with_nothing_to_recover_keeps_the_engines_error() {
        let mut f = FakeWake::new(&[Err("start the site: timed out")], resting_ps());
        let e = format!(
            "{:#}",
            super::wake_recovering(&mut f, &mut Silent).unwrap_err()
        );
        assert_eq!(e, "start the site: timed out");
        assert_eq!(f.calls, ["wake", "stale", "ps"]);
    }

    #[test]
    fn a_locked_down_site_is_not_recovered() {
        let ps = format!("{}\n{PS_EDGE}", stopped(PS_APP));
        let mut f = FakeWake::new(&[Err("locked down: unlock first")], ps);
        let e = format!(
            "{:#}",
            super::wake_recovering(&mut f, &mut Silent).unwrap_err()
        );
        assert!(e.contains("locked down"), "{e}");
        assert_eq!(f.calls, ["wake"]);
    }

    #[test]
    fn a_failed_up_after_a_failed_wake_says_why() {
        let ps = format!("{}\n{PS_EDGE}", stopped(PS_APP));
        let mut f = FakeWake::new(&[Err(WAKE_NO_DB)], ps);
        f.up = Err("Your site's database was removed from Docker.");
        let e = format!(
            "{:#}",
            super::wake_recovering(&mut f, &mut Silent).unwrap_err()
        );
        assert!(e.contains("database was removed"), "{e}");
        assert_eq!(f.calls, ["wake", "stale", "ps", "up -d"]);
    }

    use super::{
        automatic_backups_to_prune, backup_dump_script, backup_pack_script,
        container_state_from_ps, holds_endpoint, is_backup_file, is_snapshot_bundle, list_backups,
        parse_extension_list, parse_http_status, parse_local_probe, parse_remote_probe,
        parse_serialized_extensions, plan_route, plan_route_with, registry_problem,
        remote_probe_with, restore_bundle_scripts, start_action, stopped_service_containers,
        waypoint_image, Backup, ContainerState, ImageProbe, SnapshotManifest, StartAction, Version,
        MAX_WAYPOINTS, REGISTRY_OFFLINE, START_STUCK,
    };
    use super::{backup_stem, require_safety_backup, SafetyBackupFailed};
    use super::{
        check_identity, describe_backup, restore_identity, restore_with, BackupInfo, IdentityCheck,
        IdentityMismatch, RestoreOptions, Silent,
    };
    use super::{database_erased, engine_image_present, DatabaseErased};
    use super::{engine_image_need, EngineImage, LOCAL_ENGINE_GONE};
    use super::{
        lifecycle_plan, Plan, PlanStep, Reporter, ScaledReporter, Stage, BOOT_FROM, PULL_FROM,
        UP_FROM,
    };
    use super::{
        pull_then_move_db, refuse_locked, registry_refused, route_target, settings_changed,
        start_for_backup_needed, switch_channel, update_need, UpdateCheck,
    };
    use super::{start_args, start_plan, StartPlan};
    use crate::stack::Stack;
    use crate::stack::{Channel, InstallOptions};
    use anyhow::{bail, Result};
    use std::path::{Path, PathBuf};

    /// A reporter that answers the migration question however the test wants, and
    /// remembers whether it was asked at all.
    struct Answering {
        answer: bool,
        asked: Vec<String>,
    }
    impl super::Reporter for Answering {
        fn confirm(&mut self, question: &str) -> bool {
            self.asked.push(question.to_string());
            self.answer
        }
    }

    /// An install on the legacy default image, with no chosen channel — the exact
    /// shape that `pending_default_channel_migration` fires on.
    fn legacy_install(home: &std::path::Path) -> Stack {
        std::fs::create_dir_all(home).unwrap();
        let stack = Stack::at(home.to_path_buf());
        std::fs::write(stack.compose_path(), crate::stack::COMPOSE_TEMPLATE).unwrap();
        std::fs::write(
            stack.env_path(),
            format!(
                "HASH_SALT={}\nAINCIENT_IMAGE={}\nHTTP_PORT=41221\n",
                "a".repeat(64),
                crate::stack::LEGACY_DEFAULT_IMAGE
            ),
        )
        .unwrap();
        stack
    }

    /// An install configured for `image`, for exercising the paths that read and
    /// write `AINCIENT_IMAGE`.
    fn install_on(home: &std::path::Path, image: &str) -> Stack {
        std::fs::create_dir_all(home).unwrap();
        let stack = Stack::at(home.to_path_buf());
        std::fs::write(stack.compose_path(), crate::stack::COMPOSE_TEMPLATE).unwrap();
        std::fs::write(
            stack.env_path(),
            format!(
                "HASH_SALT={}\nAINCIENT_IMAGE={image}\nHTTP_PORT=41221\n",
                "a".repeat(64),
            ),
        )
        .unwrap();
        stack
    }

    /// A one-step plan onto `target`, as `plan_upgrade` shapes it.
    fn single_hop_plan(target: &str, explicit_target: bool) -> super::UpgradePlan {
        super::UpgradePlan {
            from: None,
            target_image: target.to_string(),
            steps: vec![super::UpgradeStep {
                image: target.to_string(),
                version: Version::parse("0.2.0"),
                is_target: true,
                reason: None,
            }],
            problem: None,
            explicit_target,
        }
    }

    /// The Downloads incident (Sept 2026): the GUI passed the folder the user
    /// *picked* as the export target and the engine `remove_dir_all`'d it. The
    /// engine must refuse any populated folder that isn't a previous export.
    #[test]
    fn export_refuses_a_populated_folder_that_is_not_an_export() {
        let dir = std::env::temp_dir().join(format!("atelier-export-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tax-return.pdf"), b"precious").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"").unwrap();

        let err = super::check_export_target(&dir).unwrap_err().to_string();
        assert!(err.contains("Refusing"), "{err}");
        assert!(
            dir.join("tax-return.pdf").is_file(),
            "the guard must not touch anything"
        );

        // An `install_export` onto it must also refuse, even with a valid staged copy.
        let staged = dir.with_extension("staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join(super::EXPORT_MARKER), b"{}").unwrap();
        assert!(super::install_export(&staged, &dir).is_err());
        assert!(dir.join("tax-return.pdf").is_file());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&staged).ok();
    }

    /// Absent, empty (a stray `.DS_Store` doesn't count), and marker-bearing
    /// folders are all fine; a previous export is replaced by the staged one.
    #[test]
    fn export_replaces_only_a_previous_export() {
        let base = std::env::temp_dir().join(format!("atelier-export-swap-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();

        assert!(super::check_export_target(&base.join("absent")).is_ok());
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join(".DS_Store"), b"").unwrap();
        assert!(super::check_export_target(&empty).is_ok());

        let target = base.join("site");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(super::EXPORT_MARKER), b"{}").unwrap();
        std::fs::write(target.join("old.html"), b"old").unwrap();
        let staged = super::staging_sibling(&target);
        assert_eq!(staged.parent(), target.parent());
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join(super::EXPORT_MARKER), b"{}").unwrap();
        std::fs::write(staged.join("index.html"), b"new").unwrap();

        super::install_export(&staged, &target).unwrap();
        assert!(target.join("index.html").is_file());
        assert!(!target.join("old.html").exists());
        assert!(!staged.exists());

        // A staged copy without the marker never replaces anything.
        let bad = super::staging_sibling(&target);
        std::fs::create_dir_all(&bad).unwrap();
        assert!(super::install_export(&bad, &target).is_err());
        assert!(target.join("index.html").is_file());

        std::fs::remove_dir_all(&base).ok();
    }

    /// The v0.6.0 rescue-stack incident: `update --to 0.2.0` on an install
    /// configured for `:latest` planned `:v0.2.0` but pulled and pinned `:latest`,
    /// because the single-hop path never wrote the target. The named image must be
    /// configured before anything runs.
    #[test]
    fn an_explicit_single_hop_target_is_written_before_it_runs() {
        let home = std::env::temp_dir().join(format!("atelier-retarget-{}", std::process::id()));
        let stack = install_on(&home, "ghcr.io/aincient-labs/atelier-cms:latest");
        let plan = single_hop_plan("ghcr.io/aincient-labs/atelier-cms:v0.2.0", true);

        super::retarget_single_hop(&stack, &plan).unwrap();

        assert_eq!(
            stack.env_get("AINCIENT_IMAGE").as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms:v0.2.0"),
            "the image the operator asked for must be the one that runs"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A channel plan targets `prospective_image()`, which on a legacy install is
    /// the migration the operator gets asked about — and may decline. Applying the
    /// plan must not write that target behind the decline.
    #[test]
    fn a_channel_plan_never_rewrites_the_configured_image() {
        let home = std::env::temp_dir().join(format!("atelier-no-retarget-{}", std::process::id()));
        let stack = install_on(&home, crate::stack::LEGACY_DEFAULT_IMAGE);
        let plan = single_hop_plan("ghcr.io/aincient-labs/atelier-cms:v0.7.1", false);

        super::retarget_single_hop(&stack, &plan).unwrap();

        assert_eq!(
            stack.env_get("AINCIENT_IMAGE").as_deref(),
            Some(crate::stack::LEGACY_DEFAULT_IMAGE),
            "a target nobody named must never overwrite the configured image"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The incident's mechanism, under the operator's control: declining leaves the
    /// install on the image it is already running, and writes nothing.
    #[test]
    fn declining_the_channel_migration_changes_nothing() {
        let home = std::env::temp_dir().join(format!("atelier-confirm-no-{}", std::process::id()));
        let stack = legacy_install(&home);
        let mut r = Answering {
            answer: false,
            asked: vec![],
        };

        super::announce_legacy_migrations(&stack, &mut r).unwrap();

        assert_eq!(r.asked.len(), 1, "the operator must actually be asked");
        assert_eq!(
            stack.env_get("AINCIENT_IMAGE").as_deref(),
            Some(crate::stack::LEGACY_DEFAULT_IMAGE),
            "declining must leave the image alone"
        );
        assert!(
            stack.chosen_channel().is_none(),
            "declining is not a channel choice — it must stay askable"
        );
        assert!(
            stack.pending_default_channel_migration(),
            "the migration is deferred, not cancelled"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A reporter that was never taught to ask cannot answer for the operator.
    #[test]
    fn a_reporter_that_cannot_ask_declines() {
        use super::Reporter;
        assert!(!super::Silent.confirm("switch channels?"));
    }

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

    /// The comparison the pre-pull guard makes, isolated from Docker: which
    /// installed extensions does the target no longer ship? Intersecting with the
    /// CURRENT image's list first is what keeps core out of it — both labels are
    /// non-core, so a core module is in neither and can never look missing.
    fn missing_for(installed: &[&str], current: &[&str], target: &[&str]) -> Vec<String> {
        installed
            .iter()
            .filter(|e| current.contains(e) && !target.contains(e))
            .map(|e| e.to_string())
            .collect()
    }

    #[test]
    fn the_pre_pull_guard_names_dropped_modules_and_ignores_core() {
        // The 2026-08-06 shape: `ai`/`ai_agents` installed, still in the running
        // image, gone from the target. `node` is core — in neither label.
        let missing = missing_for(
            &["node", "user", "aincient_core", "ai", "ai_agents"],
            &["aincient_core", "ai", "ai_agents", "flowdrop"],
            &["aincient_core", "flowdrop"],
        );
        assert_eq!(missing, vec!["ai", "ai_agents"]);
    }

    #[test]
    fn an_extension_the_site_added_itself_is_not_blamed_on_the_release() {
        // Installed but shipped by neither image (a module someone dropped in by
        // hand): not something this update removes, so not this guard's business.
        let missing = missing_for(&["node", "some_local_module"], &["flowdrop"], &["flowdrop"]);
        assert!(missing.is_empty());
    }

    #[test]
    fn extension_lists_parse_and_an_empty_label_is_unknown() {
        assert_eq!(
            parse_extension_list("aincient_core, flowdrop ,ai"),
            Some(vec!["ai".into(), "aincient_core".into(), "flowdrop".into()])
        );
        // The distinction the whole guard rests on: no label ≠ ships nothing.
        assert_eq!(parse_extension_list(""), None);
        assert_eq!(parse_extension_list("  , ,"), None);
    }

    #[test]
    fn a_malformed_blob_cannot_panic_the_parser() {
        // Lengths come from the database. A length that overruns the string, or
        // lands inside a multi-byte character, must yield nothing — not a panic
        // in the middle of an update.
        assert!(parse_serialized_extensions("s:99:\"node\";i:0;").is_empty());
        assert!(parse_serialized_extensions("s:2:\"é\";i:0;").is_empty());
        assert!(parse_serialized_extensions("s:").is_empty());
        assert!(parse_serialized_extensions("s:x:\"node\";i:0;").is_empty());
        assert!(parse_serialized_extensions("").is_empty());
    }

    #[test]
    fn installed_extensions_come_out_of_the_serialized_core_extension_blob() {
        // Modules and themes; `profile` is a string pair, not a weight, so it and
        // its value must not be mistaken for extensions.
        let blob = "a:3:{s:6:\"module\";a:2:{s:4:\"node\";i:0;s:2:\"ai\";i:10;}\
                    s:5:\"theme\";a:1:{s:14:\"aincient_theme\";i:0;}\
                    s:7:\"profile\";s:7:\"minimal\";}";
        assert_eq!(
            parse_serialized_extensions(blob),
            vec!["ai", "aincient_theme", "node"]
        );
    }

    /// Offline is decided by the connectivity pre-check and reported in the user's
    /// words; no registry read (no buildx) is attempted.
    #[test]
    fn an_offline_host_gets_the_offline_cause_without_a_registry_read() {
        let err =
            remote_probe_with("ghcr.io/aincient-labs/atelier-cms:stable", || false).unwrap_err();
        assert_eq!(err, REGISTRY_OFFLINE);
        assert_eq!(
            registry_problem("ghcr.io/aincient-labs/atelier-cms:stable", &err),
            "You're offline — couldn't reach the registry."
        );
    }

    #[test]
    fn a_remote_probe_reads_the_extension_label_beside_the_floor() {
        let p = parse_remote_probe(
            "sha256:413ed726a1a2|v0.5.1;0.3.0;aincient_core,flowdrop|v0.5.1;0.3.0;aincient_core,flowdrop|",
        );
        assert_eq!(p.version.as_deref(), Some("v0.5.1"));
        assert_eq!(p.floor, Some(Version(0, 3, 0)));
        assert_eq!(
            p.extensions,
            Some(vec!["aincient_core".into(), "flowdrop".into()])
        );
    }

    #[test]
    fn an_image_from_before_the_label_reports_no_extensions() {
        let p = parse_remote_probe("sha256:a36250871de0|v0.4.0;0.3.0|v0.4.0;0.3.0|");
        assert_eq!(p.version.as_deref(), Some("v0.4.0"));
        assert!(p.extensions.is_none(), "absence must read as unknown");
    }

    #[test]
    fn an_edge_stamp_compares_as_the_release_it_descends_from() {
        // The stamp change that lets a floor gate an edge install at all.
        assert_eq!(
            Version::parse("v0.5.1+edge.a1b2c3d"),
            Version::parse("0.5.1")
        );
        assert!(Version::parse("v0.5.1+edge.a1b2c3d") < Version::parse("0.6.0"));
        // The OLD stamp has no version before the `+` and stays unparseable —
        // "no position in the version order", which is the honest answer for it.
        assert_eq!(Version::parse("edge+f8bdcb9"), None);
        assert_eq!(Version::parse("dev"), None);
    }

    #[test]
    fn manifest_v2_records_the_digest_that_identifies_the_build() {
        let m: SnapshotManifest = serde_json::from_str(
            r#"{"format":"aincient-snapshot","version":2,"created":"20260805-152113",
                "image":"ghcr.io/aincient-labs/atelier-cms:edge",
                "digest":"sha256:e5dd6dbc","atelier_version":"edge+ee6a1cf"}"#,
        )
        .expect("parse v2");
        assert_eq!(m.digest, "sha256:e5dd6dbc");
        assert_eq!(m.atelier_version, "edge+ee6a1cf");
    }

    #[test]
    fn a_v1_manifest_still_parses_and_simply_has_no_digest() {
        // Archives taken before 2026-08-06. Absent provenance must read as
        // "unknown" (no warning), never as "matches".
        let m: SnapshotManifest = serde_json::from_str(
            r#"{"format":"aincient-snapshot","version":1,"created":"x","image":"y"}"#,
        )
        .expect("parse v1");
        assert!(m.digest.is_empty());
        assert_eq!(m.image, "y");
    }

    #[test]
    fn backup_script_is_valid_shell_and_packs_db_and_files() {
        let drush = "/opt/drupal/vendor/bin/drush --root=/opt/drupal/web";
        let manifest = r#"{"format":"aincient-snapshot","version":1,"created":"x","image":"y"}"#;
        let dump = backup_dump_script(drush, manifest, "/tmp/aincient-snapshot-1-2");
        let pack = backup_pack_script("/tmp/aincient-snapshot-1-2");
        assert_valid_sh(&dump);
        assert_valid_sh(&pack);
        // The pack step reads what the dump step staged.
        assert!(dump.contains("STAGE=/tmp/aincient-snapshot-1-2\n"));
        assert!(pack.contains("STAGE=/tmp/aincient-snapshot-1-2\n"));
        // A dump that isn't readable fails the backup before anything is packed,
        // and the archive is read back before the backup counts as saved.
        assert!(dump.contains("gzip -t \"$STAGE/database.sql.gz\""));
        assert!(pack.contains("tar tzf \"$ARCHIVE\""));
        let script = format!("{dump}{pack}");
        assert!(script.contains("sql:dump --gzip"), "dumps the database");
        // Both members packed: the staged DB dump and the live files tree.
        assert!(script.contains("database.sql.gz"), "packs the db dump");
        assert!(
            script.contains("-C /opt/drupal/web/sites/default files"),
            "packs the files tree"
        );
        // The derivative cache is regenerable and was 94% of a real archive.
        assert!(
            script.contains("--exclude=files/styles"),
            "skips the image-style derivative cache"
        );
        // The exclude must precede the members it applies to (GNU/busybox tar
        // apply --exclude to everything that follows on the command line).
        let exclude_at = script.find("--exclude=files/styles").unwrap();
        let files_at = script.find("sites/default files").unwrap();
        assert!(
            exclude_at < files_at,
            "exclude comes before the files member"
        );
        // Chat attachments (cms ContextStore) are packed next to frozen
        // snapshots when the folder exists, and the folder is never created.
        assert!(pack.contains(
            "if [ -d /opt/drupal/private/atelier-context ]; then CONTEXT=atelier-context; fi\n"
        ));
        assert!(pack.contains("-C /opt/drupal/private frozen $CONTEXT\n"));
        assert!(!pack.contains("mkdir -p /opt/drupal/private/atelier-context"));
    }

    #[test]
    fn the_pack_command_takes_chat_attachments_only_when_they_exist() {
        // Run the pack's member logic for real, against a private tree with and
        // without the attachments folder (paths rewritten into a scratch dir).
        let dir = Scratch::new("pack-context");
        let private = dir.0.join("private");
        let files = dir.0.join("files");
        let stage = dir.0.join("stage");
        for d in [private.join("frozen"), files.clone(), stage.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(stage.join("manifest.json"), "{}").unwrap();
        std::fs::write(stage.join("database.sql.gz"), "x").unwrap();
        let script = backup_pack_script(&stage.to_string_lossy())
            .replace("/opt/drupal/private", &private.to_string_lossy())
            .replace("/opt/drupal/web/sites/default", &dir.0.to_string_lossy());
        let members = |script: &str| {
            let ok = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .status()
                .unwrap();
            assert!(ok.success(), "pack script failed");
            std::fs::read_to_string(stage.join("members")).unwrap()
        };
        assert!(!members(&script)
            .lines()
            .any(|l| l.starts_with("atelier-context")));
        std::fs::create_dir_all(private.join("atelier-context")).unwrap();
        std::fs::write(private.join("atelier-context/abc.webp"), "img").unwrap();
        let listed = members(&script);
        assert!(
            listed.lines().any(|l| l == "atelier-context/abc.webp"),
            "{listed}"
        );
    }

    /// A unique scratch directory, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("aincient-ops-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A snapshot bundle holding just `manifest` — enough for every
    /// manifest reader.
    fn bundle_with(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let stage = dir.join(format!("stage-{name}"));
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("manifest.json"), manifest).unwrap();
        let file = dir.join(name);
        let ok = std::process::Command::new("tar")
            .arg("czf")
            .arg(&file)
            .arg("-C")
            .arg(&stage)
            .arg("manifest.json")
            .status()
            .unwrap()
            .success();
        assert!(ok, "tar");
        file
    }

    const ID_A: &str = "11111111-1111-4111-8111-111111111111";
    const ID_B: &str = "22222222-2222-4222-8222-222222222222";

    fn manifest_of(id: &str) -> SnapshotManifest {
        SnapshotManifest {
            site_id: id.to_string(),
            site_slug: "blog".to_string(),
            site_label: "Blog".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn identity_matching_restore_is_allowed_quietly() {
        let m = manifest_of(ID_A);
        assert_eq!(
            check_identity(Some(&m), Some(ID_A), "Blog (blog)", false).unwrap(),
            IdentityCheck::Same
        );
    }

    #[test]
    fn not_running_names_the_cli_command_and_stays_typed_under_context() {
        let err = anyhow::Error::from(super::NotRunning).context("backing up");
        assert!(
            err.downcast_ref::<super::NotRunning>().is_some(),
            "the window matches it"
        );
        assert!(
            format!("{err:#}").contains("`atelier app start`"),
            "{err:#}"
        );
    }

    #[test]
    fn identity_mismatch_is_refused_naming_both_sites_and_the_override() {
        let m = manifest_of(ID_A);
        let err = check_identity(Some(&m), Some(ID_B), "Shop (shop)", false).unwrap_err();
        let mismatch = err.downcast_ref::<IdentityMismatch>().expect("typed");
        assert_eq!(mismatch.archive_site_id, ID_A);
        assert_eq!(mismatch.target_site_id.as_deref(), Some(ID_B));
        let text = err.to_string();
        for needle in ["Blog (blog)", ID_A, "Shop (shop)", ID_B, "--other-site"] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        // A target that never made a backup has no identity: still another site.
        let err = check_identity(Some(&m), None, "Shop (shop)", false).unwrap_err();
        assert!(err.to_string().contains("none yet"), "{err}");
    }

    #[test]
    fn other_site_overrides_the_identity_check() {
        let m = manifest_of(ID_A);
        let got = check_identity(Some(&m), Some(ID_B), "Shop (shop)", true).unwrap();
        assert!(matches!(got, IdentityCheck::OtherAllowed(ref n) if n.contains("Blog (blog)")));
    }

    #[test]
    fn a_legacy_archive_restores_with_a_warning() {
        let v2 = SnapshotManifest {
            digest: "sha256:x".into(),
            ..Default::default()
        };
        for manifest in [None, Some(&v2)] {
            let got = check_identity(manifest, Some(ID_B), "Shop", false).unwrap();
            assert!(
                matches!(got, IdentityCheck::Unknown(ref w) if w.contains("WARNING")),
                "{got:?}"
            );
        }
    }

    #[test]
    fn manifest_v3_carries_the_site_identity() {
        let m: SnapshotManifest = serde_json::from_str(&format!(
            r#"{{"format":"aincient-snapshot","version":3,"created":"x","image":"y",
                "digest":"","atelier_version":"","site_id":"{ID_A}",
                "site_slug":"blog","site_label":"Bob's \"blog\""}}"#
        ))
        .unwrap();
        assert_eq!(m.site_id, ID_A);
        assert_eq!(m.site_name(), "Bob's \"blog\" (blog)");
    }

    /// Records what an op told its reporter, for the plan tests.
    #[derive(Default)]
    struct Recorder {
        plans: Vec<Vec<PlanStep>>,
        stages: Vec<(String, Option<f32>)>,
    }
    impl Reporter for Recorder {
        fn plan(&mut self, steps: &[PlanStep]) {
            self.plans.push(steps.to_vec());
        }
        fn stage(&mut self, _s: Stage, message: &str, fraction: Option<f32>) {
            self.stages.push((message.to_string(), fraction));
        }
    }

    #[test]
    fn a_weighted_plan_tiles_zero_to_one_in_order() {
        let plan = Plan::weighted(&[("a", "A", 1.0), ("b", "B", 3.0)]);
        let s = plan.steps();
        assert_eq!((s[0].from, s[0].to), (0.0, 0.25));
        assert_eq!((s[1].from, s[1].to), (0.25, 1.0));
    }

    #[test]
    fn a_plan_step_reports_its_start_and_passes_errors_through() {
        let plan = Plan::weighted(&[("a", "A", 1.0), ("b", "B", 1.0)]);
        let mut r = Recorder::default();
        plan.announce(&mut r);
        plan.run(&mut r, "a", "Doing A…", |_| Ok(())).unwrap();
        let err = plan
            .run(&mut r, "b", "Doing B…", |_| -> Result<()> {
                bail!("boom")
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(r.plans.len(), 1);
        assert_eq!(
            r.stages,
            vec![
                ("Doing A…".into(), Some(0.0)),
                ("Doing B…".into(), Some(0.5))
            ]
        );
    }

    #[test]
    fn a_nested_run_never_replaces_the_outer_plan() {
        let mut outer = Recorder::default();
        lifecycle_plan().announce(&mut outer);
        let mut nested = ScaledReporter {
            inner: &mut outer,
            base: 0.03,
            span: 0.02,
            prefix: String::new(),
        };
        let inner = Plan::weighted(&[("x", "X", 1.0)]);
        inner.announce(&mut nested);
        inner.finish(&mut nested, "done");
        assert_eq!(outer.plans.len(), 1, "only the lifecycle plan");
        assert_eq!(outer.stages.len(), 1);
        let f = outer.stages[0].1.unwrap();
        assert!(
            (f - 0.05).abs() < 1e-6,
            "lands at the end of its slice: {f}"
        );
    }

    #[test]
    fn the_lifecycle_plan_matches_the_fractions_its_helpers_report() {
        let plan = lifecycle_plan();
        let ids: Vec<&str> = plan.steps().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["prepare", "pull", "start", "boot"]);
        for pair in plan.steps().windows(2) {
            assert_eq!(pair[0].to, pair[1].from, "contiguous");
        }
        assert_eq!(plan.steps()[1].from, PULL_FROM);
        assert_eq!(plan.steps()[2].from, UP_FROM);
        assert_eq!(plan.steps()[3].from, BOOT_FROM);
    }

    #[cfg(unix)]
    #[test]
    fn a_backups_folder_is_tried_before_a_backup_and_nothing_is_left_or_made() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("backups-writable");
        let stack = Stack::at(dir.0.clone());
        // Not made yet: judged by the site's folder, and not created.
        assert_eq!(super::backups_writable(&stack), Ok(()));
        assert!(!stack.backups_dir().exists(), "a diagnosis makes nothing");
        std::fs::create_dir_all(stack.backups_dir()).unwrap();
        assert_eq!(super::backups_writable(&stack), Ok(()));
        assert_eq!(
            std::fs::read_dir(stack.backups_dir()).unwrap().count(),
            0,
            "the probe file is removed"
        );
        let ro = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(stack.backups_dir(), ro).unwrap();
        if std::fs::write(stack.backups_dir().join("probe"), "").is_ok() {
            // Root writes anywhere: nothing to tell apart.
            std::fs::set_permissions(stack.backups_dir(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            return;
        }
        let refused = super::backups_writable(&stack).unwrap_err();
        assert_eq!(refused.dir, stack.backups_dir());
        // The update's refusal keeps its type, with the plain cause.
        let e = super::backup_or_refuse::<()>(Err(anyhow::Error::new(refused))).unwrap_err();
        let said = e
            .downcast_ref::<super::BackupBeforeUpdateFailed>()
            .expect("typed")
            .to_string();
        assert!(
            said.starts_with("Couldn't back up your site first: Atelier can't save backups in ")
                && said.contains("it isn't writable (fix the folder's permissions")
                && said.ends_with(", so it wasn't updated"),
            "{said}"
        );
        // And the restore's, through its chain.
        let e = super::require_safety_backup(Err(anyhow::Error::new(super::BackupsNotWritable {
            dir: stack.backups_dir(),
        })))
        .unwrap_err();
        assert!(e
            .to_string()
            .contains("The backup failed: Atelier can't save backups in "));
        std::fs::set_permissions(stack.backups_dir(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }

    #[test]
    fn a_second_backup_in_the_same_second_never_overwrites_the_first() {
        let dir = Scratch::new("claim-name");
        let part = |n: &str, body: &[u8]| {
            let p = dir.0.join(format!(".{n}.part"));
            std::fs::write(&p, body).unwrap();
            p
        };
        // Both runs finish before either claims: the claim, not the pick, decides.
        let (a, b, c) = (part("a", b"one"), part("b", b"two"), part("c", b"three"));
        let first = super::claim_name(&a, &dir.0, "aincient-x", ".tar.gz").unwrap();
        let second = super::claim_name(&b, &dir.0, "aincient-x", ".tar.gz").unwrap();
        let third = super::claim_name(&c, &dir.0, "aincient-x", ".tar.gz").unwrap();
        assert_eq!(first, dir.0.join("aincient-x.tar.gz"));
        assert_eq!(second, dir.0.join("aincient-x-2.tar.gz"));
        assert_eq!(third, dir.0.join("aincient-x-3.tar.gz"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert!(
            !a.exists() && !b.exists() && !c.exists(),
            "parts are moved, not copied"
        );
    }

    #[test]
    fn only_stale_backup_parts_are_swept() {
        let dir = Scratch::new("sweep-parts");
        let old = dir.0.join(".aincient-x-1-2.part");
        let fresh = dir.0.join(".aincient-x-3-4.part");
        let backup = dir.0.join("aincient-x.tar.gz");
        let other = dir.0.join(".something.part");
        for p in [&old, &fresh, &backup, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - super::STALE_PART * 2;
        for p in [&old, &backup, &other] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        super::sweep_stale_parts(&dir.0);
        assert!(!old.exists(), "an interrupted copy is removed");
        assert!(fresh.exists(), "a copy in flight is kept");
        assert!(backup.exists() && other.exists(), "nothing else is touched");
    }

    #[test]
    fn run_ids_differ_between_runs() {
        assert_ne!(super::run_id(), super::run_id());
    }

    #[test]
    fn a_label_with_quotes_keeps_the_backup_script_valid() {
        let drush = "/opt/drupal/vendor/bin/drush --root=/opt/drupal/web";
        let manifest =
            serde_json::json!({ "site_label": "Bob's \"blog\" $(rm -rf /)" }).to_string();
        let script = backup_dump_script(drush, &manifest, "/tmp/aincient-snapshot-1-2");
        assert_valid_sh(&script);
        // What the printf writes is the manifest, byte for byte.
        let printf = script
            .lines()
            .find(|l| l.starts_with("printf"))
            .unwrap()
            .replace(" > \"$STAGE/manifest.json\"", "");
        let out = std::process::Command::new("sh")
            .args(["-c", &printf])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), manifest);
    }

    #[test]
    fn site_identity_is_minted_once_and_kept() {
        let dir = Scratch::new("site-id");
        let stack = Stack::at(dir.0.clone());
        assert_eq!(stack.site_id(), None);
        let id = stack.ensure_site_id().unwrap();
        assert!(crate::stack::is_site_id(&id), "{id}");
        assert_eq!(stack.site_id().as_deref(), Some(id.as_str()));
        assert_eq!(stack.ensure_site_id().unwrap(), id, "stable");
        // A garbled file is no identity, never a match for anything.
        std::fs::write(stack.site_id_path(), "not-an-id\n").unwrap();
        assert_eq!(stack.site_id(), None);
    }

    #[test]
    fn local_restore_refuses_another_sites_archive_before_touching_anything() {
        let dir = Scratch::new("restore-identity");
        let stack = Stack::at(dir.0.join("site"));
        std::fs::create_dir_all(&stack.home).unwrap();
        std::fs::write(stack.site_id_path(), format!("{ID_B}\n")).unwrap();
        let ours = bundle_with(
            &dir.0,
            "ours.tar.gz",
            &format!(r#"{{"format":"aincient-snapshot","version":3,"site_id":"{ID_B}"}}"#),
        );
        let theirs = bundle_with(
            &dir.0,
            "theirs.tar.gz",
            &format!(
                r#"{{"format":"aincient-snapshot","version":3,"site_id":"{ID_A}","site_slug":"blog"}}"#
            ),
        );
        let legacy = bundle_with(
            &dir.0,
            "legacy.tar.gz",
            r#"{"format":"aincient-snapshot","version":2,"image":"y"}"#,
        );
        assert_eq!(
            restore_identity(&stack, &ours, false).unwrap(),
            IdentityCheck::Same
        );
        assert!(matches!(
            restore_identity(&stack, &legacy, false).unwrap(),
            IdentityCheck::Unknown(_)
        ));
        assert!(matches!(
            restore_identity(&stack, &theirs, true).unwrap(),
            IdentityCheck::OtherAllowed(_)
        ));
        // The checked restore refuses before Docker is ever asked anything.
        let err =
            restore_with(&stack, &theirs, &RestoreOptions::default(), &mut Silent).unwrap_err();
        assert!(err.downcast_ref::<IdentityMismatch>().is_some(), "{err:#}");
    }

    #[test]
    fn the_other_site_note_is_said_once() {
        #[derive(Default)]
        struct Logged(Vec<String>);
        impl Reporter for Logged {
            fn log(&mut self, line: &str) {
                self.0.push(line.to_string());
            }
        }
        let dir = Scratch::new("restore-note");
        // Not installed: the restore stops right after the identity check,
        // before Docker is asked anything.
        let stack = Stack::at(dir.0.join("site"));
        std::fs::create_dir_all(&stack.home).unwrap();
        std::fs::write(stack.site_id_path(), format!("{ID_B}\n")).unwrap();
        let theirs = bundle_with(
            &dir.0,
            "theirs.tar.gz",
            &format!(
                r#"{{"format":"aincient-snapshot","version":3,"site_id":"{ID_A}","site_slug":"blog"}}"#
            ),
        );
        let notes = |identity_shown| {
            let mut r = Logged::default();
            let opts = RestoreOptions {
                other_site: true,
                identity_shown,
                ..RestoreOptions::default()
            };
            let _ = restore_with(&stack, &theirs, &opts, &mut r);
            r.0.iter()
                .filter(|l| l.starts_with("Restoring a backup of another site"))
                .count()
        };
        // The app: the restore says it.
        assert_eq!(notes(false), 1);
        // The CLI printed it before its confirmation: not again.
        assert_eq!(notes(true), 0);
    }

    #[test]
    fn describe_backup_reports_size_sha_and_origin() {
        let dir = Scratch::new("describe");
        let file = bundle_with(
            &dir.0,
            "aincient-x.tar.gz",
            &format!(r#"{{"site_id":"{ID_A}","site_slug":"blog","site_label":"Blog"}}"#),
        );
        let info = describe_backup(&file).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(info.size, bytes.len() as u64);
        use sha2::Digest;
        let want: String = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(info.sha256, want);
        assert_eq!(info.name, "aincient-x.tar.gz");
        assert_eq!(info.site_id.as_deref(), Some(ID_A));
        assert_eq!(info.site_name.as_deref(), Some("Blog (blog)"));
        // Round-trips as the JSON a Manager reads back.
        let back: BackupInfo =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn restore_bundle_script_is_valid_shell_and_reasserts_ownership() {
        let drush = "/opt/drupal/vendor/bin/drush --root=/opt/drupal/web";
        let parts = restore_bundle_scripts(drush, "/tmp/aincient-restore-1-2");
        for part in &parts {
            assert_valid_sh(part);
            // Each exec finds the archive and work dir the unpack step used.
            assert!(part.contains("WORK=/tmp/aincient-restore-1-2\n"));
        }
        // The database is only dropped after its dump is proven readable.
        let db = &parts[1];
        let check = db.find("gzip -t").expect("checks the dump");
        let drop = db.find("sql:drop").expect("drops");
        assert!(check < drop, "check before drop");
        let script = parts.concat();
        assert!(script.contains("sql:drop -y"), "drops before load");
        assert!(
            script.contains("| /opt/drupal/vendor/bin/drush"),
            "loads the db"
        );
        // The scotty gotcha: files written as root must be chowned back to www-data.
        assert!(
            script.contains("chown -R www-data:www-data"),
            "re-asserts files ownership"
        );
        // Chat attachments come back the way frozen snapshots do; an archive
        // without them (taken before 0.17.0) leaves the folder as it is.
        let files = &parts[2];
        assert!(files.contains(
            "if [ -d \"$WORK/atelier-context\" ]; then\n\
             \x20 rm -rf /opt/drupal/private/atelier-context\n\
             \x20 cp -a \"$WORK/atelier-context\" /opt/drupal/private/atelier-context\n\
             \x20 chown -R www-data:www-data /opt/drupal/private/atelier-context\n\
             fi\n"
        ));
    }

    #[test]
    fn restore_converges_in_the_app_container() {
        // A restore reloads a database that may predate the running image, so it
        // must end by re-running the appliance's converge (updatedb + cim +
        // cache_file_parsing) — not just a cache rebuild — or an older snapshot
        // comes up blank.
        let dir = Scratch::new("restore-converge");
        let stack = install_on(&dir.0, "ghcr.io/aincient-labs/cms:0.24.0");
        let cmd = super::converge_cmd(&stack);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(
            joined.contains("exec -T app /usr/local/bin/converge.sh"),
            "converge runs in the app container: {joined}"
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

        let listed = list_backups(&Stack::at(dir.clone()));
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
        assert_eq!(
            parse_http_status(b"HTTP/1.0 302 Found\r\nLocation: /x"),
            Some(302)
        );
        assert_eq!(
            parse_http_status(b"HTTP/1.1 503 Service Unavailable\r\n"),
            Some(503)
        );
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

    // Fixtures below are verbatim output of the two probe commands against
    // ghcr.io/aincient-labs/atelier-cms, captured 2026-08-04.

    #[test]
    fn local_probe_reads_the_digest_the_version_and_the_floor() {
        let p = parse_local_probe(
            "ghcr.io/aincient-labs/atelier-cms@sha256:413ed726a1a2|edge+f8bdcb9|0.3.0",
        );
        assert_eq!(p.digest.as_deref(), Some("sha256:413ed726a1a2"));
        assert_eq!(p.version.as_deref(), Some("edge+f8bdcb9"));
        assert_eq!(p.floor, Some(Version(0, 3, 0)));
    }

    #[test]
    fn local_probe_tolerates_a_locally_built_unlabelled_image() {
        // Empty RepoDigests and no labels — all three fields blank, not an error.
        let p = parse_local_probe("||");
        assert!(p.digest.is_none(), "no registry digest to compare");
        assert!(p.version.is_none(), "unstamped build claims no version");
        assert!(p.floor.is_none(), "and declares no floor");
    }

    #[test]
    fn a_probe_of_an_image_predating_the_floor_label_declares_no_floor() {
        // The label is simply absent, which the template renders as empty. Read as
        // "no floor", never as 0.0.0 — an old image makes no promise either way.
        let p = parse_local_probe("repo@sha256:abc|v0.1.0|");
        assert_eq!(p.version.as_deref(), Some("v0.1.0"));
        assert!(p.floor.is_none());
    }

    #[test]
    fn remote_probe_takes_one_version_and_floor_from_the_multi_arch_index() {
        // `range` over .Image emits the pair once per platform, both identical.
        let p = parse_remote_probe("sha256:413ed726a1a2|v0.4.0;0.3.0|v0.4.0;0.3.0|");
        assert_eq!(p.digest.as_deref(), Some("sha256:413ed726a1a2"));
        assert_eq!(p.version.as_deref(), Some("v0.4.0"));
        assert_eq!(p.floor, Some(Version(0, 3, 0)));
    }

    #[test]
    fn remote_probe_keeps_the_digest_when_no_platform_is_stamped() {
        let p = parse_remote_probe("sha256:a36250871de0|;|;|");
        assert_eq!(p.digest.as_deref(), Some("sha256:a36250871de0"));
        assert!(p.version.is_none());
        assert!(p.floor.is_none());
    }

    #[test]
    fn versions_parse_only_when_they_are_releases() {
        assert_eq!(Version::parse("0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("v0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse(" v1.10.3 "), Some(Version(1, 10, 3)));
        // Neither of these has a position in the version order, so neither may
        // pretend to: a floor check against them must come out "unknown".
        assert_eq!(Version::parse("edge+f8bdcb9"), None);
        assert_eq!(Version::parse("dev"), None);
        for bad in ["", "0.2", "0.2.0.1", "0.2.x", "v", "0..0", "1.2.3-rc1"] {
            assert_eq!(Version::parse(bad), None, "{bad} is not a release version");
        }
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        assert!(Version::parse("0.9.0") < Version::parse("0.10.0"));
        assert!(Version::parse("0.2.10") > Version::parse("0.2.9"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
    }

    #[test]
    fn a_waypoint_is_the_immutable_tag_the_release_workflow_publishes() {
        assert_eq!(
            waypoint_image(Version(0, 3, 0)),
            "ghcr.io/aincient-labs/atelier-cms:v0.3.0"
        );
    }

    /// A target whose floor this install already meets is one hop — the ordinary
    /// case, and the one that must not grow a step.
    #[test]
    fn a_satisfied_floor_plans_a_single_direct_hop() {
        let plan = plan_route(
            Version::parse("0.3.0"),
            "ghcr.io/aincient-labs/atelier-cms:latest".into(),
            ImageProbe {
                digest: Some("sha256:x".into()),
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                extensions: None,
            },
            None,
        );
        assert!(!plan.is_stepped());
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.steps[0].is_target);
        assert!(plan.problem.is_none());
    }

    /// A proven backwards move — installed newer than the target, both versions
    /// known — is flagged so the update path can refuse it. A site's database only
    /// migrates forward; recovery is a restore, not a downgrade.
    #[test]
    fn a_lower_target_than_the_installed_version_is_a_downgrade() {
        let plan = plan_route(
            Version::parse("0.6.0"),
            "ghcr.io/aincient-labs/atelier-cms:v0.1.0".into(),
            ImageProbe {
                version: Some("v0.1.0".into()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(plan.target_version(), Version::parse("0.1.0"));
        assert!(plan.is_downgrade());
    }

    /// The ordinary forward update is not a downgrade.
    #[test]
    fn a_higher_target_than_the_installed_version_is_not_a_downgrade() {
        let plan = plan_route(
            Version::parse("0.1.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.6.0".into()),
                ..Default::default()
            },
            None,
        );
        assert!(!plan.is_downgrade());
    }

    /// Re-running the same version (a reconverge / digest refresh) is not a
    /// downgrade — `to < from` is strict.
    #[test]
    fn the_same_version_is_not_a_downgrade() {
        let plan = plan_route(
            Version::parse("0.6.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.6.0".into()),
                ..Default::default()
            },
            None,
        );
        assert!(!plan.is_downgrade());
    }

    /// Unknown on either side cannot PROVE a downgrade, so it never claims one — the
    /// appliance's own converge refusal covers the case we can't read.
    #[test]
    fn an_unprovable_downgrade_is_not_flagged() {
        // Unknown installed version (an unstamped build).
        let unknown_from = plan_route(
            None,
            "img:v0.1.0".into(),
            ImageProbe {
                version: Some("v0.1.0".into()),
                ..Default::default()
            },
            None,
        );
        assert!(!unknown_from.is_downgrade());
        // A literal image target that carries no version (a fork, a local build).
        let unknown_to = plan_route(
            Version::parse("0.6.0"),
            "my-fork:dev".into(),
            ImageProbe::default(),
            None,
        );
        assert_eq!(unknown_to.target_version(), None);
        assert!(!unknown_to.is_downgrade());
    }

    /// No floor at all (every image published before this mechanism) is also one
    /// hop: absence is a declaration of "no requirement", not a missing one.
    #[test]
    fn no_declared_floor_plans_a_single_direct_hop() {
        let plan = plan_route(
            Version::parse("0.1.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                ..Default::default()
            },
            None,
        );
        assert!(!plan.is_stepped());
    }

    /// An unknown installed version must not manufacture a route through releases
    /// the operator may well be past already.
    #[test]
    fn an_unknown_installed_version_goes_direct_and_lets_the_appliance_decide() {
        let plan = plan_route(
            None,
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                ..Default::default()
            },
            None,
        );
        assert!(!plan.is_stepped());
        assert!(plan.steps[0].is_target);
    }

    /// The last hop is always the thing the operator asked for, and it is the only
    /// one flagged as the target — the waypoints are ours, not theirs.
    #[test]
    fn the_target_is_the_final_hop_and_carries_no_reason() {
        let plan = plan_route(
            Version::parse("0.3.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                ..Default::default()
            },
            None,
        );
        let last = plan.steps.last().unwrap();
        assert!(last.is_target);
        assert_eq!(last.image, "img:latest");
        assert!(last.reason.is_none());
        assert_eq!(plan.waypoints().count(), 0);
    }

    /// A fake registry: version → the floor that release declares.
    fn registry(
        floors: &[(&'static str, Option<&'static str>)],
    ) -> impl Fn(&str) -> std::result::Result<Option<Version>, String> {
        let floors: Vec<_> = floors
            .iter()
            .map(|(v, f)| (waypoint_image(Version::parse(v).unwrap()), *f))
            .collect();
        move |image: &str| {
            floors
                .iter()
                .find(|(img, _)| img == image)
                .map(|(_, f)| f.and_then(Version::parse))
                .ok_or_else(|| format!("no such tag: {image}"))
        }
    }

    /// THE CASE THE WHOLE MECHANISM EXISTS FOR: 0.4.0 drops the code that
    /// uninstalls `drupal/ai`, so it declares it can only migrate 0.3.0 and newer,
    /// and a 0.1.1 install has to pass through 0.3.0 to get there.
    #[test]
    fn an_unmet_floor_plans_a_waypoint_before_the_target() {
        let plan = plan_route_with(
            Version::parse("0.1.1"),
            "ghcr.io/aincient-labs/atelier-cms:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                ..Default::default()
            },
            None,
            registry(&[("0.3.0", Some("0.1.0"))]),
        );
        assert!(plan.is_stepped());
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(
            plan.steps[0].image,
            "ghcr.io/aincient-labs/atelier-cms:v0.3.0"
        );
        assert_eq!(plan.steps[0].version, Version::parse("0.3.0"));
        assert!(!plan.steps[0].is_target);
        assert!(plan.steps[0].reason.as_ref().unwrap().contains("0.4.0"));
        assert!(plan.steps[1].is_target);
        assert!(plan.problem.is_none());
    }

    /// Chained floors: the waypoint has a floor of its own that this install also
    /// fails, so the route grows another hop — and comes out OLDEST FIRST, which is
    /// the only order in which it can be applied.
    #[test]
    fn chained_floors_produce_an_oldest_first_route() {
        let plan = plan_route_with(
            Version::parse("0.1.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.6.0".into()),
                floor: Version::parse("0.5.0"),
                ..Default::default()
            },
            None,
            registry(&[
                ("0.5.0", Some("0.3.0")),
                ("0.3.0", Some("0.1.0")), // met — the walk stops here
            ]),
        );
        let route: Vec<_> = plan.steps.iter().map(|s| s.version).collect();
        assert_eq!(
            route,
            vec![
                Version::parse("0.3.0"),
                Version::parse("0.5.0"),
                Version::parse("0.6.0")
            ]
        );
        assert_eq!(plan.waypoints().count(), 2);
        assert!(plan.problem.is_none());
    }

    /// A floor that isn't older than the release declaring it can't converge. Treat
    /// it as the end of the chain rather than looping: the route so far is still
    /// worth applying, and the alternative is a hang on bad metadata.
    #[test]
    fn a_non_decreasing_floor_ends_the_walk_instead_of_looping() {
        let plan = plan_route_with(
            Version::parse("0.1.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                ..Default::default()
            },
            None,
            // 0.3.0 claims it needs 0.3.0 — itself.
            registry(&[("0.3.0", Some("0.3.0"))]),
        );
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].version, Version::parse("0.3.0"));
    }

    /// Floors that keep asking for something older forever must be reported, not
    /// walked. Each candidate here demands the next one down, so nothing is ever
    /// satisfied and only the cap stops it.
    #[test]
    fn a_route_that_never_resolves_is_reported_as_a_problem() {
        let mut floors: Vec<(&'static str, Option<&'static str>)> = Vec::new();
        for (v, f) in [
            ("0.9.0", "0.8.0"),
            ("0.8.0", "0.7.0"),
            ("0.7.0", "0.6.0"),
            ("0.6.0", "0.5.0"),
            ("0.5.0", "0.4.0"),
            ("0.4.0", "0.3.0"),
            ("0.3.0", "0.2.0"),
            ("0.2.0", "0.1.0"),
            ("0.1.0", "0.0.9"),
            ("0.0.9", "0.0.8"),
        ] {
            floors.push((v, Some(f)));
        }
        let plan = plan_route_with(
            // Older than every floor in the chain, so none is ever met.
            Version::parse("0.0.1"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v1.0.0".into()),
                floor: Version::parse("0.9.0"),
                ..Default::default()
            },
            None,
            registry(&floors),
        );
        assert!(plan.problem.is_some(), "must say the route doesn't resolve");
        assert!(plan.waypoints().count() <= MAX_WAYPOINTS);
    }

    /// A waypoint we can't read leaves the rest of the route unknown — keep the
    /// hops we do know, and say so rather than implying the route is complete.
    #[test]
    fn an_unreadable_waypoint_keeps_the_known_hops_and_explains() {
        let plan = plan_route_with(
            Version::parse("0.1.0"),
            "img:latest".into(),
            ImageProbe {
                version: Some("v0.4.0".into()),
                floor: Version::parse("0.3.0"),
                ..Default::default()
            },
            None,
            registry(&[]), // 0.3.0 isn't in the registry
        );
        assert!(plan.is_stepped());
        assert_eq!(plan.steps[0].version, Version::parse("0.3.0"));
        assert!(plan.problem.as_ref().unwrap().contains("0.3.0"));
    }

    #[test]
    fn a_missing_buildx_plugin_is_named_as_such() {
        // Docker's own wording for an absent CLI plugin.
        let problem = registry_problem("img", "docker: unknown command: docker buildx");
        assert!(problem.contains("buildx plugin is missing"), "{problem}");
        assert!(!problem.contains("logged in"), "the image is public");
    }

    #[test]
    fn any_other_registry_failure_carries_the_error_through() {
        let problem = registry_problem("img", "dial tcp: lookup ghcr.io: no such host");
        assert!(problem.contains("Couldn't reach the registry"), "{problem}");
        assert!(
            problem.contains("no such host"),
            "keeps the cause: {problem}"
        );
    }

    // --- Updates (hardening M5) ---------------------------------------------

    /// Image IDs as `docker image inspect --format {{.Id}}` and `docker inspect
    /// --format {{.Image}}` print them.
    const ID_OLD: &str = "sha256:6f1c3b0e2a9d4c7f8e5b1a2d3c4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60";
    const ID_NEW: &str = "sha256:0a9b8c7d6e5f40312233445566778899aabbccddeeff00112233445566778899";

    #[test]
    fn an_update_whose_download_is_the_running_build_does_nothing() {
        use super::UpdateNeed::{Nothing, Settings, Update};
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), Some(false), false, false),
            Nothing
        );
        // A new build, or a side that can't be read, is an update as before.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_NEW), Some(false), false, false),
            Update
        );
        assert_eq!(update_need(None, Some(ID_NEW), None, false, false), Update);
        assert_eq!(update_need(Some(ID_OLD), None, None, false, false), Update);
        // A new build with changed settings too: the full update.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_NEW), Some(true), true, false),
            Update
        );
        // The db move is due: the full update.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), Some(false), false, true),
            Update
        );
        // Same build, settings changed since the containers were made: `up`
        // on that build, no backup.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), Some(true), false, false),
            Settings
        );
        // The hashes decide over the rewritten files: a rewrite compose sees no
        // difference in changes nothing.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), Some(false), true, false),
            Nothing
        );
        // Unreadable hashes: the rewritten files decide.
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), None, true, false),
            Settings
        );
        assert_eq!(
            update_need(Some(ID_OLD), Some(ID_OLD), None, false, false),
            Nothing
        );
    }

    /// `docker compose -p atelier-hardening-test config --hash '*'`
    /// (Compose v5.1.2, 2026-11-14), right after install.
    const DECLARED_HASHES: &str =
        "app b49b0ed33609761aae262de194483bbb53cdae84a38044d8f683e5022507b722\n\
         db 564c3ffec3746bcd7b3e5e088a27156540a2029b5ae267d04a8823699a679103\n\
         edge 64dee302d711303f4c03706f107a1329ac1b13a07a3e6e008876598269bbae5b\n";

    #[test]
    fn settings_compare_each_service_s_config_hash_to_its_container() {
        // `docker inspect -f RUNNING_HASHES` of the three containers that
        // install made (another order than compose's).
        let running = "app b49b0ed33609761aae262de194483bbb53cdae84a38044d8f683e5022507b722\n\
             edge 64dee302d711303f4c03706f107a1329ac1b13a07a3e6e008876598269bbae5b\n\
             db 564c3ffec3746bcd7b3e5e088a27156540a2029b5ae267d04a8823699a679103\n";
        assert_eq!(settings_changed(DECLARED_HASHES, running), Some(false));
        // HTTP_PORT changed in .env: edge's hash moves, the others don't.
        let port_changed = running.replace(
            "edge 64dee302d711303f4c03706f107a1329ac1b13a07a3e6e008876598269bbae5b",
            "edge 1f0e37b4b44f3c6c8a3b0e3a8b4f2b0d9b8c5a7e6d4f3e2d1c0b9a8f7e6d5c4b",
        );
        assert_eq!(settings_changed(DECLARED_HASHES, &port_changed), Some(true));
        // A service with no container (or a container without the label).
        let no_edge = "app b49b0ed33609761aae262de194483bbb53cdae84a38044d8f683e5022507b722\n\
             db 564c3ffec3746bcd7b3e5e088a27156540a2029b5ae267d04a8823699a679103\n\
             edge \n";
        assert_eq!(settings_changed(DECLARED_HASHES, no_edge), Some(true));
        assert_eq!(settings_changed(DECLARED_HASHES, ""), Some(true));
        // Compose said nothing usable: unknown.
        assert_eq!(settings_changed("", running), None);
        assert!(super::SETTINGS_APPLIED.starts_with("Already up to date."));
        // Settings-only counts as a site that answered.
        assert!(super::Updated::SettingsApplied.ready());
    }

    #[test]
    fn an_unfinished_route_is_resumed_and_written() {
        let channel = || "ghcr.io/aincient-labs/atelier-cms:latest".to_string();
        let target = "ghcr.io/aincient-labs/atelier-cms:latest".to_string();
        // A route stopped at a waypoint: `.env` names the waypoint, the file the end.
        assert_eq!(
            route_target(None, Some(target.clone()), || {
                "ghcr.io/aincient-labs/atelier-cms:0.12.0".to_string()
            }),
            (target.clone(), true)
        );
        assert_eq!(route_target(None, None, channel), (target, false));
        // A version the operator names wins over the unfinished route.
        let named = "ghcr.io/aincient-labs/atelier-cms:0.20.0".to_string();
        assert_eq!(
            route_target(
                Some(named.clone()),
                Some("ghcr.io/aincient-labs/atelier-cms:latest".into()),
                channel
            ),
            (named, true)
        );
    }

    #[test]
    fn the_update_target_survives_until_cleared_and_a_channel_switch_drops_it() {
        let dir = Scratch::new("update-target");
        let stack = Stack::at(dir.0.clone());
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();
        assert_eq!(stack.update_target(), None);
        stack
            .set_update_target("ghcr.io/aincient-labs/atelier-cms:latest")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join(".update-target")).unwrap(),
            "ghcr.io/aincient-labs/atelier-cms:latest\n"
        );
        assert_eq!(
            stack.update_target().as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms:latest")
        );
        switch_channel(&stack, Channel::Edge, false, &mut Silent).unwrap();
        assert_eq!(stack.update_target(), None);
        // An empty file is no route.
        std::fs::write(stack.update_target_path(), "\n").unwrap();
        assert_eq!(stack.update_target(), None);
    }

    #[test]
    fn an_update_check_cached_before_unfinished_existed_still_reads() {
        let check: UpdateCheck = serde_json::from_str(
            r#"{"image":"ghcr.io/aincient-labs/atelier-cms:latest","current":null,
                "latest":null,"current_version":null,"latest_version":null,
                "update_available":null,"problem":null,"plan":null}"#,
        )
        .unwrap();
        assert!(!check.unfinished);
    }

    #[test]
    fn a_stopped_site_is_started_for_its_backup_but_not_a_half_removed_one() {
        // Resting through the engine: app and db exited, edge serving.
        let resting = format!("{}\n{}\n{PS_EDGE}", stopped(PS_APP), stopped(PS_DB));
        assert!(start_for_backup_needed(container_state_from_ps(&resting)));
        let all_stopped = format!(
            "{}\n{}\n{}",
            stopped(PS_APP),
            stopped(PS_DB),
            stopped(PS_EDGE)
        );
        assert!(start_for_backup_needed(container_state_from_ps(
            &all_stopped
        )));
        let running = format!("{PS_APP}\n{PS_DB}\n{PS_EDGE}");
        assert!(!start_for_backup_needed(container_state_from_ps(&running)));
        // No db container: Start would be an `up` onto the new image.
        let no_db = format!("{}\n{PS_EDGE}", stopped(PS_APP));
        assert!(!start_for_backup_needed(container_state_from_ps(&no_db)));
        assert!(!start_for_backup_needed(ContainerState::Unknown));
    }

    #[test]
    fn a_locked_site_is_refused_and_a_resting_one_is_not() {
        let e = refuse_locked(Some("locked")).unwrap_err().to_string();
        assert!(e.starts_with("This site is locked. Unlock it first"), "{e}");
        for state in [None, Some("resting"), Some("awake"), Some("stopped")] {
            assert!(refuse_locked(state).is_ok(), "{state:?}");
        }
    }

    /// A stack on the old bookworm db image, as 0.16 installs have it.
    fn legacy_db_stack(tag: &str) -> (Scratch, Stack) {
        use crate::stack::{COMPOSE_TEMPLATE, DB_IMAGE, LEGACY_DB_IMAGE};
        let dir = Scratch::new(tag);
        let stack = Stack::at(dir.0.clone());
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();
        std::fs::write(
            stack.compose_path(),
            COMPOSE_TEMPLATE.replace(DB_IMAGE, LEGACY_DB_IMAGE),
        )
        .unwrap();
        (dir, stack)
    }

    #[test]
    fn a_failed_db_image_download_leaves_compose_for_the_next_update() {
        use crate::stack::{DB_IMAGE, LEGACY_DB_IMAGE};
        let (_dir, stack) = legacy_db_stack("db-pull-fails");
        let before = std::fs::read_to_string(stack.compose_path()).unwrap();
        let mut asked = Vec::new();
        let moved = pull_then_move_db(
            &stack,
            |image, _r| {
                asked.push(image.to_string());
                bail!("Error response from daemon: Get \"https://registry-1.docker.io/v2/\": dial tcp: lookup registry-1.docker.io: no such host")
            },
            &mut Silent,
        )
        .unwrap();
        assert_eq!(moved, None);
        assert_eq!(asked, [DB_IMAGE]);
        assert_eq!(
            std::fs::read_to_string(stack.compose_path()).unwrap(),
            before
        );
        assert_eq!(
            stack.pending_db_image_migration(),
            Some((LEGACY_DB_IMAGE.to_string(), DB_IMAGE.to_string()))
        );
    }

    #[test]
    fn the_db_image_is_downloaded_before_compose_names_it() {
        use crate::stack::{DB_IMAGE, LEGACY_DB_IMAGE};
        let (_dir, stack) = legacy_db_stack("db-pull-first");
        let moved = pull_then_move_db(
            &stack,
            |_image, _r| {
                // compose.yaml still names the old image while it downloads.
                assert!(stack.pending_db_image_migration().is_some());
                Ok(())
            },
            &mut Silent,
        )
        .unwrap();
        assert_eq!(
            moved,
            Some((LEGACY_DB_IMAGE.to_string(), DB_IMAGE.to_string()))
        );
        assert_eq!(stack.pending_db_image_migration(), None);
    }

    #[test]
    fn the_login_hint_is_only_for_a_refused_login() {
        for refused in [
            r#"Error response from daemon: Head "https://ghcr.io/v2/aincient-labs/atelier-cms/manifests/latest": denied"#,
            r#"Error response from daemon: Head "https://ghcr.io/v2/aincient-labs/atelier-cms/manifests/latest": unauthorized"#,
            "Error response from daemon: pull access denied for ghcr.io/aincient-labs/atelier-cms, repository does not exist or may require 'docker login': denied: requested access to the resource is denied",
            r#"{"id":"app","status":"Error","text":"Error response from daemon: denied"}"#,
        ] {
            assert!(registry_refused(refused), "{refused}");
        }
        for other in [
            r#"Error response from daemon: Get "https://ghcr.io/v2/": dial tcp: lookup ghcr.io: no such host"#,
            "Error response from daemon: manifest unknown",
            "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: Head \"http://%2Fvar%2Frun%2Fdocker.sock/_ping\"",
            r#"{"id":"app","status":"Pulling","text":"Pulling"}"#,
        ] {
            assert!(!registry_refused(other), "{other}");
        }
    }
}
