//! Which program runs Docker on this computer, and how to restart it.
//!
//! Several failures end with "restart Docker" (a half-attached network, a
//! start that stays stuck after clearing Docker's leftovers, a daemon that
//! stopped answering). Owners don't always know how, and many don't run Docker
//! Desktop at all — OrbStack, Colima and Rancher Desktop all serve the same
//! `docker` CLI. So the Manager finds out which one answers and restarts that
//! one ([`restart`]), or says in plain words how to do it by hand
//! ([`RestartPlan::Manual`]) when it can't (Docker Engine on Linux needs
//! `sudo`).
//!
//! Detection ([`detect_from`]) and the plan ([`restart_plan`]) are pure, over
//! what Docker and the OS said; [`detect`] and [`restart`] only gather and run.

use std::fmt;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::docker::{self, output_within};
use crate::ops::{Reporter, Stage};

/// The error for a failure whose way out is restarting Docker. Its text is
/// already phrased for the owner; the app finds it by type and offers its
/// Restart button next to it.
#[derive(Debug)]
pub struct NeedsDockerRestart(pub String);

impl fmt::Display for NeedsDockerRestart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsDockerRestart {}

/// Whether restarting Docker is the owner's way out of `e`: a
/// [`NeedsDockerRestart`] anywhere in its chain, or a failed boot that says so
/// ([`crate::BootFailed::needs_docker_restart`]).
pub fn needs_docker_restart(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<NeedsDockerRestart>().is_some()
            || c.downcast_ref::<crate::BootFailed>()
                .is_some_and(crate::BootFailed::needs_docker_restart)
    })
}

/// The program that runs Docker here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    DockerDesktop,
    OrbStack,
    Colima,
    RancherDesktop,
    /// Docker Engine run by the system (systemd) on Linux.
    LinuxEngine,
    /// Something we can't name (a remote `DOCKER_HOST`, Podman, a Docker that
    /// didn't answer).
    Unknown,
}

impl Runtime {
    /// Its name, as the owner knows it.
    pub fn name(self) -> &'static str {
        match self {
            Runtime::DockerDesktop => "Docker Desktop",
            Runtime::OrbStack => "OrbStack",
            Runtime::Colima => "Colima",
            Runtime::RancherDesktop => "Rancher Desktop",
            Runtime::LinuxEngine | Runtime::Unknown => "Docker",
        }
    }
}

/// How the owner gives the containers more memory on `runtime`, as the start
/// of a sentence ("…, or stop other sites"). `profile` is a non-default Colima
/// profile ([`colima_profile`]).
///
/// Settings paths, checked against each vendor's docs on 2026-11-14:
/// - Docker Desktop: "Settings > VM resources > Advanced > Memory limit"
///   (<https://docs.docker.com/desktop/settings-and-maintenance/settings/>);
///   older Desktop releases call the tab "Resources" (not re-checked).
/// - OrbStack: "Settings > System > Memory limit", key `memory_mib`
///   (<https://docs.orbstack.dev/settings>).
/// - Colima: `colima start --memory <GiB>`, default VM 2 GiB
///   (<https://github.com/abiosoft/colima#customizing-the-vm>); a running VM
///   must be stopped first.
/// - Rancher Desktop: "Preferences > Virtual Machine > Hardware" → Memory
///   (<https://docs.rancherdesktop.io/ui/preferences/virtual-machine/hardware>;
///   the path is read off the page's breadcrumbs, not a written step).
pub fn memory_remedy(runtime: Runtime, profile: Option<&str>) -> String {
    match runtime {
        Runtime::DockerDesktop => "Give Docker more memory in Docker Desktop's settings \
             (Settings → VM resources → Advanced → Memory limit)"
            .into(),
        Runtime::OrbStack => {
            "Give Docker more memory in OrbStack's settings (Settings → System → Memory limit)"
                .into()
        }
        Runtime::Colima => {
            let p = profile
                .map(|p| format!(" --profile {p}"))
                .unwrap_or_default();
            format!("Give Docker more memory (`colima stop{p}`, then `colima start{p} --memory 4`)")
        }
        Runtime::RancherDesktop => "Give Docker more memory in Rancher Desktop's preferences \
             (Preferences → Virtual Machine → Hardware → Memory)"
            .into(),
        // No VM in between: the computer itself is out of memory.
        Runtime::LinuxEngine => "Close other programs to free memory on this computer".into(),
        Runtime::Unknown => "Give Docker more memory in its settings".into(),
    }
}

/// [`memory_remedy`] for the runtime here: one `docker info` and one `docker
/// context inspect` ([`gather`]), nothing else. For a reason already known to
/// be about memory.
pub fn memory_remedy_here() -> String {
    let signals = gather();
    let runtime = detect_from(&signals, HostOs::current());
    let profile = (runtime == Runtime::Colima)
        .then(|| colima_profile(&signals))
        .flatten();
    memory_remedy(runtime, profile.as_deref())
}

