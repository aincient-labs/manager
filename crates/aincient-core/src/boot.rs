//! What a wait for the site found: ready, still booting, or failed (M3).
//!
//! The console answering is not enough to call a site ready. The appliance's
//! entrypoint starts the web server even after a failed converge, on the
//! rolled-back state (`cms/docker/entrypoint.sh`), so a site whose update or
//! database check failed answers HTTP anyway. And a converge that gave up on its
//! database leaves the site serving nothing for minutes while the wait counts
//! down. So [`crate::ops::wait_until_ready`] also reads the app's own log, by
//! Docker's clock, from the moment the container last started, and looks at the
//! containers themselves every few seconds ([`look`] + [`judge`]).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::docker::{self, compose, preflight, LOCAL_PROBE_TIMEOUT};
use crate::stack::Stack;

/// How a wait for the site came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// The console answered and the app's log shows no failed converge.
    Ready,
    /// Nothing failed that we could see, but it hadn't answered when the wait
    /// ran out.
    Booting,
    /// The site won't come up by itself; the reason in plain words.
    Failed(String),
}

impl Boot {
    /// `Ok(true)` ready, `Ok(false)` still booting; a failed boot is an error
    /// ([`BootFailed`]), so nothing after it (a backup prune, the next hop of
    /// an update) runs as if it had worked, and the CLI exits non-zero.
    pub fn ready(self) -> anyhow::Result<bool> {
        match self {
            Boot::Ready => Ok(true),
            Boot::Booting => Ok(false),
            Boot::Failed(why) => Err(BootFailed(why).into()),
        }
    }
}

/// The error a [`Boot::Failed`] becomes. Its text is the reason, already
/// phrased for the owner; the GUI finds it by type to show its failure panel.
#[derive(Debug)]
pub struct BootFailed(pub String);

impl fmt::Display for BootFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BootFailed {}

impl BootFailed {
    /// The owner's step is restarting Docker: its network is half attached,
    /// or Docker went away during the wait. Matched against our own wording
    /// (a multi-step update quotes it inside its own sentence), so the app
    /// can offer its Restart button.
    pub fn needs_docker_restart(&self) -> bool {
        self.0.contains(NETWORK_STUCK) || self.0.contains(DOCKER_STOPPED)
    }

    /// Docker stopped part of the site for lack of memory ([`out_of_memory`]),
    /// or likely did ([`likely_out_of_memory`]): the owner's step is giving
    /// Docker more memory, not a checkup.
    pub fn out_of_memory(&self) -> bool {
        self.0.contains(RAN_OUT_OF_MEMORY) || self.0.contains(LIKELY_FOR_LACK_OF_MEMORY)
    }
}

/// How often the wait looks at the containers and the app's log.
pub(crate) const LOOK_EVERY: Duration = Duration::from_secs(5);

const CHECKUP: &str = "Run a checkup (System → Troubleshoot, or `atelier doctor`) to see why.";

/// Docker's network for the site is half attached (review-tmp, 2026-11-14):
/// a running container the network lists has no address on it, or the reverse.
/// Detected, never repaired: only restarting Docker cleared it.
pub const NETWORK_STUCK: &str = "Docker's network for your site is only half connected, so \
     its parts can't reach each other. Restart Docker (`atelier docker restart`), then start \
     your site again. Your site's data is safe.";

pub(crate) const DOCKER_STOPPED: &str = "Docker stopped while your site was starting. Start \
     Docker again (`atelier docker restart`), then start your site again. Your site's data is \
     safe.";

const SITE_STOPPED: &str = "Your site stopped while it was starting. Start it again; if it \
     stops again, run a checkup (System → Troubleshoot, or `atelier doctor`).";

/// The words every out-of-memory reason carries ([`BootFailed::out_of_memory`]).
const RAN_OUT_OF_MEMORY: &str = "ran out of memory, so Docker stopped part of it";
const LIKELY_FOR_LACK_OF_MEMORY: &str = "likely for lack of memory";

/// What the owner calls each compose service.
fn part_name(service: &str) -> &str {
    match service {
        "db" => "the database",
        "app" => "the app",
        "edge" => "the web server",
        other => other,
    }
}

fn parts(services: &BTreeSet<String>) -> String {
    // db first: it is the one whose data the owner worries about.
    let mut names: Vec<&str> = services.iter().map(String::as_str).collect();
    names.sort_by_key(|s| (*s != "db", *s != "app", *s));
    names
        .iter()
        .map(|s| part_name(s))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Docker stopped `services` because the site ran out of memory: seen as
/// `State.OOMKilled` or an `oom` event, never from exit code 137 alone (our
/// own stops give 137 too). `remedy` is [`crate::docker_runtime::memory_remedy`].
pub(crate) fn out_of_memory(services: &BTreeSet<String>, remedy: &str) -> String {
    let safe = if services.contains("db") {
        " Your site's data is safe."
    } else {
        ""
    };
    format!(
        "Your site {RAN_OUT_OF_MEMORY} ({}).{safe} {remedy}, or stop other sites, then start \
         your site again.",
        parts(services)
    )
}

/// Docker killed `services` (exit 137, or Postgres's "terminated by signal 9")
/// with no out-of-memory flag or event to prove why: said as likely, not as fact.
pub(crate) fn likely_out_of_memory(services: &BTreeSet<String>, remedy: &str) -> String {
    format!(
        "Docker stopped part of your site by force ({}), {LIKELY_FOR_LACK_OF_MEMORY}. \
         {remedy}, or stop other sites, then start your site again. If it happens again, \
         {}",
        parts(services),
        lowercase_first(CHECKUP)
    )
}

/// `services` restarted during the wait with no sign of memory trouble.
pub(crate) fn crash_loop(services: &BTreeSet<String>) -> String {
    format!(
        "Part of your site ({}) keeps stopping and starting again, so it couldn't finish \
         starting. {CHECKUP}",
        parts(services)
    )
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_lowercase().chain(c).collect())
        .unwrap_or_default()
}

/// The lines that say a converge finished well: `converge.sh`'s own last line
/// (`log "converge OK — …"`, which a converge run by `docker exec` prints too)
/// and the entrypoint's `[entrypoint] converge succeeded`.
const CONVERGE_SUCCEEDED: [&str; 2] = ["[converge] converge OK", "[entrypoint] converge succeeded"];

/// The part of the app's log after the last converge that succeeded: a later
/// converge (doctor's re-converge repair, run inside the running container)
/// settles an earlier failure, so its FATAL must not keep failing every later
/// wait on the same container. The whole log when none succeeded.
pub(crate) fn after_last_success(log: &str) -> &str {
    let mut from = 0;
    let mut at = 0;
    for line in log.split_inclusive('\n') {
        at += line.len();
        if CONVERGE_SUCCEEDED.iter().any(|m| line.contains(m)) {
            from = at;
        }
    }
    &log[from..]
}

/// The reason a converge gave, in plain words, when the app's log says it
/// failed: `[converge] FATAL: …` (`converge.sh`'s `die`) or the entrypoint's
/// `converge FAILED` line, after the last converge that succeeded
/// ([`after_last_success`]). `None` when neither is there.
pub(crate) fn boot_failure(log: &str) -> Option<String> {
    let log = after_last_success(log);
    let fatal = log
        .lines()
        .find_map(|l| l.split_once("[converge] FATAL:").map(|(_, why)| why.trim()));
    let why = match fatal {
        Some(why) => why,
        None if log.lines().any(|l| l.contains("converge FAILED")) => "",
        None => return None,
    };
    let said = if why.contains("database not reachable") {
        "Your site can't reach its database, so it couldn't finish starting.".to_string()
    } else if why.contains("rolled back to pre-upgrade snapshot") {
        "The new version couldn't finish updating your site, so your site was put back \
         the way it was before the update."
            .to_string()
    } else if why.contains("installed modules absent from this image") {
        format!(
            "This version is missing parts your site uses, so it left your site as it was ({why})."
        )
    } else if why.contains("with an image that requires") {
        format!("Your site is too old for this version to update it directly ({why}).")
    } else if why.is_empty() {
        "Your site couldn't finish starting.".to_string()
    } else {
        format!("Your site couldn't finish starting ({why}).")
    };
    Some(format!("{said} {CHECKUP}"))
}

/// One look at a booting site.
#[derive(Debug, Default)]
pub(crate) struct Look {
    /// Docker answered.
    pub docker: bool,
    /// The `app` container, if there is one: running, and when it last started.
    pub app: Option<AppRun>,
    /// The app's log since it last started; `None` when that probe failed.
    pub log: Option<String>,
    /// `(network, container)` pairs Docker has half attached ([`half_attached`]).
    pub half_attached: Vec<(String, String)>,
    /// Every service's container (db, app, edge) as Docker sees it now.
    pub services: Vec<ServiceRun>,
}

