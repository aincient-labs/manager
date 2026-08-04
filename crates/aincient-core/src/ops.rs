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
use crate::stack::{Channel, InstallOptions, Stack};

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
    /// Public site root (the front page visitors see) — the "View my site"
    /// target, distinct from the `/atelier` console.
    pub site_url: String,
    pub image: String,
    /// Which stream of images this install follows — stable, edge, or pinned.
    pub channel: Channel,
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

impl Version {
    /// Parse a release version, or `None` for anything that isn't one.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let mut next = || parts.next().filter(|p| !p.is_empty())?.parse::<u64>().ok();
        let (major, minor, patch) = (next()?, next()?, next()?);
        parts.next().is_none().then_some(Version(major, minor, patch))
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
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
}

impl UpgradePlan {
    /// Whether this route passes through versions nobody asked for.
    pub fn is_stepped(&self) -> bool {
        self.steps.len() > 1
    }

    /// The waypoints — every hop that isn't the destination.
    pub fn waypoints(&self) -> impl Iterator<Item = &UpgradeStep> {
        self.steps.iter().filter(|s| !s.is_target)
    }
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
        site_url: stack.site_url(),
        image: stack.image(),
        channel: stack.channel(),
        image_digest: local_digest(&stack.image()),
    }
}

/// Poll the console until it serves a healthy response, or `timeout` elapses,
/// reporting boot progress as it goes. Returns `true` once it's ready.
///
/// This is what holds the manager's "ready" signal back until Drupal has
/// actually finished booting (site install, `converge.sh` migrations) instead of
/// the moment the container reports "running".
pub fn wait_until_ready(stack: &Stack, timeout: Duration, r: &mut dyn Reporter) -> bool {
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

/// Compare the local image to the registry tag, reading both digests (the
/// comparison) and both version labels (what to *call* them).
///
/// Inconclusive is a legitimate outcome, but it must say why: an unattributed
/// "couldn't check" is what made atelier-cms#7 undiagnosable from a bug report.
/// Each failure is attributed to one of four causes — Docker unavailable, the
/// image not pulled, `buildx` missing, the registry unreachable — since the fix
/// differs for every one of them.
pub fn check_update(stack: &Stack) -> UpdateCheck {
    let image = stack.image();
    let mut check = UpdateCheck {
        image: image.clone(),
        current: None,
        latest: None,
        current_version: None,
        latest_version: None,
        update_available: None,
        problem: None,
        plan: None,
    };

    // Docker itself is the floor: without it neither probe can say anything, and
    // `preflight` already phrases that case well.
    if let Some(problem) = preflight().problem() {
        check.problem = Some(problem);
        return check;
    }

    match local_probe(&image) {
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

    let mut target = None;
    match remote_probe(&image) {
        Ok(probe) => {
            check.latest = probe.digest.clone();
            check.latest_version = probe.version.clone();
            target = Some(probe);
        }
        Err(e) if check.problem.is_none() => check.problem = Some(registry_problem(&image, &e)),
        Err(_) => {}
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
            check.plan = Some(plan_route(from, image, target, None));
        }
    }
    check
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
    let target_image = match to {
        Some(v) => match Version::parse(v) {
            Some(v) => waypoint_image(v),
            // Not a version — take it as a literal image reference, which is how
            // an operator pins a fork or a local build.
            None => v.to_string(),
        },
        // The image an update is heading for is the one that will be configured
        // when it runs — so a pending legacy-`:edge` → stable move counts, or the
        // route would be planned against the tag the operator is about to leave.
        None if stack.pending_default_channel_migration() => Channel::Stable
            .image()
            .unwrap_or_else(|| stack.image()),
        None => stack.image(),
    };

    let (from, from_problem) = match local_probe(&stack.image()) {
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

    match remote_probe(&target_image) {
        Ok(target) => plan_route(from, target_image, target, from_problem),
        Err(e) => {
            // No floors to read → the direct hop, and say why it wasn't verified.
            // Not an error: the appliance's own refusal covers the unsafe case, so
            // an unreadable registry costs the route, not the safety.
            let mut plan = plan_route(from, target_image.clone(), ImageProbe::default(), None);
            plan.problem = Some(registry_problem(&target_image, &e));
            plan
        }
    }
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
    }
}

/// Turn a failed registry read into advice. A missing `buildx` plugin is the
/// common one — it ships with Docker Desktop but is a separate package on Linux
/// (`docker-buildx-plugin`), and nothing else in the manager needs it, so a host
/// without it works fine right up to the update check.
fn registry_problem(image: &str, error: &str) -> String {
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
    r.stage(Stage::Preflight, "Checking Docker…", Some(0.04));
    preflight().require()?;
    r.stage(Stage::Scaffold, "Preparing the stack…", Some(0.08));
    stack.ensure_scaffold(opts)?;
    if opts.image.is_none() {
        announce_channel_migration(stack, r)?;
    }
    pull(stack, r)?;
    up(stack, r)?;
    Ok(wait_until_ready(stack, READY_TIMEOUT, r))
}

/// Pull a newer image and recreate the stack — the upgrade path. Plans the route
/// itself and walks every hop it needs; see [`apply_upgrade`] to show the operator
/// the route first. Returns whether the console came up before the timeout.
pub fn update(stack: &Stack, r: &mut dyn Reporter) -> Result<bool> {
    let plan = plan_upgrade(stack, None);
    apply_upgrade(stack, &plan, r)
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
pub fn apply_upgrade(stack: &Stack, plan: &UpgradePlan, r: &mut dyn Reporter) -> Result<bool> {
    ensure_installed(stack)?;
    r.stage(Stage::Preflight, "Checking Docker…", Some(0.02));
    preflight().require()?;
    announce_channel_migration(stack, r)?;

    if !plan.is_stepped() {
        pull(stack, r)?;
        up(stack, r)?;
        return Ok(wait_until_ready(stack, READY_TIMEOUT, r));
    }

    // A stepped route is the case where a snapshot is least optional: it is longer,
    // it crosses releases the operator never chose to run, and it is only ever
    // planned because one of them changes state in a way that can't be reversed by
    // running the old image again. Best effort, like the channel-switch snapshot —
    // converge's own per-hop rollback is the guarantee; this is the belt.
    let total = plan.steps.len();
    if status(stack).running {
        r.stage(Stage::Working, "Backing up before the first step…", Some(0.03));
        match backup(stack, Some("before-stepped-upgrade"), r) {
            Ok(path) => r.stage(
                Stage::Working,
                &format!("Snapshot saved: {}", path.display()),
                Some(0.05),
            ),
            Err(e) => r.stage(
                Stage::Working,
                &format!(
                    "Couldn't take a snapshot first ({e:#}) — continuing; each step \
                     rolls its own database back if it fails."
                ),
                Some(0.05),
            ),
        }
    }

    // Hops share the bar from 0.05 to 1.0, each rescaling the stage fractions the
    // shared pull/up/wait helpers report so a five-minute route doesn't look like
    // five separate runs that each finish at 100%.
    const ROUTE_START: f32 = 0.05;
    let span = (1.0 - ROUTE_START) / total as f32;

    for (i, step) in plan.steps.iter().enumerate() {
        let label = match step.version {
            Some(v) => v.to_string(),
            None => step.image.clone(),
        };
        let mut hop = ScaledReporter {
            inner: r,
            base: ROUTE_START + span * i as f32,
            span,
            prefix: format!("Step {} of {total} ({label}): ", i + 1),
        };
        hop.stage(
            Stage::Scaffold,
            &format!("switching to {}…", step.image),
            Some(0.0),
        );
        stack.set_image(&step.image)?;
        pull(stack, &mut hop)?;
        up(stack, &mut hop)?;
        let ready = wait_until_ready(stack, READY_TIMEOUT, &mut hop);

        if !ready {
            // Deliberately an error, not a `false`: on a multi-hop route "still
            // booting" and "this step refused or rolled back" look identical from
            // out here, and the difference decides whether the operator should
            // wait or read the logs. Only the LAST hop can honestly report
            // still-booting, and it returns below.
            if step.is_target {
                return Ok(false);
            }
            bail!(
                "step {} of {total} ({label}) didn't finish coming up, so the rest of \
                 the upgrade was not attempted. Your site is on {label} — its own \
                 migration either succeeded or rolled itself back, so it is not \
                 half-upgraded. Check `atelier app logs` for what happened, then run \
                 `atelier app update` again to continue.",
                i + 1
            );
        }
    }
    Ok(true)
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
    fn stage(&mut self, stage: Stage, message: &str, fraction: Option<f32>) {
        self.inner.stage(
            stage,
            &format!("{}{message}", self.prefix),
            fraction.map(|f| self.base + f * self.span),
        );
    }

    fn log(&mut self, line: &str) {
        self.inner.log(line);
    }

    fn captures_output(&self) -> bool {
        self.inner.captures_output()
    }
}

/// Run the one-time legacy-`:edge` → stable move, and say so out loud.
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
fn announce_channel_migration(stack: &Stack, r: &mut dyn Reporter) -> Result<()> {
    if !stack.pending_default_channel_migration() {
        return Ok(());
    }
    if status(stack).running {
        r.stage(Stage::Working, "Backing up before switching channels…", Some(0.09));
        match backup(stack, Some("before-channel-switch"), r) {
            Ok(path) => r.stage(
                Stage::Working,
                &format!("Snapshot saved: {}", path.display()),
                Some(0.1),
            ),
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
    Ok(())
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
    ensure_installed(stack)?;
    let image = stack.set_channel(channel)?;
    if !apply {
        return Ok((image, None));
    }
    preflight().require()?;
    pull(stack, r)?;
    up(stack, r)?;
    Ok((image, Some(wait_until_ready(stack, READY_TIMEOUT, r))))
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
pub(crate) const FILES_DIR: &str = "/opt/drupal/web/sites/default/files";

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
    let url = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http"));
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

/// The OCI label the image build stamps with the version it is (DECISIONS 0308).
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// The label carrying an image's upgrade floor — the oldest version whose database
/// its `converge.sh` will migrate. Baked from `docker/upgrade-floor`.
///
/// Read from the REGISTRY, which is the reason it is a label at all: the manager
/// has to know an image's floor before deciding to pull it, and the same value in
/// a file inside the image is only readable once the image is already downloaded.
const FLOOR_LABEL: &str = "dev.atelier.upgrade.min-from";

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
}

fn local_digest(image: &str) -> Option<String> {
    local_probe(image).ok().and_then(|p| p.digest)
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
             {{{{if .Config.Labels}}}}{{{{index .Config.Labels \"{FLOOR_LABEL}\"}}}}{{{{end}}}}"
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
    }
}

/// Read the registry's digest and version label for the same tag, without
/// pulling. `.Image` is keyed by platform for a multi-arch tag, so the version is
/// collected by ranging over it (identical across arches — verified: two
/// differently-stamped builds share byte-identical layers) rather than by naming
/// one platform the host may not be.
fn remote_probe(image: &str) -> std::result::Result<ImageProbe, String> {
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
             {{{{index $img.Config.Labels \"{FLOOR_LABEL}\"}}}}\
             {{{{end}}}}|{{{{end}}}}"
        ),
    ]);
    Ok(parse_remote_probe(&docker::probe(c)?))
}

/// Split `digest|version;floor|version;floor|` — one `version;floor` pair per
/// platform in a multi-arch tag. The pairs are identical across arches (verified:
/// two differently-stamped builds share byte-identical layers), so the first
/// populated one answers for the tag; ranging rather than naming a platform keeps
/// this working on a host whose arch we didn't guess.
fn parse_remote_probe(out: &str) -> ImageProbe {
    let mut parts = out.split('|');
    let digest = parts.next().unwrap_or_default();
    let (version, floor) = parts
        .map(|pair| {
            let (v, f) = pair.split_once(';').unwrap_or((pair, ""));
            (non_empty(v.trim()), Version::parse(f.trim()))
        })
        .find(|(v, f)| v.is_some() || f.is_some())
        .unwrap_or((None, None));
    ImageProbe {
        digest: non_empty(digest.trim()),
        version,
        floor,
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
    use super::{
        backup_script, is_backup_file, is_snapshot_bundle, list_backups, parse_http_status,
        parse_local_probe, parse_remote_probe, plan_route, plan_route_with, registry_problem,
        restore_bundle_script, waypoint_image, ImageProbe, Version, MAX_WAYPOINTS,
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
            },
            None,
        );
        assert!(!plan.is_stepped());
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.steps[0].is_target);
        assert!(plan.problem.is_none());
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
        assert_eq!(plan.steps[0].image, "ghcr.io/aincient-labs/atelier-cms:v0.3.0");
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
            vec![Version::parse("0.3.0"), Version::parse("0.5.0"), Version::parse("0.6.0")]
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
        assert!(problem.contains("no such host"), "keeps the cause: {problem}");
    }
}