/// The operating system this Manager runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Mac,
    Linux,
    Windows,
    Other,
}

impl HostOs {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => HostOs::Mac,
            "linux" => HostOs::Linux,
            "windows" => HostOs::Windows,
            _ => HostOs::Other,
        }
    }
}

/// What Docker said about itself; every field is `None` when it didn't say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// `docker info`'s `OperatingSystem` ("OrbStack", "Docker Desktop",
    /// "Ubuntu 24.04 LTS", …).
    pub operating_system: Option<String>,
    /// `docker info`'s `Name`: the daemon's host name ("orbstack",
    /// "docker-desktop", "colima", "lima-rancher-desktop", …).
    pub name: Option<String>,
    /// The active context's name ("orbstack", "desktop-linux", "colima", …).
    pub context: Option<String>,
    /// The active context's endpoint (`unix:///Users/me/.orbstack/run/docker.sock`).
    /// When it is a symlinked socket (`/var/run/docker.sock` on a Mac), the
    /// file it points at.
    pub endpoint: Option<String>,
}

/// Which runtime `signals` describe on `os`. `docker info` decides when it
/// answered; the context's endpoint otherwise (Docker may be the very thing
/// that is stuck).
pub fn detect_from(signals: &Signals, os: HostOs) -> Runtime {
    let info_os = signals.operating_system.as_deref().unwrap_or("").trim();
    let name = signals.name.as_deref().unwrap_or("").trim();
    if info_os == "OrbStack" {
        return Runtime::OrbStack;
    }
    if info_os == "Docker Desktop" {
        return Runtime::DockerDesktop;
    }
    if name == "colima" || name.starts_with("colima-") {
        return Runtime::Colima;
    }
    if name.contains("rancher-desktop") || info_os.contains("Rancher Desktop") {
        return Runtime::RancherDesktop;
    }

    let endpoint = signals.endpoint.as_deref().unwrap_or("");
    let context = signals.context.as_deref().unwrap_or("");
    if endpoint.contains("/.orbstack/") || context == "orbstack" {
        return Runtime::OrbStack;
    }
    if endpoint.contains("/.colima/") || context == "colima" || context.starts_with("colima-") {
        return Runtime::Colima;
    }
    if endpoint.contains("/.rd/") || context == "rancher-desktop" {
        return Runtime::RancherDesktop;
    }
    if endpoint.contains("/.docker/run/docker.sock")
        || endpoint.contains("/.docker/desktop/docker.sock")
        || endpoint.contains("dockerDesktop")
        || context == "desktop-linux"
        || (os == HostOs::Windows && endpoint.contains("docker_engine"))
    {
        return Runtime::DockerDesktop;
    }
    let system_socket = matches!(
        endpoint,
        "unix:///var/run/docker.sock" | "unix:///run/docker.sock"
    );
    if os == HostOs::Linux && (system_socket || (endpoint.is_empty() && !info_os.is_empty())) {
        return Runtime::LinuxEngine;
    }
    Runtime::Unknown
}

/// The Colima profile `signals` name when it isn't the default one: Colima
/// names its daemon and its context `colima-<profile>` (plain `colima` for
/// `default`) and keeps the socket in `~/.colima/<profile>/`. A bare `colima
/// restart` restarts the default profile, whatever Docker is talking to.
pub fn colima_profile(signals: &Signals) -> Option<String> {
    let named = |s: &str| -> Option<String> {
        let s = s.trim();
        if s == "colima" {
            return Some("default".into());
        }
        s.strip_prefix("colima-")
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    };
    let in_endpoint = |e: &str| -> Option<String> {
        let (_, rest) = e.split_once("/.colima/")?;
        let (dir, _) = rest.split_once('/')?;
        (!dir.is_empty() && !dir.starts_with('_')).then(|| dir.to_string())
    };
    signals
        .name
        .as_deref()
        .and_then(named)
        .or_else(|| signals.context.as_deref().and_then(named))
        .or_else(|| signals.endpoint.as_deref().and_then(in_endpoint))
        .filter(|p| p != "default")
}

/// `colima restart`, with `--profile` for any profile but the default one.
fn colima_restart(profile: Option<&str>) -> Vec<String> {
    // scenario: none — restarts the owner's Docker runtime; a scenario never restarts the Docker every other project shares (owner checks it by hand)
    let mut args = vec!["colima".to_string(), "restart".to_string()];
    if let Some(p) = profile {
        args.extend(["--profile".to_string(), p.to_string()]);
    }
    args
}

/// One step of a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run this program with these arguments; it must succeed.
    Run(Vec<String>),
    /// Wait until the macOS app of this name has quit.
    WaitQuit(String),
}

impl Step {
    fn run(args: &[&str]) -> Self {
        Step::Run(args.iter().map(|a| (*a).to_string()).collect())
    }
}

/// How Docker gets restarted here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartPlan {
    /// The Manager runs these, in order.
    Steps(Vec<Step>),
    /// The owner does it: `steps` in plain words, and the command to copy,
    /// when there is one.
    Manual { steps: String, copy: Option<String> },
}