#[derive(Debug, Clone)]
pub(crate) struct AppRun {
    pub name: String,
    /// Running, and not between two runs of a restart policy (Docker says
    /// `Running: true` while it waits to restart a container that died).
    pub running: bool,
    pub started_at: String,
}

/// One service's container at a look: what says it died or was killed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServiceRun {
    /// The container's ID: `docker events` name it by this.
    pub id: String,
    pub service: String,
    pub name: String,
    /// `State.OOMKilled`: a process in it was killed for lack of memory since
    /// it last started. Set for a killed child too (the container keeps
    /// running), and reset by every start, a restart policy's included.
    pub oom_killed: bool,
    /// `State.Restarting`: died, and Docker restarts it after a pause.
    pub restarting: bool,
    pub exit_code: i64,
    /// `RestartCount`: how often its restart policy restarted it.
    pub restart_count: u64,
}

/// What the looks so far add up to. Docker not answering, stopping and a
/// half-attached network have to be seen twice in a row (one [`LOOK_EVERY`]
/// apart) before they end the wait, so a container caught mid-start, or one
/// slow answer from a busy Docker, never fails a healthy boot.
#[derive(Debug, Default)]
pub(crate) struct Strikes {
    docker: u8,
    stopped: u8,
    network: u8,
    /// Looks in a row where a service was OOM-killed or had restarted.
    trouble: u8,
    /// Each container's `RestartCount` when the wait started ([`Self::from_baseline`]),
    /// or at the first look that saw it.
    baseline: HashMap<String, u64>,
    /// Services seen with `State.OOMKilled` during this wait.
    pub oom_killed: BTreeSet<String>,
    /// Services whose restart policy restarted them during this wait.
    pub restarted: BTreeSet<String>,
    /// Services seen [`killed_and_restarting`] during this wait.
    pub killed: BTreeSet<String>,
}

impl Strikes {
    /// Strikes counted against `services` as they were when the wait started.
    pub(crate) fn from_baseline(services: &[ServiceRun]) -> Self {
        Strikes {
            baseline: services
                .iter()
                .map(|s| (s.name.clone(), s.restart_count))
                .collect(),
            ..Strikes::default()
        }
    }

    /// Record what `look` says about memory and restarts; whether it saw any.
    fn note(&mut self, look: &Look) -> bool {
        let mut trouble = false;
        for s in &look.services {
            let base = *self
                .baseline
                .entry(s.name.clone())
                .or_insert(s.restart_count);
            if s.oom_killed {
                self.oom_killed.insert(s.service.clone());
                trouble = true;
            }
            if killed_and_restarting(s) {
                self.killed.insert(s.service.clone());
            }
            if s.restart_count > base || s.restarting {
                self.restarted.insert(s.service.clone());
                trouble = true;
            }
        }
        trouble
    }
}

/// Killed by SIGKILL (exit 137) and about to be started again by its restart
/// policy. Our own stops never leave a container restarting, so the kill was
/// the kernel's: in practice memory. Docker doesn't always say so: when the
/// container's main process is the one killed, CI's Docker sometimes set
/// neither `OOMKilled` nor an `oom` event across seven kills (2026-11-15).
pub(crate) fn killed_and_restarting(s: &ServiceRun) -> bool {
    s.restarting && s.exit_code == 137
}

/// Whether this look ends the wait, and why.
pub(crate) fn judge(look: &Look, strikes: &mut Strikes) -> Option<String> {
    if !look.docker {
        // Nothing else in this look is worth reading: every probe failed.
        strikes.docker += 1;
        return (strikes.docker >= 2).then(|| DOCKER_STOPPED.to_string());
    }
    strikes.docker = 0;
    let trouble = strikes.note(look);
    if let Some(why) = look.log.as_deref().and_then(boot_failure) {
        // The database is out of reach because the network is half attached:
        // the owner's step is restarting Docker, not a checkup.
        if why.contains("can't reach its database") && !look.half_attached.is_empty() {
            return Some(NETWORK_STUCK.to_string());
        }
        return Some(why);
    }
    // A service OOM-killed (its container may well keep running: a killed
    // child, or a restart policy that already started it again) or restarted
    // since the wait started. Its own words come from [`memory_verdict`] once
    // the wait ends; without memory trouble it is a crash loop.
    strikes.trouble = if trouble { strikes.trouble + 1 } else { 0 };
    if strikes.trouble >= 2 {
        let mut services = strikes.restarted.clone();
        services.extend(strikes.oom_killed.iter().cloned());
        return Some(crash_loop(&services));
    }
    let stopped = !look.app.as_ref().is_some_and(|a| a.running) || look.log.is_none();
    strikes.stopped = if stopped { strikes.stopped + 1 } else { 0 };
    if strikes.stopped >= 2 {
        return Some(SITE_STOPPED.to_string());
    }
    strikes.network = if look.half_attached.is_empty() {
        0
    } else {
        strikes.network + 1
    };
    if strikes.network >= 2 {
        return Some(NETWORK_STUCK.to_string());
    }
    None
}

