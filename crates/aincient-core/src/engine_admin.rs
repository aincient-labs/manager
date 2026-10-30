//! Driving a site's engine without HTTP (m3-7, E-01): `docker exec <engine>
//! atelier engine admin …`, run on this computer for a local site and over
//! the SSH session for a server. No token: whoever can `docker exec` is
//! already root-equivalent on that host. EXPERIMENTAL, DECISIONS 0483.
//!
//! - [`EngineInfo`] says whether a stack uses the engine: `.env` names
//!   `compose.engine.yaml` and compose knows an `engine` container. It rides
//!   in [`crate::ops::Status`], so a server's `app status --json` already
//!   tells the Manager (one round trip it makes anyway).
//! - [`route`] decides, per lifecycle action, engine or host CLI. The host CLI
//!   from M2 stays the fallback for everything it already did.
//! - [`EngineAdmin`] runs the admin verbs, reading `admin status` first and
//!   refusing a different API major ([`crate::engine_client::check_api`], m3-3).
//! - [`remove`] takes the engine off a stack (m3-6, C-09): wake, disable,
//!   `up -d --remove-orphans`, verify edge + app + db.

use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docker::{compose, docker, output_within, RunError};
use crate::engine;
use crate::engine_client::{check_api, EngineError};
use crate::Stack;

/// How long an admin verb may take (lockdown and wake run compose).
pub const ADMIN_DEADLINE: Duration = Duration::from_secs(150);
/// The services of the default stack, which [`remove`] restores (C-09).
pub const DEFAULT_SERVICES: [&str; 3] = ["app", "db", "edge"];

/// Whether a stack uses the engine, and its container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EngineInfo {
    /// `.env` names the opt-in compose file.
    pub enabled: bool,
    /// The engine container's name, as compose reports it.
    #[serde(default)]
    pub container: Option<String>,
    /// That container is running.
    #[serde(default)]
    pub running: bool,
}

impl EngineInfo {
    /// The container to `docker exec` into, when there is a usable one.
    pub fn ready(&self) -> Option<&str> {
        self.container
            .as_deref()
            .filter(|c| self.enabled && self.running && valid_container(c))
    }
}

/// A compose container name, as `docker exec` takes it. Checked before it
/// goes into a command (it comes from a server's answer).
pub fn valid_container(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// `(service, state, name)` from `compose ps --format json` (one object per
/// line, or one array).
fn parse_ps(out: &str) -> Vec<(String, String, String)> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .flat_map(|v| match v {
            Value::Array(a) => a,
            one => vec![one],
        })
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            (s("Service"), s("State").to_lowercase(), s("Name"))
        })
        .collect()
}

/// The local stack's engine, or `None` when it isn't enabled. One `compose
/// ps` (only when `.env` names the engine); a failed probe reads as not running.
pub fn probe(stack: &Stack) -> Option<EngineInfo> {
    if !engine::is_enabled(stack) {
        return None;
    }
    let mut c = compose(stack);
    c.args(["ps", "-a", "--format", "json", "engine"]);
    let found = output_within(&mut c, Duration::from_secs(20))
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
        .into_iter()
        .find(|(service, _, _)| service == "engine");
    Some(EngineInfo {
        enabled: true,
        running: found.as_ref().is_some_and(|(_, st, _)| st == "running"),
        container: found.map(|(_, _, name)| name).filter(|n| !n.is_empty()),
    })
}

/// A lifecycle action the engine owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Status,
    Freeze,
    Live,
    Wake,
    Lockdown,
    Unlock,
}

/// Where a lifecycle action runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// `docker exec <container> atelier engine admin …`.
    Engine(String),
    /// The host CLI, as in M2 (`atelier --site <slug> …` on the server, or the
    /// local code path).
    HostCli,
    /// Not possible without the engine.
    Refuse(String),
}

/// Why lockdown and unlock need the engine.
pub fn needs_engine(what: &str) -> String {
    format!(
        "{what} needs the engine, and this site has none running. Enable the engine \
         (experimental) or run it on the server: `docker exec <project>-engine-1 atelier \
         engine admin {what}`"
    )
}