/// The restart for `runtime` on `os`. `desktop_cli` is whether `docker
/// desktop` answers (Docker Desktop 4.37 and later); `profile` is the Colima
/// profile when it isn't the default one ([`colima_profile`]).
///
/// OrbStack restarts only its built-in `docker` machine: `orbctl stop` would
/// stop all of OrbStack, the owner's Linux machines too.
pub fn restart_plan(
    runtime: Runtime,
    os: HostOs,
    desktop_cli: bool,
    profile: Option<&str>,
) -> RestartPlan {
    match runtime {
        // scenario: none — restarts the owner's Docker runtime; a scenario never restarts the Docker every other project shares (owner checks it by hand)
        Runtime::OrbStack => RestartPlan::Steps(vec![Step::run(&["orbctl", "restart", "docker"])]),
        Runtime::DockerDesktop if desktop_cli => {
            // scenario: none — restarts the owner's Docker runtime; a scenario never restarts the Docker every other project shares (owner checks it by hand)
            RestartPlan::Steps(vec![Step::run(&["docker", "desktop", "restart"])])
        }
        Runtime::DockerDesktop if os == HostOs::Mac => RestartPlan::Steps(vec![
            Step::run(&["osascript", "-e", "quit app \"Docker\""]),
            Step::WaitQuit("Docker".into()),
            Step::run(&["open", "-a", "Docker"]),
        ]),
        Runtime::Colima => RestartPlan::Steps(vec![Step::Run(colima_restart(profile))]),
        Runtime::RancherDesktop => RestartPlan::Steps(vec![
            // scenario: none — restarts the owner's Docker runtime; a scenario never restarts the Docker every other project shares (owner checks it by hand)
            Step::run(&["rdctl", "shutdown"]),
            // scenario: none — restarts the owner's Docker runtime; a scenario never restarts the Docker every other project shares (owner checks it by hand)
            Step::run(&["rdctl", "start"]),
        ]),
        _ => {
            let (steps, copy) = manual_steps(runtime, os, profile);
            RestartPlan::Manual { steps, copy }
        }
    }
}

/// How to restart `runtime` by hand, in plain words, and the command to
/// copy when there is one. Also what a failed restart ends with. `profile`
/// as for [`restart_plan`].
pub fn manual_steps(
    runtime: Runtime,
    os: HostOs,
    profile: Option<&str>,
) -> (String, Option<String>) {
    let said = |s: &str| (s.to_string(), None);
    let command = |c: String| (format!("Run `{c}` in a terminal."), Some(c));
    match (runtime, os) {
        (Runtime::OrbStack, _) => (
            "Run `orbctl restart docker` in a terminal, or quit OrbStack from the menu bar and \
             open it again."
                .into(),
            Some("orbctl restart docker".into()),
        ),
        (Runtime::DockerDesktop, HostOs::Mac) => {
            said("Quit Docker Desktop from the menu bar (the whale icon), then open it again.")
        }
        (Runtime::DockerDesktop, HostOs::Windows) => said(
            "Right-click the Docker icon by the clock, choose Quit Docker Desktop, then open \
             Docker Desktop again.",
        ),
        (Runtime::DockerDesktop, _) => {
            said("Quit Docker Desktop from its icon in the system tray, then open it again.")
        }
        (Runtime::Colima, _) => command(colima_restart(profile).join(" ")),
        (Runtime::RancherDesktop, _) => said("Quit Rancher Desktop, then open it again."),
        (Runtime::LinuxEngine, _) => (
            "Run `sudo systemctl restart docker` in a terminal.".into(),
            Some("sudo systemctl restart docker".into()),
        ),
        (Runtime::Unknown, _) => said("Quit Docker and open it again."),
    }
}