/// `docker inspect` of one container, the fields a look reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Inspected {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "State", default)]
    state: InspectedState,
    #[serde(rename = "NetworkSettings", default)]
    network_settings: InspectedNetworks,
    #[serde(rename = "HostConfig", default)]
    host_config: InspectedHostConfig,
    #[serde(rename = "Config", default)]
    config: InspectedConfig,
    #[serde(rename = "RestartCount", default)]
    restart_count: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InspectedHostConfig {
    /// The network the container was created on (compose: `<project>_default`).
    #[serde(rename = "NetworkMode", default)]
    network_mode: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InspectedConfig {
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InspectedState {
    #[serde(rename = "Running", default)]
    running: bool,
    #[serde(rename = "StartedAt", default)]
    started_at: String,
    #[serde(rename = "Restarting", default)]
    restarting: bool,
    #[serde(rename = "OOMKilled", default)]
    oom_killed: bool,
    #[serde(rename = "ExitCode", default)]
    exit_code: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InspectedNetworks {
    #[serde(rename = "Networks", default)]
    networks: Option<HashMap<String, InspectedEndpoint>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InspectedEndpoint {
    #[serde(rename = "IPAddress", default)]
    ip_address: String,
    /// The names the others reach this container by on this network
    /// (compose: the container name and the service).
    #[serde(rename = "Aliases", default)]
    aliases: Option<Vec<String>>,
}

impl Inspected {
    /// The container name without `docker inspect`'s leading `/`.
    pub(crate) fn name(&self) -> &str {
        self.name.trim_start_matches('/')
    }

    /// The compose service this container runs.
    fn service(&self) -> Option<&str> {
        self.config
            .labels
            .as_ref()
            .and_then(|l| l.get("com.docker.compose.service"))
            .map(String::as_str)
    }

    fn run(&self) -> ServiceRun {
        ServiceRun {
            id: self.id.clone(),
            service: self.service().unwrap_or_default().to_string(),
            name: self.name().to_string(),
            oom_killed: self.state.oom_killed,
            restarting: self.state.restarting,
            exit_code: self.state.exit_code,
            restart_count: self.restart_count,
        }
    }

    /// The networks this container has an address on.
    fn addressed(&self) -> Vec<&str> {
        self.network_settings
            .networks
            .iter()
            .flatten()
            .filter(|(_, e)| !e.ip_address.is_empty())
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

/// A container that lost a network: one its compose service declares (or,
/// when the declared ones can't be read, the one it was created on, its
/// `HostConfig.NetworkMode`) holds no endpoint of it, so nothing on that
/// network can reach it by name. `docker network disconnect` on a stopped
/// container leaves exactly this (it drops the network from the container's
/// own settings), and neither `compose start`, `compose up` nor a Docker
/// restart puts it back (review-tmp, 2026-11-14: db running with no network,
/// the app "can't reach its database"). On an engine site edge declares two
/// (`<project>_default` and `<project>_engine`) and can lose either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detached {
    pub network: String,
    pub container: String,
    /// The compose service (`db`, `app`, …): the name the others reach it by.
    pub service: String,
    pub running: bool,
}

/// Each compose service's networks by their Docker names, from `docker
/// compose config --format json`: `services.<s>.networks` keys resolved
/// through the top-level `networks.<key>.name`. Read from the files compose
/// reads (engine overlay included), so it needs no container to remember
/// them; a container keeps no record of a network it was disconnected from.
/// `None` when the answer doesn't parse.
pub(crate) fn declared_networks(config_json: &str) -> Option<HashMap<String, Vec<String>>> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        services: HashMap<String, Service>,
        #[serde(default)]
        networks: HashMap<String, Option<Network>>,
    }
    #[derive(Deserialize)]
    struct Service {
        #[serde(default)]
        networks: Option<HashMap<String, serde_json::Value>>,
    }
    #[derive(Deserialize)]
    struct Network {
        #[serde(default)]
        name: Option<String>,
    }
    let config: Config = serde_json::from_str(config_json.trim()).ok()?;
    let name = |key: &str| {
        config
            .networks
            .get(key)
            .and_then(|n| n.as_ref())
            .and_then(|n| n.name.clone())
    };
    Some(
        config
            .services
            .iter()
            .map(|(service, s)| {
                let mut nets: Vec<String> = s
                    .networks
                    .iter()
                    .flatten()
                    .filter_map(|(key, _)| name(key))
                    .collect();
                nets.sort();
                (service.clone(), nets)
            })
            .collect(),
    )
}

/// The [`Detached`] containers among `containers`, against the networks each
/// service declares (`declared`, [`declared_networks`]), or each container's
/// `NetworkMode` for a service it doesn't list. Docker's built-in modes
/// (`default`, `bridge`, `host`, `none`, `container:…`) aren't a network a
/// container can lose this way.
pub(crate) fn detached(
    containers: &[Inspected],
    declared: Option<&HashMap<String, Vec<String>>>,
) -> Vec<Detached> {
    let mut out = Vec::new();
    for c in containers {
        let Some(service) = c.service() else {
            continue;
        };
        let expected: Vec<&str> = match declared.and_then(|d| d.get(service)) {
            Some(nets) if !nets.is_empty() => nets.iter().map(String::as_str).collect(),
            _ => vec![c.host_config.network_mode.as_str()],
        };
        for network in expected {
            if matches!(network, "" | "default" | "bridge" | "host" | "none")
                || network.starts_with("container:")
            {
                continue;
            }
            let attached = c
                .network_settings
                .networks
                .as_ref()
                .is_some_and(|n| n.contains_key(network));
            if !attached {
                out.push(Detached {
                    network: network.to_string(),
                    container: c.name().to_string(),
                    service: service.to_string(),
                    running: c.state.running,
                });
            }
        }
    }
    out
}

/// One endpoint to put back after a stale-endpoint clear: the container, the
/// network, and the names it answered to there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reconnect {
    pub network: String,
    pub container: String,
    pub aliases: Vec<String>,
}

/// For each `(network, container)` about to be force-disconnected, what puts it
/// back: that endpoint's own `Aliases` as `docker inspect` lists them before
/// the disconnect, or the compose service name when it lists none. Every
/// network, not only the one the container was created on: an engine site's
/// edge is on two, and cut off the engine's it can't reach the engine.
pub(crate) fn endpoints_to_restore(
    stale: &[(String, String)],
    inspected: &[Inspected],
) -> Vec<Reconnect> {
    stale
        .iter()
        .map(|(network, container)| {
            let c = inspected.iter().find(|c| c.name() == container);
            let mut aliases: Vec<String> = c
                .and_then(|c| c.network_settings.networks.as_ref())
                .and_then(|n| n.get(network))
                .and_then(|e| e.aliases.clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|a| !a.is_empty())
                .collect();
            if aliases.is_empty() {
                aliases.extend(c.and_then(Inspected::service).map(str::to_string));
            }
            Reconnect {
                network: network.clone(),
                container: container.clone(),
                aliases,
            }
        })
        .collect()
}

/// The site's [`Detached`] containers right now (every service, running or
/// not), or `None` when Docker didn't answer.
pub fn detached_now(stack: &Stack) -> Option<Vec<Detached>> {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "--format", "json"]);
    let out = docker::try_capture_empty_ok(ps)?;
    let names: Vec<String> = crate::ops::parse_ps(&out)
        .into_iter()
        .map(|e| e.name)
        .filter(|n| !n.is_empty())
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    // Unreadable: each container's own NetworkMode, as before.
    let mut config = compose(stack);
    config.args(["config", "--format", "json"]);
    let declared = docker::try_capture(config).and_then(|j| declared_networks(&j));
    Some(detached(&inspect(&names), declared.as_ref()))
}

pub(crate) fn parse_inspect(json: &str) -> Vec<Inspected> {
    serde_json::from_str(json.trim()).unwrap_or_default()
}

/// `docker network inspect` of one network: its name and the containers it
/// lists as attached.
#[derive(Debug, Deserialize)]
pub(crate) struct NetworkMembers {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Containers", default)]
    containers: Option<HashMap<String, NetworkMember>>,
}

#[derive(Debug, Deserialize)]
struct NetworkMember {
    #[serde(rename = "Name", default)]
    name: String,
}

impl NetworkMembers {
    fn lists(&self, container: &str) -> bool {
        self.containers
            .iter()
            .flatten()
            .any(|(_, m)| m.name == container)
    }
}

pub(crate) fn parse_network_inspect(json: &str) -> Vec<NetworkMembers> {
    serde_json::from_str(json.trim()).unwrap_or_default()
}

/// `(network, container)` for each running container that one of the site's
/// networks lists while the container has no address on it, or that has an
/// address on a network that doesn't list it. Docker's half-attached state
/// (review-tmp, 2026-11-14: the network listed db at .2, db itself had no
/// network; the app got "No route to host").
pub(crate) fn half_attached(
    networks: &[NetworkMembers],
    containers: &[Inspected],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in containers.iter().filter(|c| c.state.running) {
        let addressed = c.addressed();
        for n in networks {
            if n.lists(c.name()) != addressed.contains(&n.name.as_str()) {
                out.push((n.name.clone(), c.name().to_string()));
            }
        }
    }
    out
}

/// The site's `db` and `app` containers by `compose ps` (`name`, `service`),
/// or `None` when Docker didn't answer.
fn service_containers(stack: &Stack) -> Option<Vec<(String, String)>> {
    containers_of(stack, &["db", "app"])
}

/// The site's containers of `services` by `compose ps -a` (`name`, `service`),
/// or `None` when Docker didn't answer.
fn containers_of(stack: &Stack, services: &[&str]) -> Option<Vec<(String, String)>> {
    let mut ps = compose(stack);
    ps.args(["ps", "-a", "--format", "json"]);
    let out = docker::try_capture_empty_ok(ps)?;
    Some(
        crate::ops::parse_ps(&out)
            .into_iter()
            .filter(|e| services.contains(&e.service.as_str()) && !e.name.is_empty())
            .map(|e| (e.name, e.service))
            .collect(),
    )
}

pub(crate) fn inspect(names: &[&str]) -> Vec<Inspected> {
    if names.is_empty() {
        return Vec::new();
    }
    let mut c = docker::docker();
    c.arg("inspect").args(names);
    docker::try_capture(c)
        .map(|out| parse_inspect(&out))
        .unwrap_or_default()
}

/// The half-attached pairs among `containers`, over the site's own networks.
fn half_attached_among(stack: &Stack, containers: &[Inspected]) -> Vec<(String, String)> {
    let mut ls = docker::docker();
    ls.args([
        "network",
        "ls",
        "--filter",
        &format!("label=com.docker.compose.project={}", stack.project_name()),
        "--format",
        "{{.Name}}",
    ]);
    let names: Vec<String> = docker::try_capture(ls)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    let mut c = docker::docker();
    c.args(["network", "inspect"]).args(&names);
    let networks = docker::try_capture(c)
        .map(|out| parse_network_inspect(&out))
        .unwrap_or_default();
    half_attached(&networks, containers)
}

/// The site's half-attached `(network, container)` pairs right now, or `None`
/// when Docker didn't answer. For doctor.
pub fn half_attached_now(stack: &Stack) -> Option<Vec<(String, String)>> {
    let services = service_containers(stack)?;
    let names: Vec<&str> = services.iter().map(|(n, _)| n.as_str()).collect();
    Some(half_attached_among(stack, &inspect(&names)))
}

/// The app's log since its container last started, stdout and stderr
/// together in the order the container wrote them ([`interleave`]:
/// `converge.sh` reports FATAL on stderr and `converge OK` on stdout). `None`
/// when it can't be read.
fn app_log(app: &AppRun) -> Option<String> {
    let mut c = docker::docker();
    c.args([
        "logs",
        "--timestamps",
        "--since",
        &app.started_at,
        &app.name,
    ]);
    let out = docker::output_within(&mut c, LOCAL_PROBE_TIMEOUT).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(interleave(
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    ))
}

/// `docker logs --timestamps` output, its stdout and stderr (Docker hands them
/// back on separate pipes), as one log in the order the lines were written,
/// timestamps removed. Each line starts with Docker's fixed-width RFC 3339
/// time, so the two streams merge by comparing it; a tie keeps stdout first.
/// Reading one stream after the other put a later `converge OK` (stdout) before
/// the FATAL (stderr) it settled, so the site failed again on the old FATAL
/// (scenario `boot_fatal`).
pub(crate) fn interleave(stdout: &str, stderr: &str) -> String {
    merge_by_time(stdout, stderr, false)
}