/// Engine or host CLI for `action` (m3-7). With the engine present:
/// `status` reads the engine's state too, `wake`, `lockdown` and `unlock` go
/// through it. `freeze` and `live` stay on the host CLI in every case: its
/// freeze takes the owner's options (label, keep, base URL), the engine's
/// admin has none. Without the engine, lockdown and unlock are refused (only
/// the engine has them); the rest is the host CLI, as before.
pub fn route(action: Lifecycle, engine: Option<&EngineInfo>) -> Via {
    let ready = engine.and_then(EngineInfo::ready).map(str::to_string);
    match (action, ready) {
        (Lifecycle::Freeze | Lifecycle::Live, _) => Via::HostCli,
        (Lifecycle::Status | Lifecycle::Wake, None) => Via::HostCli,
        (Lifecycle::Lockdown, None) => Via::Refuse(needs_engine("lockdown")),
        (Lifecycle::Unlock, None) => Via::Refuse(needs_engine("unlock")),
        (_, Some(container)) => Via::Engine(container),
    }
}

/// The admin verbs the Manager uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminVerb {
    Status,
    Lockdown,
    Unlock,
    Wake,
}

impl AdminVerb {
    pub fn word(self) -> &'static str {
        match self {
            AdminVerb::Status => "status",
            AdminVerb::Lockdown => "lockdown",
            AdminVerb::Unlock => "unlock",
            AdminVerb::Wake => "wake",
        }
    }
}

/// The `docker` arguments for one admin verb. No `-i`/`-t`: stdin stays closed.
pub fn admin_argv(container: &str, verb: AdminVerb) -> Vec<String> {
    ["exec", container, "atelier", "engine", "admin", verb.word()]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Where `docker` runs: this computer, or a server over SSH
/// ([`crate::remote::Session`] implements it).
pub trait DockerExec {
    /// "this computer" or the ssh target, for messages.
    fn place(&self) -> String;
    /// `docker <args…>`; a non-zero exit is an error carrying its message.
    fn docker(&self, args: &[String], deadline: Duration) -> Result<Output>;
}

/// `docker` on this computer.
pub struct LocalDocker;

impl DockerExec for LocalDocker {
    fn place(&self) -> String {
        "this computer".into()
    }

    fn docker(&self, args: &[String], deadline: Duration) -> Result<Output> {
        let mut c = docker();
        c.args(args);
        let what = format!("docker {}", args.join(" "));
        let out = match output_within(&mut c, deadline) {
            Ok(o) => o,
            Err(RunError::TimedOut { after, .. }) => {
                bail!("`{what}` didn't finish within {}s", after.as_secs())
            }
            Err(RunError::Io { source, .. }) => bail!("couldn't run docker: {source}"),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("no message");
            bail!("`{what}` failed: {detail}");
        }
        Ok(out)
    }
}

/// `atelier engine admin status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStatus {
    pub api: String,
    #[serde(default)]
    pub engine: String,
    pub app: String,
    pub db: String,
    pub lockdown: bool,
    #[serde(default)]
    pub wake_paused: bool,
    #[serde(default)]
    pub wake_code_set: bool,
}

impl AdminStatus {
    /// `app` and `db` both running.
    pub fn awake(&self) -> bool {
        self.app == "running" && self.db == "running"
    }
}

/// One site's engine, reached with `docker exec`.
pub struct EngineAdmin<'a, D: DockerExec + ?Sized> {
    docker: &'a D,
    container: String,
}

impl<'a, D: DockerExec + ?Sized> EngineAdmin<'a, D> {
    /// The engine of a stack described by `info`, or a refusal saying why
    /// there is none to use.
    pub fn new(docker: &'a D, info: Option<&EngineInfo>, what: &str) -> Result<Self> {
        match info.and_then(EngineInfo::ready) {
            Some(c) => Ok(EngineAdmin {
                docker,
                container: c.to_string(),
            }),
            None => bail!(needs_engine(what)),
        }
    }

    pub fn container(&self) -> &str {
        &self.container
    }