/// How long one detection probe may take: Docker may be the stuck thing.
const DETECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one restart step may take (a restart waits for every container
/// to stop).
const STEP_TIMEOUT: Duration = Duration::from_secs(180);
/// How long Docker Desktop may take to quit.
const QUIT_WITHIN: Duration = Duration::from_secs(60);
/// How long Docker may take to answer again after the restart.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// The first line of a probe's output, when it succeeded within `deadline`.
fn answer(mut cmd: Command, deadline: Duration) -> Option<String> {
    let out = output_within(&mut cmd, deadline).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Split `a|b` into its two non-empty halves.
fn halves(line: &str) -> (Option<String>, Option<String>) {
    let (a, b) = line.split_once('|').unwrap_or((line, ""));
    let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    (some(a), some(b))
}

/// Ask Docker what it is, each probe bounded by [`DETECT_TIMEOUT`].
pub fn gather() -> Signals {
    let mut info = docker::docker();
    info.args(["info", "--format", "{{.OperatingSystem}}|{{.Name}}"]);
    let (operating_system, name) = answer(info, DETECT_TIMEOUT)
        .map(|l| halves(&l))
        .unwrap_or_default();
    let mut ctx = docker::docker();
    ctx.args([
        "context",
        "inspect",
        "--format",
        "{{.Name}}|{{.Endpoints.docker.Host}}",
    ]);
    let (context, endpoint) = answer(ctx, DETECT_TIMEOUT)
        .map(|l| halves(&l))
        .unwrap_or_default();
    Signals {
        operating_system,
        name,
        context,
        endpoint: endpoint.map(resolve_socket),
    }
}

/// A `unix://` endpoint whose socket is a symlink (`/var/run/docker.sock` on a
/// Mac points into OrbStack's or Docker Desktop's folder), as the file it
/// points at; anything else unchanged.
fn resolve_socket(endpoint: String) -> String {
    let Some(path) = endpoint.strip_prefix("unix://") else {
        return endpoint;
    };
    match std::fs::read_link(path) {
        Ok(target) => format!("unix://{}", target.display()),
        Err(_) => endpoint,
    }
}

/// Whether the `docker desktop` command answers (Docker Desktop 4.37+).
fn has_desktop_cli() -> bool {
    let mut c = docker::docker();
    c.args(["desktop", "version"]);
    answer(c, DETECT_TIMEOUT).is_some()
}

/// The runtime here and how to restart it.
#[derive(Debug, Clone)]
pub struct Detected {
    pub runtime: Runtime,
    pub os: HostOs,
    /// The Colima profile, when it isn't the default one.
    pub profile: Option<String>,
    pub plan: RestartPlan,
}

/// Find out which runtime runs Docker here, and how to restart it.
pub fn detect() -> Detected {
    let os = HostOs::current();
    let signals = gather();
    let runtime = detect_from(&signals, os);
    let desktop_cli = runtime == Runtime::DockerDesktop && has_desktop_cli();
    let profile = (runtime == Runtime::Colima)
        .then(|| colima_profile(&signals))
        .flatten();
    Detected {
        runtime,
        os,
        plan: restart_plan(runtime, os, desktop_cli, profile.as_deref()),
        profile,
    }
}

/// Detect, then [`restart`].
pub fn restart_docker(r: &mut dyn Reporter) -> Result<Runtime> {
    let found = detect();
    restart(&found, r)?;
    Ok(found.runtime)
}

/// Restart Docker by `found`'s plan, then wait until it answers again
/// (up to [`ANSWER_WITHIN`]). A manual plan, or a step that fails, is an error
/// that ends with how to do it by hand.
pub fn restart(found: &Detected, r: &mut dyn Reporter) -> Result<()> {
    let name = found.runtime.name();
    let (by_hand, _) = manual_steps(found.runtime, found.os, found.profile.as_deref());
    let steps = match &found.plan {
        RestartPlan::Steps(steps) => steps,
        RestartPlan::Manual { steps, .. } => {
            return Err(anyhow!("Atelier can't restart {name} for you. {steps}"));
        }
    };
    for step in steps {
        match step {
            Step::Run(args) => {
                r.stage(Stage::Working, &step_headline(name, args), None);
                run_step(args).map_err(|why| {
                    anyhow!("Couldn't restart {name}: {why}. {by_hand} Then try again.")
                })?;
            }
            Step::WaitQuit(app) => {
                r.stage(
                    Stage::Working,
                    &format!("Waiting for {name} to quit…"),
                    None,
                );
                if !wait_quit(app) {
                    return Err(anyhow!(
                        "{name} didn't quit within {}s. {by_hand} Then try again.",
                        QUIT_WITHIN.as_secs()
                    ));
                }
            }
        }
    }
    r.stage(
        Stage::Working,
        "Waiting for Docker to answer again (up to 2 minutes)…",
        None,
    );
    if !wait_answering(ANSWER_WITHIN) {
        return Err(anyhow!(
            "{name} was restarted, but Docker hasn't answered within {}s. Wait a minute and \
             try again; if it still doesn't answer: {by_hand}",
            ANSWER_WITHIN.as_secs()
        ));
    }
    r.stage(Stage::Working, "Docker is answering again.", None);
    Ok(())
}

/// The headline for a step: "Stopping OrbStack…", "Restarting Colima…".
fn step_headline(name: &str, args: &[String]) -> String {
    let verb =
        args.iter().skip(1).map(String::as_str).find(|a| {
            matches!(*a, "stop" | "start" | "restart" | "shutdown") || a.starts_with("quit")
        });
    match verb {
        Some("stop" | "shutdown") => format!("Stopping {name}…"),
        Some(v) if v.starts_with("quit") => format!("Quitting {name}…"),
        Some("start") => format!("Starting {name}…"),
        Some("restart") => format!("Restarting {name}…"),
        _ if args.first().map(String::as_str) == Some("open") => format!("Opening {name}…"),
        _ => format!("Restarting {name}…"),
    }
}

/// Run one restart step; the reason in plain text when it fails.
fn run_step(args: &[String]) -> std::result::Result<(), String> {
    let (program, rest) = args.split_first().ok_or("nothing to run")?;
    let mut cmd = if program == "docker" {
        docker::docker()
    } else {
        docker::tool(program)
    };
    cmd.args(rest);
    let out = output_within(&mut cmd, STEP_TIMEOUT).map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty());
    Err(match last {
        Some(line) => format!("`{}` failed: {line}", args.join(" ")),
        None => format!("`{}` exited with {}", args.join(" "), out.status),
    })
}

/// Wait until the macOS app `app` is no longer running.
fn wait_quit(app: &str) -> bool {
    let started = Instant::now();
    while started.elapsed() < QUIT_WITHIN {
        let mut c = docker::tool("osascript");
        c.args(["-e", &format!("application \"{app}\" is running")]);
        if answer(c, DETECT_TIMEOUT).as_deref() == Some("false") {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// Whether `docker info` answers, polling every 2 s for up to `within`.
pub fn wait_answering(within: Duration) -> bool {
    let started = Instant::now();
    loop {
        let mut c = docker::docker();
        c.args(["info", "--format", "{{.ServerVersion}}"]);
        if answer(c, DETECT_TIMEOUT).is_some() {
            return true;
        }
        if started.elapsed() >= within {
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// One line of `docker ps --format '{{json .}}'`, the fields we read.
#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "Names", default)]
    names: String,
    #[serde(rename = "Labels", default)]
    labels: String,
}

/// The compose project a container belongs to, from `docker ps`'s
/// comma-separated `Labels`.
fn compose_project(labels: &str) -> Option<&str> {
    labels
        .split(',')
        .find_map(|l| l.strip_prefix("com.docker.compose.project="))
}

/// The running containers in `ps` (`docker ps --format '{{json .}}'`, one
/// object per line) that aren't part of compose project `own`.
pub fn parse_others(ps: &str, own: Option<&str>) -> Vec<String> {
    ps.lines()
        .filter_map(|l| serde_json::from_str::<PsLine>(l.trim()).ok())
        .filter(|c| own.is_none() || compose_project(&c.labels) != own)
        .map(|c| c.names)
        .filter(|n| !n.is_empty())
        .collect()
}

/// The containers running in Docker now that aren't the site's own (compose
/// project `own`): what a restart stops besides it. `None` when Docker didn't
/// answer, which is not the same as nothing else running.
pub fn other_running_containers(own: Option<&str>) -> Option<Vec<String>> {
    let mut c = docker::docker();
    c.args(["ps", "--format", "{{json .}}"]);
    match output_within(&mut c, DETECT_TIMEOUT) {
        Ok(out) if out.status.success() => {
            Some(parse_others(&String::from_utf8_lossy(&out.stdout), own))
        }
        _ => None,
    }
}

/// What a confirmation says a restart stops: "This stops everything …".
/// `others` is `None` when Docker didn't say.
pub fn stops_what(others: Option<&[String]>) -> String {
    const SHOWN: usize = 8;
    let lead = "This stops everything running in Docker on this computer, not only Atelier:";
    let Some(others) = others else {
        return format!(
            "{lead} Docker isn't answering, so Atelier can't tell what else is running. \
             Anything that is starts again if it's set to."
        );
    };
    if others.is_empty() {
        return format!("{lead} nothing else is running.");
    }
    let mut list = others
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if others.len() > SHOWN {
        list.push_str(&format!(" and {} more", others.len() - SHOWN));
    }
    format!("{lead} {list}. They start again if they're set to.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(
        os: Option<&str>,
        name: Option<&str>,
        ctx: Option<&str>,
        ep: Option<&str>,
    ) -> Signals {
        Signals {
            operating_system: os.map(Into::into),
            name: name.map(Into::into),
            context: ctx.map(Into::into),
            endpoint: ep.map(Into::into),
        }
    }

    // `docker info --format '{{json .}}'` OperatingSystem / Name, and `docker
    // context inspect` endpoints, as each runtime reports them.

    #[test]
    fn docker_info_names_the_runtime_when_it_answers() {
        let cases = [
            // OrbStack 1.x on macOS (the owner's Mac, 2026-11-14).
            (
                signals(Some("OrbStack"), Some("orbstack"), None, None),
                Runtime::OrbStack,
            ),
            // Docker Desktop 4.x, any OS.
            (
                signals(Some("Docker Desktop"), Some("docker-desktop"), None, None),
                Runtime::DockerDesktop,
            ),
            // Colima's VM is Ubuntu; its daemon is named after the profile.
            (
                signals(Some("Ubuntu 24.04 LTS"), Some("colima"), None, None),
                Runtime::Colima,
            ),
            (
                signals(Some("Ubuntu 24.04.1 LTS"), Some("colima-work"), None, None),
                Runtime::Colima,
            ),
            // Rancher Desktop on macOS (lima VM) and Windows (WSL distribution).
            (
                signals(
                    Some("Alpine Linux v3.20"),
                    Some("lima-rancher-desktop"),
                    None,
                    None,
                ),
                Runtime::RancherDesktop,
            ),
            (
                signals(
                    Some("Rancher Desktop WSL Distribution"),
                    Some("DESKTOP-1234"),
                    None,
                    None,
                ),
                Runtime::RancherDesktop,
            ),
        ];
        for (s, want) in cases {
            assert_eq!(detect_from(&s, HostOs::Mac), want, "{s:?}");
        }
    }

    #[test]
    fn the_context_endpoint_names_it_when_docker_does_not_answer() {
        let cases = [
            (
                "orbstack",
                "unix:///Users/me/.orbstack/run/docker.sock",
                Runtime::OrbStack,
            ),
            (
                "colima",
                "unix:///Users/me/.colima/default/docker.sock",
                Runtime::Colima,
            ),
            (
                "rancher-desktop",
                "unix:///Users/me/.rd/docker.sock",
                Runtime::RancherDesktop,
            ),
            (
                "desktop-linux",
                "unix:///Users/me/.docker/run/docker.sock",
                Runtime::DockerDesktop,
            ),
        ];
        for (ctx, ep, want) in cases {
            assert_eq!(
                detect_from(&signals(None, None, Some(ctx), Some(ep)), HostOs::Mac),
                want
            );
            // The endpoint alone is enough (a `default` context on DOCKER_HOST).
            assert_eq!(
                detect_from(&signals(None, None, Some("default"), Some(ep)), HostOs::Mac),
                want
            );
        }
        // Docker Desktop for Linux and for Windows.
        assert_eq!(
            detect_from(
                &signals(
                    None,
                    None,
                    Some("desktop-linux"),
                    Some("unix:///home/me/.docker/desktop/docker.sock")
                ),
                HostOs::Linux
            ),
            Runtime::DockerDesktop
        );
        assert_eq!(
            detect_from(
                &signals(
                    None,
                    None,
                    Some("desktop-linux"),
                    Some("npipe:////./pipe/dockerDesktopLinuxEngine")
                ),
                HostOs::Windows
            ),
            Runtime::DockerDesktop
        );
        // /var/run/docker.sock on a Mac, resolved to where it points.
        assert_eq!(
            detect_from(
                &signals(
                    None,
                    None,
                    Some("default"),
                    Some("unix:///Users/me/.orbstack/run/docker.sock")
                ),
                HostOs::Mac
            ),
            Runtime::OrbStack
        );
    }

    #[test]
    fn linux_with_the_system_socket_is_the_engine() {
        let engine = signals(
            Some("Ubuntu 24.04.1 LTS"),
            Some("vps-4021"),
            Some("default"),
            Some("unix:///var/run/docker.sock"),
        );
        assert_eq!(detect_from(&engine, HostOs::Linux), Runtime::LinuxEngine);
        // Not answering, but the system socket: still the engine.
        let quiet = signals(
            None,
            None,
            Some("default"),
            Some("unix:///var/run/docker.sock"),
        );
        assert_eq!(detect_from(&quiet, HostOs::Linux), Runtime::LinuxEngine);
        // The same socket on a Mac, unresolved, names nothing.
        assert_eq!(detect_from(&quiet, HostOs::Mac), Runtime::Unknown);
        // A remote daemon names nothing.
        let remote = signals(
            Some("Ubuntu 22.04 LTS"),
            Some("build-host"),
            Some("remote"),
            Some("tcp://10.0.0.5:2376"),
        );
        assert_eq!(detect_from(&remote, HostOs::Linux), Runtime::Unknown);
        assert_eq!(
            detect_from(&Signals::default(), HostOs::Mac),
            Runtime::Unknown
        );
    }

    #[test]
    fn each_runtime_gets_its_own_restart() {
        let run = |a: &[&str]| Step::run(a);
        assert_eq!(
            // Only OrbStack's Docker engine (its built-in `docker` machine):
            // `orbctl stop` would stop the owner's Linux machines too.
            restart_plan(Runtime::OrbStack, HostOs::Mac, false, None),
            RestartPlan::Steps(vec![run(&["orbctl", "restart", "docker"])])
        );
        assert_eq!(
            restart_plan(Runtime::DockerDesktop, HostOs::Windows, true, None),
            RestartPlan::Steps(vec![run(&["docker", "desktop", "restart"])])
        );
        assert_eq!(
            restart_plan(Runtime::DockerDesktop, HostOs::Mac, false, None),
            RestartPlan::Steps(vec![
                run(&["osascript", "-e", "quit app \"Docker\""]),
                Step::WaitQuit("Docker".into()),
                run(&["open", "-a", "Docker"]),
            ])
        );
        assert_eq!(
            restart_plan(Runtime::Colima, HostOs::Mac, false, None),
            RestartPlan::Steps(vec![run(&["colima", "restart"])])
        );
        assert_eq!(
            restart_plan(Runtime::RancherDesktop, HostOs::Mac, false, None),
            RestartPlan::Steps(vec![run(&["rdctl", "shutdown"]), run(&["rdctl", "start"])])
        );
    }

    #[test]
    fn a_colima_restart_names_the_profile_docker_talks_to() {
        let profile = |name, ctx, ep| colima_profile(&signals(None, name, ctx, ep));
        // The default profile: daemon `colima`, context `colima`,
        // `~/.colima/default/docker.sock`. A bare `colima restart` is right.
        assert_eq!(
            profile(
                Some("colima"),
                Some("colima"),
                Some("unix:///Users/u/.colima/default/docker.sock")
            ),
            None
        );
        // Profile `work`, from each signal alone (Docker may not answer).
        assert_eq!(
            profile(Some("colima-work"), None, None).as_deref(),
            Some("work")
        );
        assert_eq!(
            profile(None, Some("colima-work"), None).as_deref(),
            Some("work")
        );
        assert_eq!(
            profile(
                None,
                Some("default"),
                Some("unix:///Users/u/.colima/work/docker.sock")
            )
            .as_deref(),
            Some("work")
        );
        // Not Colima at all.
        assert_eq!(profile(Some("orbstack"), Some("orbstack"), None), None);

        let work = Some("work");
        assert_eq!(
            restart_plan(Runtime::Colima, HostOs::Mac, false, work),
            RestartPlan::Steps(vec![Step::run(&["colima", "restart", "--profile", "work"])])
        );
        assert_eq!(
            restart_plan(Runtime::Colima, HostOs::Mac, false, None),
            RestartPlan::Steps(vec![Step::run(&["colima", "restart"])])
        );
        assert_eq!(
            manual_steps(Runtime::Colima, HostOs::Mac, work),
            (
                "Run `colima restart --profile work` in a terminal.".to_string(),
                Some("colima restart --profile work".to_string())
            )
        );
        assert_eq!(
            manual_steps(Runtime::Colima, HostOs::Linux, None)
                .1
                .as_deref(),
            Some("colima restart")
        );
    }

    #[test]
    fn what_the_manager_cant_restart_is_said_in_plain_words() {
        assert_eq!(
            restart_plan(Runtime::LinuxEngine, HostOs::Linux, false, None),
            RestartPlan::Manual {
                steps: "Run `sudo systemctl restart docker` in a terminal.".into(),
                copy: Some("sudo systemctl restart docker".into()),
            }
        );
        for os in [HostOs::Windows, HostOs::Linux] {
            match restart_plan(Runtime::DockerDesktop, os, false, None) {
                RestartPlan::Manual { steps, copy } => {
                    assert!(steps.starts_with("Right-click") || steps.starts_with("Quit"));
                    assert!(steps.contains("Docker Desktop"), "{steps}");
                    assert_eq!(copy, None);
                }
                other => panic!("{os:?}: {other:?}"),
            }
        }
        assert_eq!(
            restart_plan(Runtime::Unknown, HostOs::Mac, false, None),
            RestartPlan::Manual {
                steps: "Quit Docker and open it again.".into(),
                copy: None,
            }
        );
        assert_eq!(
            manual_steps(Runtime::OrbStack, HostOs::Mac, None),
            (
                "Run `orbctl restart docker` in a terminal, or quit OrbStack from the menu bar \
                 and open it again."
                    .to_string(),
                Some("orbctl restart docker".to_string())
            )
        );
    }

    #[test]
    fn a_manual_plan_restarts_nothing_and_says_how() {
        let found = Detected {
            runtime: Runtime::LinuxEngine,
            os: HostOs::Linux,
            profile: None,
            plan: restart_plan(Runtime::LinuxEngine, HostOs::Linux, false, None),
        };
        let e = restart(&found, &mut crate::ops::Silent).unwrap_err();
        assert_eq!(
            e.to_string(),
            "Atelier can't restart Docker for you. Run `sudo systemctl restart docker` in a \
             terminal."
        );
    }

    #[test]
    fn step_headlines_say_what_happens() {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            step_headline("OrbStack", &s(&["orbctl", "stop"])),
            "Stopping OrbStack…"
        );
        assert_eq!(
            step_headline("OrbStack", &s(&["orbctl", "start"])),
            "Starting OrbStack…"
        );
        assert_eq!(
            step_headline(
                "Docker Desktop",
                &s(&["osascript", "-e", "quit app \"Docker\""])
            ),
            "Quitting Docker Desktop…"
        );
        assert_eq!(
            step_headline("Docker Desktop", &s(&["open", "-a", "Docker"])),
            "Opening Docker Desktop…"
        );
        assert_eq!(
            step_headline("Colima", &s(&["colima", "restart"])),
            "Restarting Colima…"
        );
        assert_eq!(
            step_headline("OrbStack", &s(&["orbctl", "restart", "docker"])),
            "Restarting OrbStack…"
        );
    }

    // Real `docker ps --format '{{json .}}'` lines (OrbStack, Docker 29.4,
    // 2026-11-14), trimmed to the fields that matter plus a few around them.
    const PS: &str = r#"{"Command":"\"/docker-entrypoint.…\"","CreatedAt":"2026-11-14 17:54:33 +0100 CET","ID":"92ee71018e68","Image":"ddev/ddev-dbserver-mariadb-10.11:v1.25.1-flowdrop-28x-built","Labels":"com.ddev.app-type=drupal11,com.ddev.platform=ddev,com.ddev.site-name=flowdrop-28x,com.docker.compose.config-hash=d5676d8e9788c8c939c26fcf4b1454af1ca0e2063af9e00f80f5e953f4c40644,com.docker.compose.container-number=1,com.docker.compose.oneoff=False,com.docker.compose.project.config_files=/Users/me/work/fddo/.ddev/.ddev-docker-compose-full.yaml,com.docker.compose.project.working_dir=/Users/me/work/fddo/.ddev,com.docker.compose.project=ddev-flowdrop-28x,com.docker.compose.service=db,com.docker.compose.version=5.0.2","LocalVolumes":"2","Names":"ddev-flowdrop-28x-db","Networks":"ddev_default,ddev-flowdrop-28x_default","Platform":null,"Ports":"127.0.0.1:32979->3306/tcp","RunningFor":"5 minutes ago","Size":"0B","State":"running","Status":"Up 5 minutes (healthy)"}
{"Command":"\"/entrypoint.sh\"","CreatedAt":"2026-11-14 09:12:01 +0100 CET","ID":"1b2c3d4e5f60","Image":"ghcr.io/aincient-labs/atelier-cms:0.22.0","Labels":"com.docker.compose.config-hash=0f1e2d3c,com.docker.compose.container-number=1,com.docker.compose.oneoff=False,com.docker.compose.project.config_files=/Users/me/.atelier/sites/review-tmp/compose.yaml,com.docker.compose.project.working_dir=/Users/me/.atelier/sites/review-tmp,com.docker.compose.project=atelier-review-tmp,com.docker.compose.service=app,com.docker.compose.version=5.0.2","LocalVolumes":"1","Names":"atelier-review-tmp-app-1","Networks":"atelier-review-tmp_default","Platform":null,"Ports":"","RunningFor":"8 hours ago","Size":"0B","State":"running","Status":"Up 8 hours"}
{"Command":"\"nginx -g 'daemon of…\"","CreatedAt":"2026-11-14 09:12:00 +0100 CET","ID":"2c3d4e5f6071","Image":"nginx:1.27-alpine","Labels":"maintainer=NGINX Docker Maintainers <docker-maint@nginx.com>","LocalVolumes":"0","Names":"loose-nginx","Networks":"bridge","Platform":null,"Ports":"0.0.0.0:8081->80/tcp","RunningFor":"8 hours ago","Size":"0B","State":"running","Status":"Up 8 hours"}
"#;

    #[test]
    fn others_are_every_running_container_but_the_sites_own() {
        assert_eq!(
            parse_others(PS, Some("atelier-review-tmp")),
            ["ddev-flowdrop-28x-db", "loose-nginx"]
        );
        // `com.docker.compose.project.working_dir=` is not the project label.
        assert_eq!(
            compose_project(
                "com.docker.compose.project.working_dir=/x,com.docker.compose.project=p"
            ),
            Some("p")
        );
        assert_eq!(parse_others(PS, None).len(), 3);
        assert!(parse_others("", Some("atelier")).is_empty());
        assert!(parse_others("not json\n", Some("atelier")).is_empty());
    }

    #[test]
    fn the_confirmation_names_what_else_stops() {
        assert_eq!(
            stops_what(Some(&[])),
            "This stops everything running in Docker on this computer, not only Atelier: \
             nothing else is running."
        );
        assert_eq!(
            stops_what(Some(&["ddev-flowdrop-28x-db".into(), "loose-nginx".into()])),
            "This stops everything running in Docker on this computer, not only Atelier: \
             ddev-flowdrop-28x-db, loose-nginx. They start again if they're set to."
        );
        let many: Vec<String> = (1..=11).map(|i| format!("c{i}")).collect();
        assert!(stops_what(Some(&many)).contains("c8 and 3 more."));
        assert!(stops_what(None).contains("can't tell what else is running"));
    }

    #[test]
    fn a_restart_is_owed_by_type_not_by_wording() {
        let e: anyhow::Error = NeedsDockerRestart("stuck".into()).into();
        assert!(needs_docker_restart(&e));
        assert!(needs_docker_restart(&e.context("couldn't start the site")));
        let boot: anyhow::Error = crate::BootFailed(crate::boot::NETWORK_STUCK.into()).into();
        assert!(needs_docker_restart(&boot));
        let db: anyhow::Error =
            crate::BootFailed("Your site can't reach its database.".into()).into();
        assert!(!needs_docker_restart(&db));
        assert!(!needs_docker_restart(&anyhow!("Docker Desktop said no")));
    }
}