/// [`interleave`], keeping each line's timestamp (the saved failure logs,
/// [`crate::update_logs`]).
pub(crate) fn interleave_timestamped(stdout: &str, stderr: &str) -> String {
    merge_by_time(stdout, stderr, true)
}

fn merge_by_time(stdout: &str, stderr: &str, keep_time: bool) -> String {
    fn split(line: &str) -> (&str, &str) {
        line.split_once(' ').unwrap_or(("", line))
    }
    let mut out = stdout.lines().map(split).peekable();
    let mut err = stderr.lines().map(split).peekable();
    let mut log = String::new();
    loop {
        let next = match (out.peek(), err.peek()) {
            (Some(o), Some(e)) if e.0 < o.0 => err.next(),
            (Some(_), _) => out.next(),
            (None, Some(_)) => err.next(),
            (None, None) => break,
        };
        if let Some((time, line)) = next {
            if keep_time && !time.is_empty() {
                log.push_str(time);
                log.push(' ');
            }
            log.push_str(line);
            log.push('\n');
        }
    }
    log
}

/// Look at the site. With `network`, also at how Docker attached its
/// containers (a few more `docker` calls).
pub(crate) fn look(stack: &Stack, network: bool) -> Look {
    let Some(services) = containers_of(stack, &SERVICES) else {
        return Look {
            docker: preflight().ok(),
            ..Look::default()
        };
    };
    let names: Vec<&str> = services.iter().map(|(n, _)| n.as_str()).collect();
    let inspected = inspect(&names);
    let app = services
        .iter()
        .find(|(_, s)| s == "app")
        .and_then(|(n, _)| inspected.iter().find(|i| i.name() == n))
        .map(|i| AppRun {
            name: i.name().to_string(),
            running: i.state.running && !i.state.restarting,
            started_at: i.state.started_at.clone(),
        });
    let log = app.as_ref().and_then(app_log);
    // A failed read is Docker going away, or the container with it.
    let docker = log.is_some() || preflight().ok();
    let half_attached = if network && docker {
        let db_app: Vec<Inspected> = inspected
            .iter()
            .filter(|i| matches!(i.service(), Some("db" | "app")))
            .cloned()
            .collect();
        half_attached_among(stack, &db_app)
    } else {
        Vec::new()
    };
    Look {
        docker,
        app,
        log,
        half_attached,
        services: inspected.iter().map(Inspected::run).collect(),
    }
}

/// The services a look inspects.
const SERVICES: [&str; 3] = ["db", "app", "edge"];

/// Every service's container now, for the [`Strikes`] baseline when a wait
/// starts. Empty when Docker didn't answer.
pub(crate) fn services_now(stack: &Stack) -> Vec<ServiceRun> {
    let Some(services) = containers_of(stack, &SERVICES) else {
        return Vec::new();
    };
    let names: Vec<&str> = services.iter().map(|(n, _)| n.as_str()).collect();
    inspect(&names).iter().map(Inspected::run).collect()
}

/// Seconds since the Unix epoch, as `docker events --since` takes them.
pub(crate) fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One `docker events --format '{{json .}}'` line, the fields read here.
#[derive(Debug, Deserialize)]
struct Event {
    #[serde(default)]
    id: String,
    #[serde(rename = "Action", default)]
    action: String,
    #[serde(rename = "Actor", default)]
    actor: EventActor,
}

#[derive(Debug, Default, Deserialize)]
struct EventActor {
    #[serde(rename = "Attributes", default)]
    attributes: HashMap<String, String>,
}

/// What `docker events` (oom + die) said about each service.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Events {
    /// Services with an `oom` event: proof that survives the restart.
    pub oom: BTreeSet<String>,
    /// Services that died with exit code 137 (SIGKILL: memory, or a stop).
    pub killed: BTreeSet<String>,
}

/// The events of the containers `current` lists (by ID; all of them when
/// `None`): a container an update replaced just before the wait died with 137
/// when it was stopped, and that says nothing about memory.
pub(crate) fn parse_events(out: &str, current: Option<&[ServiceRun]>) -> Events {
    let mut events = Events::default();
    for e in out
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l.trim()).ok())
        .filter(|e| current.is_none_or(|c| c.iter().any(|s| s.id == e.id)))
    {
        let Some(service) = e.actor.attributes.get("com.docker.compose.service") else {
            continue;
        };
        match e.action.as_str() {
            "oom" => {
                events.oom.insert(service.clone());
            }
            "die" if e.actor.attributes.get("exitCode").map(String::as_str) == Some("137") => {
                events.killed.insert(service.clone());
            }
            _ => {}
        }
    }
    events
}

/// The site's `oom` and `die` events from `since` (a `docker events --since`
/// value: Unix seconds or a duration like `24h`) until now. Docker keeps its
/// last 256 events in memory, so this outlives the restart that cleared
/// `State.OOMKilled`. Empty when Docker didn't answer.
pub(crate) fn events_since(stack: &Stack, since: &str, current: Option<&[ServiceRun]>) -> Events {
    let mut c = docker::docker();
    c.args([
        "events",
        "--since",
        since,
        "--until",
        &unix_secs(SystemTime::now()).saturating_add(1).to_string(),
        "--filter",
        &format!("label=com.docker.compose.project={}", stack.project_name()),
        "--filter",
        "event=oom",
        "--filter",
        "event=die",
        "--format",
        "{{json .}}",
    ]);
    docker::try_capture(c)
        .map(|out| parse_events(&out, current))
        .unwrap_or_default()
}

/// Postgres's own line when the kernel killed one of its processes
/// (`postmaster.c`: "server process (PID n) was terminated by signal 9:
/// Killed"). The database then restarts itself inside a running container.
pub(crate) fn postgres_killed(log: &str) -> bool {
    log.lines()
        .any(|l| l.contains("was terminated by signal 9"))
}

/// The db container's log since `since` (Unix seconds); empty when unreadable.
fn db_log_since(stack: &Stack, since: u64) -> String {
    let Some(db) = containers_of(stack, &["db"]).and_then(|c| c.into_iter().next()) else {
        return String::new();
    };
    let mut c = docker::docker();
    c.args(["logs", "--since", &since.to_string(), &db.0]);
    match docker::output_within(&mut c, LOCAL_PROBE_TIMEOUT) {
        Ok(out) => format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => String::new(),
    }
}

/// Everything that says memory stopped part of the site.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryEvidence {
    /// `State.OOMKilled` seen, or an `oom` event: out of memory for certain.
    pub oom: BTreeSet<String>,
    /// Killed (exit 137, or Postgres's signal-9 line) with no proof of why.
    pub killed: BTreeSet<String>,
}

impl MemoryEvidence {
    /// What the evidence says, in plain words; `None` when it says nothing.
    pub(crate) fn reason(&self, remedy: impl FnOnce() -> String) -> Option<String> {
        if !self.oom.is_empty() {
            return Some(out_of_memory(&self.oom, &remedy()));
        }
        if !self.killed.is_empty() {
            return Some(likely_out_of_memory(&self.killed, &remedy()));
        }
        None
    }
}

/// Put together what the looks saw (`seen`), the containers now, the site's
/// events since `since` and, for the database, Postgres's own log.
pub(crate) fn memory_evidence(
    seen: &BTreeSet<String>,
    seen_killed: &BTreeSet<String>,
    now: &[ServiceRun],
    events: Events,
    db_log: &str,
) -> MemoryEvidence {
    let mut oom = seen.clone();
    oom.extend(
        now.iter()
            .filter(|s| s.oom_killed)
            .map(|s| s.service.clone()),
    );
    oom.extend(events.oom);
    let mut killed = events.killed;
    killed.extend(seen_killed.iter().cloned());
    killed.extend(
        now.iter()
            .filter(|s| killed_and_restarting(s))
            .map(|s| s.service.clone()),
    );
    if postgres_killed(db_log) {
        killed.insert("db".into());
    }
    killed.retain(|s| !oom.contains(s));
    MemoryEvidence { oom, killed }
}

/// After a wait that failed or ran out: did memory stop part of the site?
/// Asks Docker once (inspect, `docker events` since the wait started, the
/// db's log) and names the runtime's memory setting only when it did.
pub(crate) fn memory_verdict(
    stack: &Stack,
    started: SystemTime,
    strikes: &Strikes,
) -> Option<String> {
    // A couple of seconds early: Docker's clock and ours may differ a little.
    let since = unix_secs(started).saturating_sub(2);
    let now = services_now(stack);
    let evidence = memory_evidence(
        &strikes.oom_killed,
        &strikes.killed,
        &now,
        events_since(stack, &since.to_string(), Some(&now)),
        &db_log_since(stack, since),
    );
    evidence.reason(crate::docker_runtime::memory_remedy_here)
}