    fn exec(&self, verb: AdminVerb) -> Result<String> {
        let out = self
            .docker
            .docker(&admin_argv(&self.container, verb), ADMIN_DEADLINE)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `admin status`, as it is: no version check (for [`remove`], which
    /// must work whatever the engine's version).
    pub fn status_unchecked(&self) -> Result<AdminStatus> {
        let text = self.exec(AdminVerb::Status)?;
        let line = text.lines().rfind(|l| l.starts_with('{')).unwrap_or("");
        serde_json::from_str(line).with_context(|| {
            format!(
                "the engine on {} answered `admin status` with something the Manager can't \
                 read; it may predate this Manager",
                self.docker.place()
            )
        })
    }

    /// `admin status`, refused when the engine's API major isn't this
    /// Manager's (m3-3, C-08).
    pub fn status(&self) -> Result<AdminStatus> {
        let st = self.status_unchecked()?;
        check_api(&st.api).map_err(|e: EngineError| anyhow!(e))?;
        Ok(st)
    }

    /// The version check, then `verb`. Returns what the engine printed.
    pub fn run(&self, verb: AdminVerb) -> Result<String> {
        self.status()?;
        if verb == AdminVerb::Status {
            return Ok(String::new());
        }
        self.exec(verb)
    }
}

/// Turn the engine on for a stack on this computer and start it: the image
/// must be there (built locally until slice C pins one), then
/// [`engine::enable`], then `compose up -d --remove-orphans`. The caller has
/// the user's opt-in (the CLI's `--experimental`, the GUI's setting).
pub fn enable_local(stack: &Stack) -> Result<()> {
    let mut probe = docker();
    probe.args(["image", "inspect", engine::ENGINE_IMAGE]);
    if crate::docker::run_capture(probe, "find the engine image").is_err() {
        bail!(
            "no {} image on this computer — build it first with engine/build.sh",
            engine::ENGINE_IMAGE
        );
    }
    engine::enable(stack)?;
    let mut c = compose(stack);
    c.args(["up", "-d", "--remove-orphans"]);
    crate::docker::run_capture(c, "start the stack with the engine").map(|_| ())
}

/// The steps of [`remove`], so its order and failure handling are tested
/// against a fake. [`LocalRemove`] is the real one; a server runs it through
/// its own `atelier engine remove`.
pub trait RemoveSteps {
    /// The engine as compose sees it, or `None` when the stack doesn't use one.
    fn info(&mut self) -> Result<Option<EngineInfo>>;
    /// `admin status`, unchecked.
    fn engine_state(&mut self) -> Result<AdminStatus>;
    /// Start db and app: through the engine when `via_engine`, else compose.
    fn wake(&mut self, via_engine: bool) -> Result<()>;
    /// Take the engine's lines out of `.env` and remove its files.
    fn disable(&mut self) -> Result<()>;
    /// `compose up -d --remove-orphans`.
    fn up(&mut self) -> Result<()>;
    /// `(service, state)` of every container compose now lists.
    fn services(&mut self) -> Result<Vec<(String, String)>>;
}

/// What [`remove`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    /// There was no engine to remove.
    NotThere,
    /// Removed; edge, app and db run.
    Done,
}

/// Remove the engine and put today's stack back (m3-6, C-09):
///
/// 1. Refuse while locked down: removing the engine starts `app` and `db`
///    again, which a lockdown keeps stopped. Refuse too when the engine isn't
///    running, since then nothing can say whether it is locked down.
/// 2. Wake a resting site (through the engine when its API is this
///    Manager's, else with compose).
/// 3. Disable: `.env` first, then the two files.
/// 4. `up -d --remove-orphans`: the engine container goes, edge returns to
///    `edge.conf`.
/// 5. Verify that edge, app and db run and nothing else is left.
///
/// Each failure stops the flow and says what state the stack is in.
pub fn remove(steps: &mut dyn RemoveSteps, say: &mut dyn FnMut(&str)) -> Result<Removed> {
    let Some(info) = steps.info()? else {
        say("This site has no engine; nothing to remove.");
        return Ok(Removed::NotThere);
    };
    if info.ready().is_none() {
        bail!(
            "the engine is enabled but its container isn't running, so the Manager can't tell \
             whether the site is locked down; removing it would start app and db. Nothing was \
             changed. Start it first (`docker compose up -d` in the stack directory), or, if \
             you are sure the site isn't locked down, run `atelier engine disable \
             --experimental` there"
        );
    }
    let state = steps.engine_state()?;
    if state.lockdown {
        bail!(
            "the site is locked down; removing the engine would start app and db again. \
             Unlock it first (`site unlock`). Nothing was changed"
        );
    }
    if !state.awake() {
        say("Waking the site first…");
        let via_engine = check_api(&state.api).is_ok();
        steps
            .wake(via_engine)
            .context("couldn't wake the site; nothing was removed")?;
    }
    say("Removing the engine's files…");
    steps
        .disable()
        .context("couldn't take the engine out of the stack's files; the engine still runs")?;
    say("Restarting the stack without the engine…");
    steps.up().context(
        "the engine's files are removed, but `docker compose up -d --remove-orphans` failed; \
         run `atelier app start` to bring the stack back",
    )?;
    let services = steps.services()?;
    let mut running: Vec<&str> = services
        .iter()
        .filter(|(_, st)| st == "running")
        .map(|(s, _)| s.as_str())
        .collect();
    running.sort_unstable();
    let leftover: Vec<&str> = services
        .iter()
        .map(|(s, _)| s.as_str())
        .filter(|s| !DEFAULT_SERVICES.contains(s))
        .collect();
    if running != DEFAULT_SERVICES || !leftover.is_empty() {
        bail!(
            "the engine is removed, but the stack isn't back to edge, app and db: running \
             {running:?}{}. Check `atelier doctor`",
            if leftover.is_empty() {
                String::new()
            } else {
                format!(", also present {leftover:?}")
            }
        );
    }
    say("Engine removed: edge, app and db run as before. Its volumes are kept.");
    Ok(Removed::Done)
}

/// [`RemoveSteps`] for a stack on this computer (also what a server's own
/// `atelier engine remove` runs).
pub struct LocalRemove<'a> {
    pub stack: &'a Stack,
    info: Option<EngineInfo>,
}

impl<'a> LocalRemove<'a> {
    pub fn new(stack: &'a Stack) -> Self {
        LocalRemove { stack, info: None }
    }
}

impl RemoveSteps for LocalRemove<'_> {
    fn info(&mut self) -> Result<Option<EngineInfo>> {
        self.info = probe(self.stack);
        Ok(self.info.clone())
    }

    fn engine_state(&mut self) -> Result<AdminStatus> {
        EngineAdmin::new(&LocalDocker, self.info.as_ref(), "remove")?.status_unchecked()
    }

    fn wake(&mut self, via_engine: bool) -> Result<()> {
        if via_engine {
            EngineAdmin::new(&LocalDocker, self.info.as_ref(), "wake")?.run(AdminVerb::Wake)?;
            return Ok(());
        }
        let mut c = compose(self.stack);
        c.args(["start", "db", "app"]);
        crate::docker::run_capture(c, "start db and app").map(|_| ())
    }

    fn disable(&mut self) -> Result<()> {
        engine::disable(self.stack)
    }

    fn up(&mut self) -> Result<()> {
        let mut c = compose(self.stack);
        c.args(["up", "-d", "--remove-orphans"]);
        crate::docker::run_capture(c, "restart the stack without the engine").map(|_| ())
    }

    fn services(&mut self) -> Result<Vec<(String, String)>> {
        let mut c = compose(self.stack);
        c.args(["ps", "-a", "--format", "json"]);
        let out = crate::docker::run_capture(c, "list the stack's services")?;
        Ok(parse_ps(&out)
            .into_iter()
            .map(|(service, state, _)| (service, state))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn info(running: bool) -> EngineInfo {
        EngineInfo {
            enabled: true,
            container: Some("atelier-blog-engine-1".into()),
            running,
        }
    }

    #[test]
    fn engine_present_routes_its_verbs_through_docker_exec() {
        let on = info(true);
        let e = |a| route(a, Some(&on));
        let via = Via::Engine("atelier-blog-engine-1".into());
        assert_eq!(e(Lifecycle::Status), via);
        assert_eq!(e(Lifecycle::Wake), via);
        assert_eq!(e(Lifecycle::Lockdown), via);
        assert_eq!(e(Lifecycle::Unlock), via);
        assert_eq!(e(Lifecycle::Freeze), Via::HostCli);
        assert_eq!(e(Lifecycle::Live), Via::HostCli);
    }

    #[test]
    fn without_a_running_engine_the_host_cli_stays_and_lockdown_is_refused() {
        let stopped = info(false);
        let bad_name = EngineInfo {
            container: Some("x; rm -rf /".into()),
            ..info(true)
        };
        let disabled = EngineInfo::default();
        for engine in [None, Some(&stopped), Some(&bad_name), Some(&disabled)] {
            for a in [
                Lifecycle::Status,
                Lifecycle::Wake,
                Lifecycle::Freeze,
                Lifecycle::Live,
            ] {
                assert_eq!(route(a, engine), Via::HostCli, "{a:?} {engine:?}");
            }
            for (a, word) in [
                (Lifecycle::Lockdown, "lockdown"),
                (Lifecycle::Unlock, "unlock"),
            ] {
                match route(a, engine) {
                    Via::Refuse(m) => {
                        assert!(m.contains("Enable the engine (experimental)"), "{m}");
                        assert!(
                            m.contains("run it on the server") && m.contains(word),
                            "{m}"
                        );
                    }
                    other => panic!("{a:?}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn container_names_are_checked_before_they_reach_a_command() {
        for ok in ["atelier-engine-1", "atelier-blog-engine-1", "a_b.c"] {
            assert!(valid_container(ok), "{ok}");
        }
        for bad in ["", "-x", "a b", "a;b", "a$(x)", "a/b", &"a".repeat(129)] {
            assert!(!valid_container(bad), "{bad}");
        }
        assert_eq!(
            admin_argv("atelier-engine-1", AdminVerb::Lockdown),
            [
                "exec",
                "atelier-engine-1",
                "atelier",
                "engine",
                "admin",
                "lockdown"
            ]
        );
    }

    #[test]
    fn compose_ps_parses_both_shapes() {
        let lines = "{\"Service\":\"engine\",\"State\":\"running\",\"Name\":\"p-engine-1\"}\n\
                     {\"Service\":\"app\",\"State\":\"Exited\",\"Name\":\"p-app-1\"}";
        let arr = "[{\"Service\":\"engine\",\"State\":\"running\",\"Name\":\"p-engine-1\"}]";
        assert_eq!(
            parse_ps(lines)[1],
            ("app".into(), "exited".into(), "p-app-1".into())
        );
        assert_eq!(parse_ps(arr)[0].2, "p-engine-1");
        assert!(parse_ps("").is_empty());
    }

    /// Answers `docker` with scripted outputs, records the calls.
    #[derive(Default)]
    struct FakeDocker {
        replies: RefCell<Vec<Result<String, String>>>,
        calls: RefCell<Vec<String>>,
    }

    impl DockerExec for FakeDocker {
        fn place(&self) -> String {
            "test".into()
        }
        fn docker(&self, args: &[String], _: Duration) -> Result<Output> {
            self.calls.borrow_mut().push(args.join(" "));
            match self.replies.borrow_mut().remove(0) {
                Ok(stdout) => Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: stdout.into_bytes(),
                    stderr: vec![],
                }),
                Err(e) => Err(anyhow!(e)),
            }
        }
    }

    const STATUS_V1: &str = r#"{"api":"1.0.0","engine":"0.12.0","app":"running","db":"running","lockdown":false,"wake_paused":false,"wake_code_set":false}"#;

    #[test]
    fn admin_verbs_read_the_version_first_and_refuse_another_major() {
        let d = FakeDocker::default();
        d.replies
            .borrow_mut()
            .extend([Ok(STATUS_V1.to_string()), Ok("Locked down.".to_string())]);
        let on = info(true);
        let a = EngineAdmin::new(&d, Some(&on), "lockdown").unwrap();
        assert_eq!(a.run(AdminVerb::Lockdown).unwrap(), "Locked down.");
        assert_eq!(
            *d.calls.borrow(),
            [
                "exec atelier-blog-engine-1 atelier engine admin status",
                "exec atelier-blog-engine-1 atelier engine admin lockdown"
            ]
        );

        for (api, says) in [
            ("2.0.0", "Update the Manager"),
            ("0.4.0", "Update the site"),
        ] {
            let d = FakeDocker::default();
            d.replies
                .borrow_mut()
                .push(Ok(STATUS_V1.replace("1.0.0", api)));
            let a = EngineAdmin::new(&d, Some(&on), "lockdown").unwrap();
            let e = a.run(AdminVerb::Lockdown).unwrap_err().to_string();
            assert!(e.contains(says), "{api}: {e}");
            assert_eq!(d.calls.borrow().len(), 1, "no verb after a refused version");
        }
        assert!(EngineAdmin::new(&d, Some(&info(false)), "unlock").is_err());
        assert!(EngineAdmin::new(&d, None, "unlock").is_err());
    }

    /// A scripted stack for [`remove`].
    struct Steps {
        info: Option<EngineInfo>,
        state: AdminStatus,
        fail: Option<&'static str>,
        after: Vec<(String, String)>,
        log: Vec<String>,
    }

    impl Steps {
        fn new(app: &str) -> Self {
            Steps {
                info: Some(info(true)),
                state: AdminStatus {
                    api: "1.0.0".into(),
                    engine: String::new(),
                    app: app.into(),
                    db: app.into(),
                    lockdown: false,
                    wake_paused: false,
                    wake_code_set: false,
                },
                fail: None,
                after: ["edge", "app", "db"]
                    .iter()
                    .map(|s| (s.to_string(), "running".to_string()))
                    .collect(),
                log: vec![],
            }
        }
        fn step(&mut self, name: &str) -> Result<()> {
            self.log.push(name.to_string());
            if self.fail == Some(name.split(' ').next().unwrap()) {
                bail!("{name} broke");
            }
            Ok(())
        }
    }

    impl RemoveSteps for Steps {
        fn info(&mut self) -> Result<Option<EngineInfo>> {
            Ok(self.info.clone())
        }
        fn engine_state(&mut self) -> Result<AdminStatus> {
            self.step("state")?;
            Ok(self.state.clone())
        }
        fn wake(&mut self, via_engine: bool) -> Result<()> {
            self.step(if via_engine {
                "wake engine"
            } else {
                "wake compose"
            })
        }
        fn disable(&mut self) -> Result<()> {
            self.step("disable")
        }
        fn up(&mut self) -> Result<()> {
            self.step("up")
        }
        fn services(&mut self) -> Result<Vec<(String, String)>> {
            self.step("services")?;
            Ok(self.after.clone())
        }
    }

    fn run(s: &mut Steps) -> Result<Removed> {
        remove(s, &mut |_| {})
    }

    #[test]
    fn remove_wakes_a_resting_site_then_disables_ups_and_verifies() {
        let mut s = Steps::new("exited");
        assert_eq!(run(&mut s).unwrap(), Removed::Done);
        assert_eq!(s.log, ["state", "wake engine", "disable", "up", "services"]);

        // Awake already: no wake.
        let mut s = Steps::new("running");
        run(&mut s).unwrap();
        assert_eq!(s.log, ["state", "disable", "up", "services"]);

        // An engine of another major: woken with compose, not its admin.
        let mut s = Steps::new("exited");
        s.state.api = "2.0.0".into();
        run(&mut s).unwrap();
        assert_eq!(s.log[1], "wake compose");
    }

    #[test]
    fn remove_refuses_while_locked_down_or_when_the_engine_cant_be_asked() {
        let mut s = Steps::new("exited");
        s.state.lockdown = true;
        let e = run(&mut s).unwrap_err().to_string();
        assert!(
            e.contains("locked down") && e.contains("Nothing was changed"),
            "{e}"
        );
        assert_eq!(s.log, ["state"]);

        let mut s = Steps::new("running");
        s.info = Some(info(false));
        let e = run(&mut s).unwrap_err().to_string();
        assert!(
            e.contains("isn't running") && e.contains("Nothing was changed"),
            "{e}"
        );
        assert!(s.log.is_empty());

        let mut s = Steps::new("running");
        s.info = None;
        assert_eq!(run(&mut s).unwrap(), Removed::NotThere);
        assert!(s.log.is_empty());
    }

    #[test]
    fn each_failure_stops_the_flow_and_says_where_the_stack_is() {
        for (fail, done, says) in [
            ("wake", vec!["state", "wake engine"], "nothing was removed"),
            (
                "disable",
                vec!["state", "wake engine", "disable"],
                "the engine still runs",
            ),
            (
                "up",
                vec!["state", "wake engine", "disable", "up"],
                "atelier app start",
            ),
        ] {
            let mut s = Steps::new("exited");
            s.fail = Some(fail);
            let e = format!("{:#}", run(&mut s).unwrap_err());
            assert!(e.contains(says), "{fail}: {e}");
            assert_eq!(s.log, done, "{fail}");
        }
    }

    #[test]
    fn remove_verifies_exactly_edge_app_and_db() {
        let mut s = Steps::new("running");
        s.after.push(("engine".into(), "running".into()));
        let e = run(&mut s).unwrap_err().to_string();
        assert!(e.contains("engine"), "{e}");

        let mut s = Steps::new("running");
        s.after[1].1 = "exited".into();
        let e = run(&mut s).unwrap_err().to_string();
        assert!(e.contains("isn't back to edge, app and db"), "{e}");
    }
}