/// Services OOM-killed now (`State.OOMKilled`) or by an `oom` event in the
/// last day, for doctor. `None` when Docker didn't answer.
pub fn out_of_memory_now(stack: &Stack) -> Option<BTreeSet<String>> {
    let services = containers_of(stack, &SERVICES)?;
    let names: Vec<&str> = services.iter().map(|(n, _)| n.as_str()).collect();
    let mut oom: BTreeSet<String> = inspect(&names)
        .iter()
        .filter(|i| i.state.oom_killed)
        .map(|i| i.service().unwrap_or_default().to_string())
        .collect();
    oom.extend(events_since(stack, "24h", None).oom);
    Some(oom)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docker inspect` of review-tmp's db and app on 2026-11-14, after a
    /// stale-endpoint recovery had force-disconnected the stopped db: the
    /// fields this module reads, values as Docker gave them (env left out:
    /// it holds the site's secrets).
    const REVIEW_TMP_DB_DETACHED: &str = r#"[{"Name":"/atelier-review-tmp-db-1","State":{"Running":true,"StartedAt":"2026-11-14T17:57:37.553866251Z"},"HostConfig":{"NetworkMode":"atelier-review-tmp_default"},"Config":{"Labels":{"com.docker.compose.project":"atelier-review-tmp","com.docker.compose.service":"db"}},"NetworkSettings":{"Networks":{}}},{"Name":"/atelier-review-tmp-app-1","State":{"Running":true,"StartedAt":"2026-11-14T17:57:37.555892626Z"},"HostConfig":{"NetworkMode":"atelier-review-tmp_default"},"Config":{"Labels":{"com.docker.compose.project":"atelier-review-tmp","com.docker.compose.service":"app"}},"NetworkSettings":{"Networks":{"atelier-review-tmp_default":{"IPAddress":"192.168.158.3","Aliases":["atelier-review-tmp-app-1","app"],"NetworkID":"9ad2edbed3147a61e5420854a6e6228b583728faa6fe6bb3d351a39963dd844b"}}}}]"#;

    #[test]
    fn a_container_that_lost_its_network_is_found_and_an_attached_one_is_not() {
        let containers = parse_inspect(REVIEW_TMP_DB_DETACHED);
        assert_eq!(containers.len(), 2, "both parse");
        assert_eq!(
            detached(&containers, None),
            vec![Detached {
                network: "atelier-review-tmp_default".into(),
                container: "atelier-review-tmp-db-1".into(),
                service: "db".into(),
                running: true,
            }]
        );
        // The half-attached check can't see it: neither side lists db.
        let net = parse_network_inspect(
            r#"[{"Name":"atelier-review-tmp_default","Containers":{"x":{"Name":"atelier-review-tmp-app-1"}}}]"#,
        );
        assert!(half_attached(&net, &containers).is_empty());
    }

    #[test]
    fn both_streams_merge_in_the_order_they_were_written() {
        // `docker logs --timestamps` of a site whose converge failed, then a
        // later converge (doctor's re-converge) succeeded in the same run.
        let stdout = "2026-11-14T20:00:01.000000000Z [entrypoint] starting\n\
                      2026-11-14T20:05:00.000000000Z [converge] converge OK — site is on 11.2.5\n";
        let stderr =
            "2026-11-14T20:00:02.000000000Z [converge] FATAL: post-update health check failed\n\
                      2026-11-14T20:00:02.000000000Z [entrypoint] converge FAILED\n";
        let log = interleave(stdout, stderr);
        assert_eq!(
            log,
            "[entrypoint] starting\n\
             [converge] FATAL: post-update health check failed\n\
             [entrypoint] converge FAILED\n\
             [converge] converge OK — site is on 11.2.5\n"
        );
        assert_eq!(boot_failure(&log), None, "the later success settles it");
        // Read one stream after the other, the old FATAL came last.
        assert!(boot_failure(&format!("{stdout}{stderr}")).is_some());
        // A tie keeps stdout first; one stream alone is itself.
        assert_eq!(interleave("T a\n", "T b\n"), "a\nb\n");
        assert_eq!(interleave("", "T b\nT c\n"), "b\nc\n");
    }

    #[test]
    fn built_in_network_modes_are_never_detached() {
        for mode in ["default", "bridge", "host", "none", "container:abc"] {
            let json = format!(
                r#"[{{"Name":"/x","State":{{"Running":true}},"HostConfig":{{"NetworkMode":"{mode}"}},"Config":{{"Labels":{{"com.docker.compose.service":"db"}}}},"NetworkSettings":{{"Networks":{{}}}}}}]"#
            );
            assert!(detached(&parse_inspect(&json), None).is_empty(), "{mode}");
        }
    }

    /// `docker inspect atelier-hardening-test-db-1 atelier-hardening-test-edge-1`
    /// of the stopped site (2026-11-14), the keys this module reads (env left
    /// out). A stopped container keeps each endpoint's `Aliases`.
    const STOPPED_DB_EDGE: &str = r#"[{"Name": "/atelier-hardening-test-db-1", "State": {"Running": false, "StartedAt": "2026-11-14T18:31:17.703037277Z"}, "HostConfig": {"NetworkMode": "atelier-hardening-test_default"}, "Config": {"Labels": {"com.docker.compose.project": "atelier-hardening-test", "com.docker.compose.service": "db"}}, "NetworkSettings": {"Networks": {"atelier-hardening-test_default": {"IPAddress": "", "Aliases": ["atelier-hardening-test-db-1", "db"], "DNSNames": ["atelier-hardening-test-db-1", "db", "4232a084c221"], "NetworkID": "7e7b5181fa0717cda34445873c35d89ada0974307719253720fe09ff5cf09b78"}}}},
    {"Name": "/atelier-hardening-test-edge-1", "State": {"Running": false, "StartedAt": "2026-11-14T18:31:28.472232817Z"}, "HostConfig": {"NetworkMode": "atelier-hardening-test_default"}, "Config": {"Labels": {"com.docker.compose.project": "atelier-hardening-test", "com.docker.compose.service": "edge"}}, "NetworkSettings": {"Networks": {"atelier-hardening-test_default": {"IPAddress": "", "Aliases": ["atelier-hardening-test-edge-1", "edge"], "DNSNames": ["atelier-hardening-test-edge-1", "edge", "af644983b511"], "NetworkID": "7e7b5181fa0717cda34445873c35d89ada0974307719253720fe09ff5cf09b78"}}}}]"#;

    /// The services' `networks` and the top-level `networks` of `docker compose
    /// config --format json` over compose.yaml + the engine overlay
    /// (`ENGINE_COMPOSE_TEMPLATE`), project `atelier-hardening-test`, Compose
    /// v5.1.2 (everything else, environment included, left out).
    const ENGINE_CONFIG: &str = r#"{"services": {"app": {"networks": {"default": null}}, "db": {"networks": {"default": null}}, "edge": {"networks": {"default": null, "engine": null}}, "engine": {"networks": {"engine": null}}},
      "networks": {"default": {"name": "atelier-hardening-test_default", "ipam": {}}, "engine": {"name": "atelier-hardening-test_engine", "ipam": {}, "internal": true}}}"#;

    /// [`STOPPED_DB_EDGE`]'s edge as on an engine site: on the engine network
    /// too, with the aliases compose gives every endpoint.
    fn engine_edge() -> String {
        STOPPED_DB_EDGE.replace(
            r#""Aliases": ["atelier-hardening-test-edge-1", "edge"], "DNSNames": ["atelier-hardening-test-edge-1", "edge", "af644983b511"], "NetworkID": "7e7b5181fa0717cda34445873c35d89ada0974307719253720fe09ff5cf09b78"}"#,
            r#""Aliases": ["atelier-hardening-test-edge-1", "edge"], "DNSNames": ["atelier-hardening-test-edge-1", "edge", "af644983b511"], "NetworkID": "7e7b5181fa0717cda34445873c35d89ada0974307719253720fe09ff5cf09b78"}, "atelier-hardening-test_engine": {"IPAddress": "", "Aliases": ["atelier-hardening-test-edge-1", "edge"], "NetworkID": "2b1f0c9e8d7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c"}"#,
        )
    }

    #[test]
    fn a_cleared_container_is_put_back_on_every_network_under_its_own_names() {
        let inspected = parse_inspect(&engine_edge());
        let stale = [
            (
                "atelier-hardening-test_default".to_string(),
                "atelier-hardening-test-edge-1".to_string(),
            ),
            (
                "atelier-hardening-test_engine".to_string(),
                "atelier-hardening-test-edge-1".to_string(),
            ),
            (
                "atelier-hardening-test_default".to_string(),
                "atelier-hardening-test-db-1".to_string(),
            ),
        ];
        let names = |n: &str| Reconnect {
            network: n.split('|').next().unwrap().into(),
            container: n.split('|').nth(1).unwrap().into(),
            aliases: vec![
                n.split('|').nth(1).unwrap().into(),
                n.split('|').nth(2).unwrap().into(),
            ],
        };
        assert_eq!(
            endpoints_to_restore(&stale, &inspected),
            [
                names("atelier-hardening-test_default|atelier-hardening-test-edge-1|edge"),
                // Before 0.17.0's fix only NetworkMode came back: edge lost
                // the engine for good.
                names("atelier-hardening-test_engine|atelier-hardening-test-edge-1|edge"),
                names("atelier-hardening-test_default|atelier-hardening-test-db-1|db"),
            ]
        );
        // An endpoint that lists no aliases goes back under its service name.
        let bare =
            STOPPED_DB_EDGE.replace(r#""Aliases": ["atelier-hardening-test-db-1", "db"], "#, "");
        assert_eq!(
            endpoints_to_restore(&stale[2..], &parse_inspect(&bare))[0].aliases,
            ["db"]
        );
    }

    #[test]
    fn a_container_missing_any_network_its_service_declares_is_detached() {
        let declared = declared_networks(ENGINE_CONFIG).unwrap();
        assert_eq!(
            declared["edge"],
            [
                "atelier-hardening-test_default",
                "atelier-hardening-test_engine"
            ]
        );
        assert_eq!(declared["engine"], ["atelier-hardening-test_engine"]);
        // Edge after a clear that only put NetworkMode back: still on default,
        // gone from the engine network. NetworkMode alone can't see it.
        let only_default = parse_inspect(STOPPED_DB_EDGE);
        assert!(detached(&only_default, None).is_empty());
        assert_eq!(
            detached(&only_default, Some(&declared)),
            [Detached {
                network: "atelier-hardening-test_engine".into(),
                container: "atelier-hardening-test-edge-1".into(),
                service: "edge".into(),
                running: false,
            }]
        );
        // Both networks there: nothing to do.
        assert!(detached(&parse_inspect(&engine_edge()), Some(&declared)).is_empty());
        // A service the config doesn't list falls back to its NetworkMode.
        let unlisted = declared_networks(r#"{"services": {}, "networks": {}}"#).unwrap();
        assert!(detached(&only_default, Some(&unlisted)).is_empty());
        assert_eq!(declared_networks("not json"), None);
    }

    /// `docker inspect atelier-hardening-test-app-1 atelier-hardening-test-db-1`,
    /// trimmed to the keys a look reads (2026-11-14), with db as review-tmp had
    /// it: running and healthy, but with no network at all.
    const INSPECT_DB_DETACHED: &str = r#"[
      {
        "Id": "9772e359133ddd3c21adfb553555d1cd1ac0b1caf54c4d310d0e50b93ffffd09",
        "Name": "/atelier-hardening-test-app-1",
        "State": {"Status": "running", "Running": true, "Paused": false, "Restarting": false,
          "OOMKilled": false, "Dead": false, "Pid": 1544735, "ExitCode": 0, "Error": "",
          "StartedAt": "2026-11-14T12:08:06.529116477Z", "FinishedAt": "2026-11-14T12:07:34.979414189Z"},
        "NetworkSettings": {"Networks": {"atelier-hardening-test_default": {
          "IPAMConfig": null, "Links": null, "Aliases": ["atelier-hardening-test-app-1", "app"],
          "NetworkID": "17bfd5038b0e6ed9747e209d1b67d967e8166c2aed2e3e5642821902faf70027",
          "EndpointID": "f830e3d61794ffef0c944df5e8c6db7014818219433ffdd6f45d2bf19f2645ab",
          "Gateway": "192.168.171.1", "IPAddress": "192.168.171.3", "IPPrefixLen": 24,
          "DNSNames": ["atelier-hardening-test-app-1", "app", "9772e359133d"]}}}
      },
      {
        "Id": "561110aa79767cf82dfbfec002ef65cd08d44222ee5fd1bca8146f348013503d",
        "Name": "/atelier-hardening-test-db-1",
        "State": {"Status": "running", "Running": true, "Paused": false, "Restarting": false,
          "OOMKilled": false, "Dead": false, "Pid": 1544176, "ExitCode": 0, "Error": "",
          "StartedAt": "2026-11-14T12:07:55.823410098Z", "FinishedAt": "2026-11-14T12:07:35.339974162Z"},
        "NetworkSettings": {"Networks": {}}
      }
    ]"#;

    /// `docker network inspect atelier-hardening-test_default`, trimmed: the
    /// network still lists db at .2.
    const NETWORK: &str = r#"[
      {
        "Name": "atelier-hardening-test_default",
        "Id": "17bfd5038b0e6ed9747e209d1b67d967e8166c2aed2e3e5642821902faf70027",
        "Scope": "local", "Driver": "bridge",
        "Labels": {"com.docker.compose.network": "default",
          "com.docker.compose.project": "atelier-hardening-test"},
        "Containers": {
          "3ca00c913a8cabed1aaa5237eea8f2a63e9c2a0c2e846f0990e1c8b569b29c45": {
            "Name": "atelier-hardening-test-edge-1",
            "EndpointID": "5719b2f208900b24bc7cffe71e4ff0dad8d29f737eb56ff0b62a311ad5f78c9e",
            "MacAddress": "0a:3f:d6:ab:3c:b3", "IPv4Address": "192.168.171.4/24", "IPv6Address": ""},
          "561110aa79767cf82dfbfec002ef65cd08d44222ee5fd1bca8146f348013503d": {
            "Name": "atelier-hardening-test-db-1",
            "EndpointID": "9cc70a7144e30a974f4d12eab74bc62096989e613f6b31d0765421110412e527",
            "MacAddress": "8a:a9:90:47:4f:8c", "IPv4Address": "192.168.171.2/24", "IPv6Address": ""},
          "9772e359133ddd3c21adfb553555d1cd1ac0b1caf54c4d310d0e50b93ffffd09": {
            "Name": "atelier-hardening-test-app-1",
            "EndpointID": "f830e3d61794ffef0c944df5e8c6db7014818219433ffdd6f45d2bf19f2645ab",
            "MacAddress": "6a:5a:62:a7:0c:ee", "IPv4Address": "192.168.171.3/24", "IPv6Address": ""}
        }
      }
    ]"#;

    #[test]
    fn a_running_container_the_network_lists_without_an_address_is_half_attached() {
        let found = half_attached(
            &parse_network_inspect(NETWORK),
            &parse_inspect(INSPECT_DB_DETACHED),
        );
        assert_eq!(
            found,
            [(
                "atelier-hardening-test_default".to_string(),
                "atelier-hardening-test-db-1".to_string()
            )]
        );
    }

    #[test]
    fn a_container_with_an_address_the_network_doesnt_list_is_half_attached() {
        // The reverse: app has its address, the network forgot it.
        let healthy_db = INSPECT_DB_DETACHED.replace(
            r#""NetworkSettings": {"Networks": {}}"#,
            r#""NetworkSettings": {"Networks": {"atelier-hardening-test_default": {"IPAddress": "192.168.171.2"}}}"#,
        );
        let mut network = parse_network_inspect(NETWORK);
        network[0]
            .containers
            .as_mut()
            .unwrap()
            .retain(|_, m| m.name != "atelier-hardening-test-app-1");
        assert_eq!(
            half_attached(&network, &parse_inspect(&healthy_db)),
            [(
                "atelier-hardening-test_default".to_string(),
                "atelier-hardening-test-app-1".to_string()
            )]
        );
    }

    #[test]
    fn an_attached_site_or_a_stopped_container_is_not_half_attached() {
        let healthy = INSPECT_DB_DETACHED.replace(
            r#""NetworkSettings": {"Networks": {}}"#,
            r#""NetworkSettings": {"Networks": {"atelier-hardening-test_default": {"IPAddress": "192.168.171.2"}}}"#,
        );
        assert!(
            half_attached(&parse_network_inspect(NETWORK), &parse_inspect(&healthy)).is_empty()
        );
        // A stopped db keeps no address; that is stale_endpoints' case, not this one.
        let mut stopped = parse_inspect(INSPECT_DB_DETACHED);
        stopped[1].state.running = false;
        assert!(half_attached(&parse_network_inspect(NETWORK), &stopped).is_empty());
    }

    /// The tail of `docker logs` of an app whose database stayed out of reach
    /// (converge.sh `wait_for_db`, then entrypoint.sh).
    const DB_UNREACHABLE: &str = "[converge] waiting for database… (56s)\n\
        [converge] waiting for database… (58s)\n\
        [converge] FATAL: database not reachable after 60s\n\
        [entrypoint] converge FAILED — starting server on the rolled-back state for inspection\n";

    #[test]
    fn a_failed_converge_reads_in_plain_words() {
        let why = boot_failure(DB_UNREACHABLE).unwrap();
        assert!(
            why.starts_with("Your site can't reach its database"),
            "{why}"
        );
        assert!(why.contains("atelier doctor"));
        let rolled = boot_failure(
            "[converge] upgrade failed — the converged site is not healthy\n\
             [converge] FATAL: rolled back to pre-upgrade snapshot\n\
             [entrypoint] converge FAILED — starting server on the rolled-back state for inspection\n",
        )
        .unwrap();
        assert!(
            rolled.contains("put back the way it was before the update"),
            "{rolled}"
        );
        assert_eq!(
            boot_failure("[converge] FATAL: post-install health check failed\n").unwrap(),
            format!(
                "Your site couldn't finish starting (post-install health check failed). {CHECKUP}"
            )
        );
        // The entrypoint's line alone still counts.
        assert!(boot_failure(
            "[entrypoint] converge FAILED — starting server on the rolled-back state for inspection"
        )
        .unwrap()
        .starts_with("Your site couldn't finish starting."));
    }

    #[test]
    fn a_good_boot_log_is_not_a_failure() {
        // `docker logs --since <StartedAt> atelier-hardening-test-app-1`, 2026-11-14.
        let ok = "[converge] database is reachable\n\
            [converge] pgvector extension present\n\
            [converge] database is populated\n\
            [converge] this build already converged this database → restart (no snapshot, updates, import, rebuild or health gate)\n\
            [converge] converge OK — restart of the build that converged this site\n\
            [entrypoint] converge succeeded\n";
        assert_eq!(boot_failure(ok), None);
    }

    #[test]
    fn a_later_successful_converge_settles_an_earlier_failure() {
        // The app booted with its database out of reach; doctor's re-converge
        // (`docker exec … converge.sh`, its output mirrored into the app's log)
        // then fixed it without restarting the container.
        let fixed = format!(
            "{DB_UNREACHABLE}\
            [converge] database is reachable\n\
            [converge] pgvector extension present\n\
            [converge] database is populated\n\
            [converge] this build already converged this database → restart (no snapshot, updates, import, rebuild or health gate)\n\
            [converge] converge OK — restart of the build that converged this site\n"
        );
        assert_eq!(boot_failure(&fixed), None);
        // A fresh install's last line, then the entrypoint's.
        let installed = format!(
            "{DB_UNREACHABLE}[converge] converge OK — site is on 11.2.5\n\
             [entrypoint] converge succeeded\n"
        );
        assert_eq!(boot_failure(&installed), None);
        // A failure after the success still counts.
        let failed_again = format!("{fixed}[converge] FATAL: database not reachable after 60s\n");
        assert!(boot_failure(&failed_again)
            .unwrap()
            .starts_with("Your site can't reach its database"));
        // No trailing newline on the success line: nothing after it.
        assert_eq!(
            boot_failure("[converge] FATAL: x\n[entrypoint] converge succeeded"),
            None
        );
    }

    fn running_app() -> Option<AppRun> {
        Some(AppRun {
            name: "atelier-hardening-test-app-1".into(),
            running: true,
            started_at: "2026-11-14T12:08:06.529116477Z".into(),
        })
    }

    #[test]
    fn a_failed_converge_ends_the_wait_at_once() {
        let look = Look {
            docker: true,
            app: running_app(),
            log: Some(DB_UNREACHABLE.into()),
            half_attached: Vec::new(),
            services: Vec::new(),
        };
        let why = judge(&look, &mut Strikes::default()).unwrap();
        assert!(why.contains("can't reach its database"));
    }

    #[test]
    fn an_unreachable_database_on_a_half_attached_network_says_restart_docker() {
        let look = Look {
            docker: true,
            app: running_app(),
            log: Some(DB_UNREACHABLE.into()),
            half_attached: vec![("n".into(), "atelier-hardening-test-db-1".into())],
            services: Vec::new(),
        };
        assert_eq!(
            judge(&look, &mut Strikes::default()).as_deref(),
            Some(NETWORK_STUCK)
        );
        assert!(NETWORK_STUCK.contains("Restart Docker (`atelier docker restart`)"));
        assert!(!NETWORK_STUCK.contains("Docker Desktop"));
        assert!(BootFailed(NETWORK_STUCK.into()).needs_docker_restart());
    }

    #[test]
    fn docker_going_away_ends_the_wait_on_the_second_look_and_says_so() {
        let gone = Look::default();
        let mut strikes = Strikes::default();
        assert_eq!(judge(&gone, &mut strikes), None, "one slow answer");
        assert_eq!(judge(&gone, &mut strikes).as_deref(), Some(DOCKER_STOPPED));
    }

    #[test]
    fn one_slow_docker_answer_between_good_looks_never_fails_a_boot() {
        let mut strikes = Strikes::default();
        let good = Look {
            docker: true,
            app: running_app(),
            log: Some("[converge] waiting for database… (0s)\n".into()),
            half_attached: Vec::new(),
            services: Vec::new(),
        };
        for _ in 0..3 {
            assert_eq!(judge(&Look::default(), &mut strikes), None);
            assert_eq!(judge(&good, &mut strikes), None);
        }
    }

    #[test]
    fn the_app_stopping_ends_the_wait_on_the_second_look() {
        let mut strikes = Strikes::default();
        let stopped = Look {
            docker: true,
            app: Some(AppRun {
                running: false,
                ..running_app().unwrap()
            }),
            log: Some(String::new()),
            half_attached: Vec::new(),
            services: Vec::new(),
        };
        assert_eq!(judge(&stopped, &mut strikes), None);
        assert_eq!(judge(&stopped, &mut strikes).as_deref(), Some(SITE_STOPPED));
        // Gone altogether (no app container, so no log either).
        let mut strikes = Strikes::default();
        let gone = Look {
            docker: true,
            ..Look::default()
        };
        assert_eq!(judge(&gone, &mut strikes), None);
        assert_eq!(judge(&gone, &mut strikes).as_deref(), Some(SITE_STOPPED));
    }

    #[test]
    fn one_odd_look_between_good_ones_never_fails_a_boot() {
        let mut strikes = Strikes::default();
        let good = Look {
            docker: true,
            app: running_app(),
            log: Some("[converge] waiting for database… (0s)\n".into()),
            half_attached: Vec::new(),
            services: Vec::new(),
        };
        let odd = Look {
            docker: true,
            app: Some(AppRun {
                running: false,
                ..running_app().unwrap()
            }),
            log: Some(String::new()),
            half_attached: vec![("n".into(), "c".into())],
            services: Vec::new(),
        };
        for _ in 0..3 {
            assert_eq!(judge(&odd, &mut strikes), None);
            assert_eq!(judge(&good, &mut strikes), None);
        }
        // Half attached twice in a row ends it.
        let stuck = Look {
            half_attached: vec![("n".into(), "c".into())],
            ..good
        };
        assert_eq!(judge(&stuck, &mut strikes), None);
        assert_eq!(judge(&stuck, &mut strikes).as_deref(), Some(NETWORK_STUCK));
    }

    #[test]
    fn a_failed_boot_is_an_error_a_slow_one_is_not() {
        assert!(Boot::Ready.ready().unwrap());
        assert!(!Boot::Booting.ready().unwrap());
        let e = Boot::Failed("Your site can't reach its database".into())
            .ready()
            .unwrap_err();
        assert_eq!(
            e.downcast_ref::<BootFailed>().map(|b| b.0.as_str()),
            Some("Your site can't reach its database")
        );
    }

    fn svc(service: &str, oom: bool, restarting: bool, count: u64) -> ServiceRun {
        ServiceRun {
            id: format!("id-{service}"),
            service: service.into(),
            name: format!("atelier-x-{service}-1"),
            oom_killed: oom,
            restarting,
            exit_code: if oom { 137 } else { 0 },
            restart_count: count,
        }
    }

    fn booting(services: Vec<ServiceRun>) -> Look {
        Look {
            docker: true,
            app: running_app(),
            log: Some("[converge] waiting for database… (0s)\n".into()),
            half_attached: Vec::new(),
            services,
        }
    }

    /// `docker inspect` of a container whose restart policy is about to start
    /// it again after an OOM kill (OrbStack, Docker 29.4.0, 2026-11-14): Docker
    /// says `Running: true` while it waits.
    const INSPECT_OOM_RESTARTING: &str = r#"[{"Name":"/atelier-scn-oom-app-1","RestartCount":5,"State":{"Status":"restarting","Running":true,"Restarting":true,"OOMKilled":true,"ExitCode":137,"StartedAt":"2026-11-14T20:56:00.1Z"},"Config":{"Labels":{"com.docker.compose.service":"app"}}}]"#;

    #[test]
    fn a_container_between_restarts_is_not_running_and_carries_its_oom_flag() {
        let i = parse_inspect(INSPECT_OOM_RESTARTING);
        let run = i[0].run();
        assert_eq!(
            run,
            ServiceRun {
                id: String::new(),
                service: "app".into(),
                name: "atelier-scn-oom-app-1".into(),
                oom_killed: true,
                restarting: true,
                exit_code: 137,
                restart_count: 5,
            }
        );
    }

    #[test]
    fn a_service_killed_or_restarted_twice_in_a_row_ends_the_wait() {
        let base = vec![svc("db", false, false, 0), svc("app", false, false, 2)];
        // OOMKilled on a running container: a killed child process.
        let mut strikes = Strikes::from_baseline(&base);
        let child = booting(vec![svc("db", false, false, 0), svc("app", true, false, 2)]);
        assert_eq!(judge(&child, &mut strikes), None, "one look is not enough");
        let why = judge(&child, &mut strikes).unwrap();
        assert!(
            why.contains("(the app) keeps stopping and starting again"),
            "{why}"
        );
        assert!(strikes.oom_killed.contains("app"));
        // RestartCount above the baseline, the flag already reset by the restart.
        let mut strikes = Strikes::from_baseline(&base);
        let looping = booting(vec![
            svc("db", false, false, 1),
            svc("app", false, false, 2),
        ]);
        assert_eq!(judge(&looping, &mut strikes), None);
        assert!(judge(&looping, &mut strikes)
            .unwrap()
            .contains("(the database)"));
        assert!(strikes.oom_killed.is_empty());
        assert_eq!(strikes.restarted, BTreeSet::from(["db".to_string()]));
        // Restart counts a site had before the wait never count.
        let mut strikes = Strikes::from_baseline(&base);
        let calm = booting(base.clone());
        for _ in 0..3 {
            assert_eq!(judge(&calm, &mut strikes), None);
        }
        // A container the baseline missed starts its own baseline.
        let mut strikes = Strikes::default();
        let late = booting(vec![svc("edge", false, false, 4)]);
        for _ in 0..3 {
            assert_eq!(judge(&late, &mut strikes), None);
        }
    }

    /// `docker events --format '{{json .}}'` of an OOM-killed scenario app
    /// (OrbStack, Docker 29.4.0, 2026-11-14), the compose labels Docker copies
    /// into the attributes, trimmed.
    const EVENTS: &str = r#"{"status":"oom","id":"bc41","Type":"container","Action":"oom","Actor":{"ID":"bc41","Attributes":{"com.docker.compose.project":"atelier-scn-oom","com.docker.compose.service":"app","image":"atelier-scn-app:x","name":"atelier-scn-oom-app-1"}},"scope":"local","time":1794689000}
{"status":"die","id":"bc41","Type":"container","Action":"die","Actor":{"ID":"bc41","Attributes":{"com.docker.compose.project":"atelier-scn-oom","com.docker.compose.service":"app","execDuration":"0","exitCode":"137","name":"atelier-scn-oom-app-1"}},"scope":"local","time":1794689000}
{"status":"die","id":"ed9e","Type":"container","Action":"die","Actor":{"ID":"ed9e","Attributes":{"com.docker.compose.project":"atelier-scn-oom","com.docker.compose.service":"edge","exitCode":"137","name":"atelier-scn-oom-edge-1"}},"scope":"local","time":1794689001}
{"status":"die","id":"ed9f","Type":"container","Action":"die","Actor":{"ID":"ed9f","Attributes":{"com.docker.compose.service":"db","exitCode":"0"}},"time":1794689002}
not json"#;

    #[test]
    fn docker_events_name_the_oom_killed_and_the_killed_services() {
        // Only the containers there now: edge's 137 came from one replaced
        // before the wait.
        let now = [ServiceRun {
            id: "bc41".into(),
            ..svc("app", false, false, 0)
        }];
        let e = parse_events(EVENTS, Some(&now));
        assert_eq!(e.oom, BTreeSet::from(["app".to_string()]));
        assert_eq!(e.killed, BTreeSet::from(["app".to_string()]));
        let e = parse_events(EVENTS, None);
        assert_eq!(e.oom, BTreeSet::from(["app".to_string()]));
        assert_eq!(
            e.killed,
            BTreeSet::from(["app".to_string(), "edge".to_string()])
        );
    }

    const PG_KILLED: &str = "2026-11-14 20:56:53.894 UTC [1] LOG:  server process (PID 63) was terminated by signal 9: Killed\n\
        2026-11-14 20:56:53.894 UTC [1] LOG:  terminating any other active server processes\n";

    #[test]
    fn a_container_killed_and_restarting_counts_as_killed_without_the_flag_or_events() {
        // CI's Docker, 2026-11-15: seven kills of the app's main process,
        // `Restarting` with exit 137, and neither `OOMKilled` nor an event.
        let killed = ServiceRun {
            exit_code: 137,
            ..svc("app", false, true, 7)
        };
        assert!(killed_and_restarting(&killed));
        assert!(
            !killed_and_restarting(&svc("app", false, false, 7)),
            "our own stop: 137, not restarting"
        );
        let mut strikes = Strikes::from_baseline(&[svc("app", false, false, 0)]);
        assert!(strikes.note(&booting(vec![killed.clone()])));
        assert_eq!(strikes.killed, BTreeSet::from(["app".to_string()]));
        let remedy = || "Give Docker more memory".to_string();
        // Seen during the wait, though restarted by the time the wait ended.
        let e = memory_evidence(
            &BTreeSet::new(),
            &strikes.killed,
            &[svc("app", false, false, 8)],
            Events::default(),
            "",
        );
        let why = e.reason(remedy).unwrap();
        assert!(
            why.contains("(the app), likely for lack of memory"),
            "{why}"
        );
        assert!(BootFailed(why).out_of_memory());
        // Or seen now.
        let e = memory_evidence(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[killed],
            Events::default(),
            "",
        );
        assert_eq!(e.killed, BTreeSet::from(["app".to_string()]));
    }

    #[test]
    fn out_of_memory_is_said_only_with_the_flag_or_an_oom_event() {
        let remedy = || {
            "Give Docker more memory in OrbStack's settings (Settings → System → Memory limit)"
                .to_string()
        };
        // Exit 137 alone (our own stops give it too): hedged.
        let e = memory_evidence(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[],
            parse_events(
                &EVENTS.replace("\"Action\":\"oom\"", "\"Action\":\"x\""),
                None,
            ),
            "",
        );
        let why = e.reason(remedy).unwrap();
        assert!(why.starts_with("Docker stopped part of your site by force (the app and the web server), likely for lack of memory."), "{why}");
        assert!(BootFailed(why).out_of_memory());
        // The oom event: certain, and the 137 of the same service isn't repeated.
        let e = memory_evidence(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[],
            parse_events(EVENTS, None),
            "",
        );
        assert_eq!(e.oom, BTreeSet::from(["app".to_string()]));
        assert_eq!(e.killed, BTreeSet::from(["edge".to_string()]));
        assert_eq!(
            e.reason(remedy).unwrap(),
            "Your site ran out of memory, so Docker stopped part of it (the app). Give Docker \
             more memory in OrbStack's settings (Settings → System → Memory limit), or stop \
             other sites, then start your site again."
        );
        // The database: the flag on a running db plus Postgres's own line.
        let e = memory_evidence(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[svc("db", true, false, 0)],
            Events::default(),
            PG_KILLED,
        );
        let why = e.reason(remedy).unwrap();
        assert!(
            why.contains("stopped part of it (the database). Your site's data is safe."),
            "{why}"
        );
        assert!(BootFailed(why).out_of_memory());
        // Postgres's line alone: killed, cause unproven.
        let e = memory_evidence(
            &BTreeSet::new(),
            &BTreeSet::new(),
            &[],
            Events::default(),
            PG_KILLED,
        );
        assert!(e
            .reason(remedy)
            .unwrap()
            .contains("(the database), likely for lack of memory"));
        // What the looks saw counts after the restart reset the flag.
        let e = memory_evidence(
            &BTreeSet::from(["app".to_string()]),
            &BTreeSet::new(),
            &[svc("app", false, false, 3)],
            Events::default(),
            "",
        );
        assert!(e.reason(remedy).unwrap().contains("(the app)"));
        // Nothing: no memory reason, the wait's own reason stands.
        assert_eq!(
            memory_evidence(
                &BTreeSet::new(),
                &BTreeSet::new(),
                &[svc("app", false, false, 3)],
                Events::default(),
                "x"
            )
            .reason(remedy),
            None
        );
        assert!(!BootFailed(crash_loop(&BTreeSet::from(["app".to_string()]))).out_of_memory());
    }
}
