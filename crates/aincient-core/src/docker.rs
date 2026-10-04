//! Thin wrapper over the `docker` / `docker compose` CLIs.
//!
//! The manager never talks to the Docker socket directly — it shells out, exactly
//! as a user would. This keeps behaviour identical to `install.sh` and avoids a
//! heavy API-client dependency.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::stack::Stack;

/// Result of checking that the host can run the appliance at all.
///
/// Docker is the real prerequisite floor (a CMS needs PHP + a database + storage),
/// so both front-ends gate on this and show actionable guidance rather than a raw
/// Docker error.
#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub docker_installed: bool,
    pub docker_running: bool,
    pub compose_available: bool,
}

impl Preflight {
    pub fn ok(&self) -> bool {
        self.docker_installed && self.docker_running && self.compose_available
    }

    /// A human-facing, actionable description of the first unmet requirement.
    pub fn problem(&self) -> Option<String> {
        if !self.docker_installed {
            Some(
                "Docker isn't installed. Install Docker Desktop (Mac/Windows) or Docker \
                 Engine (Linux), then try again. Atelier runs as a container, so Docker is \
                 required."
                    .to_string(),
            )
        } else if !self.docker_running {
            Some("Docker is installed but not running. Start Docker, then try again.".to_string())
        } else if !self.compose_available {
            Some(
                "The Docker Compose plugin is missing. Install it (it ships with Docker \
                 Desktop), then try again — check with `docker compose version`."
                    .to_string(),
            )
        } else {
            None
        }
    }

    /// Fail with [`problem`](Self::problem) unless every requirement is met.
    pub fn require(&self) -> Result<()> {
        match self.problem() {
            Some(msg) => bail!(msg),
            None => Ok(()),
        }
    }
}

/// Probe the host for Docker + the Compose plugin.
pub fn preflight() -> Preflight {
    let docker_installed = quiet(docker().arg("--version"));
    let docker_running = docker_installed && quiet(docker().arg("info"));
    let compose_available = docker_installed && quiet(docker().args(["compose", "version"]));
    Preflight {
        docker_installed,
        docker_running,
        compose_available,
    }
}

/// A `docker compose` invocation rooted at the stack directory, so it picks up
/// the stack's `compose.yaml` and `.env` exactly as a manual run would.
pub fn compose(stack: &Stack) -> Command {
    let mut c = docker();
    // Pin the project name explicitly so a custom `ATELIER_HOME` is a truly
    // independent stack. `-p` outranks the literal `name:` in the on-disk
    // compose.yaml; the default `~/.atelier` resolves to `atelier`, matching that
    // literal, so existing installs address the exact same containers/volumes.
    c.arg("compose")
        .args(["-p", &stack.project_name()])
        .current_dir(&stack.home);
    c
}

/// A bare `docker` invocation with a `PATH` that includes the common Docker
/// install locations.
///
/// Every docker command in the manager is built here so it behaves the same no
/// matter how the manager itself was launched. A GUI `.app` opened from Finder or
/// the Dock inherits launchd's minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`) —
/// not the login shell's — so OrbStack, Docker Desktop, Homebrew, Rancher Desktop
/// and colima installs (which live under `~/.orbstack/bin`, `/usr/local/bin`,
/// `/opt/homebrew/bin`, …) are invisible and `docker` looks "not installed", even
/// though the same binary resolves fine from a terminal. Appending those dirs to
/// the inherited `PATH` fixes GUI launches while leaving CLI/terminal launches —
/// which already have a full `PATH` — to keep resolving `docker` exactly as before.
pub fn docker() -> Command {
    let mut c = Command::new("docker");
    c.env("PATH", augmented_path());
    c
}

/// The process `PATH` with the well-known Docker install dirs appended.
///
/// Existing entries keep priority (so a terminal launch resolves `docker` from the
/// user's own `PATH` unchanged); the extras only help when a dir is otherwise
/// missing. Non-existent dirs are harmless — the OS skips them during lookup.
fn augmented_path() -> String {
    let mut parts: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut extras: Vec<PathBuf> = Vec::new();
    if let Some(home) = &home {
        // OrbStack, Rancher Desktop, Docker Desktop user bin, generic user bin.
        for sub in [".orbstack/bin", ".rd/bin", ".docker/bin", ".local/bin"] {
            extras.push(home.join(sub));
        }
    }
    extras.extend(
        [
            "/usr/local/bin",                 // Docker Desktop, Homebrew (Intel), colima
            "/opt/homebrew/bin",              // Homebrew (Apple Silicon)
            "/home/linuxbrew/.linuxbrew/bin", // Homebrew (Linux)
            "/Applications/Docker.app/Contents/Resources/bin", // Docker Desktop (macOS)
        ]
        .iter()
        .map(PathBuf::from),
    );

    for dir in extras {
        if !parts.contains(&dir) {
            parts.push(dir);
        }
    }
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Run a command, discard output, return whether it succeeded — within
/// [`LOCAL_PROBE_TIMEOUT`], so a wedged Docker Desktop reads as "not running"
/// instead of freezing whoever asked.
fn quiet(cmd: &mut Command) -> bool {
    output_within(cmd, LOCAL_PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Deadline for a probe that only talks to the local Docker daemon (`docker
/// info`, `compose ps`, `image inspect`, …). Generous, because a healthy daemon
/// answers in well under a second and the only thing this guards against is one
/// that never answers (Docker Desktop stalls `docker info` when its own backend is
/// wedged — manager#6).
pub const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Deadline for a probe that has to reach the registry (`buildx imagetools
/// inspect`). On a flaky network DNS can resolve while TCP/TLS stalls, which left
/// the update check hanging for minutes (manager#6).
pub const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for the connectivity pre-check in front of every registry probe — DNS
/// resolution plus one TCP connect. Short on purpose: it decides "offline" before
/// a buildx process is ever spawned.
pub const CONNECTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

/// How often [`output_within`] checks whether the child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Why [`output_within`] produced no output.
#[derive(Debug)]
pub enum RunError {
    /// The program couldn't be started (or waited on) at all.
    Io {
        program: String,
        source: std::io::Error,
    },
    /// It ran past its deadline and was killed.
    TimedOut { command: String, after: Duration },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Io { program, source } => write!(f, "couldn't run {program}: {source}"),
            RunError::TimedOut { command, after } => write!(
                f,
                "`{command}` didn't finish within {} and was stopped",
                human_duration(*after)
            ),
        }
    }
}

impl std::error::Error for RunError {}

/// `15s`, `300ms` — whichever reads naturally for the deadline at hand.
fn human_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 && d.as_secs() > 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// A short, recognisable rendering of a command for an error message: the program
/// and its leading arguments, stopping before a `--format` template (those are
/// long and unreadable) and capping the count, so the message names *which*
/// command timed out without dumping it.
fn describe(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    for arg in cmd.get_args().take(6) {
        let arg = arg.to_string_lossy();
        if arg.starts_with("--format") || arg.contains('{') || arg.contains('\n') {
            break;
        }
        parts.push(arg.into_owned());
    }
    parts.join(" ")
}

/// Spawn `cmd`, capture its stdout/stderr, and wait at most `deadline` for it.
///
/// The one place a probe's subprocess gets a deadline (manager#6). On expiry the
/// child is killed — on Unix its whole process group, because `docker` runs CLI
/// plugins such as `docker-buildx` as child processes that would otherwise outlive
/// it and pile up — and reaped, so nothing is left behind, and the error names the
/// command and the deadline it blew.
///
/// Both pipes are drained on their own threads from the start, so a chatty child
/// can never deadlock on a full pipe buffer while we wait for it to exit.
///
/// Only for probes. Long, user-visible operations (pull, up, backup, restore) must
/// keep running for as long as they need — they use [`run_streaming`],
/// [`run_inherited`] or [`run_capture`], which have no deadline.
pub fn output_within(cmd: &mut Command, deadline: Duration) -> Result<Output, RunError> {
    let started = Instant::now();
    let command = describe(cmd);
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a timeout can take the plugin children too.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|source| RunError::Io {
        program: program.clone(),
        source,
    })?;

    // Each reader sends (is_stderr, bytes) once its pipe closes.
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_stderr, pipe) in [
        (
            false,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            true,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        if let Some(mut pipe) = pipe {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((is_stderr, buf));
            });
        }
    }
    drop(tx);

    let timed_out = |child: &mut Child| {
        kill_and_reap(child);
        RunError::TimedOut {
            command: command.clone(),
            after: deadline,
        }
    };

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let elapsed = started.elapsed();
                if elapsed >= deadline {
                    return Err(timed_out(&mut child));
                }
                thread::sleep(POLL_INTERVAL.min(deadline - elapsed));
            }
            Err(source) => {
                kill_and_reap(&mut child);
                return Err(RunError::Io { program, source });
            }
        }
    };

    // The child has exited; its pipes close with it unless something it spawned
    // inherited them, so collecting the output is bounded by the same deadline
    // (plus a moment of grace for the readers to hand over what they read).
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let collect_by = started + deadline + Duration::from_millis(500);
    for _ in 0..2 {
        let left = collect_by.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, buf)) => stderr = buf,
            Ok((false, buf)) => stdout = buf,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A grandchild is holding a pipe open past the deadline.
                kill_group(&child);
                return Err(RunError::TimedOut {
                    command,
                    after: deadline,
                });
            }
        }
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Kill a timed-out child (and, on Unix, its process group) and wait for it, so
/// it never lingers as a zombie.
fn kill_and_reap(child: &mut Child) {
    kill_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL the child's process group — which [`output_within`] made the child
/// lead, so the group id is its pid. Catches the plugin processes `docker` spawns.
#[cfg(unix)]
fn kill_group(child: &Child) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: `kill(2)` takes plain integers and has no memory-safety
        // preconditions; a negative pid addresses the process group we created
        // for this child (and only that group).
        unsafe {
            kill(-pid, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_child: &Child) {}

/// The `host:port` a registry probe for `image` has to reach — the registry part
/// of the reference, or Docker Hub when the reference names none. Our images live
/// on `ghcr.io`, so in practice this is `ghcr.io:443`; deriving it keeps a fork on
/// another registry (or a local one) from being mis-reported as offline.
pub fn registry_endpoint(image: &str) -> (String, u16) {
    let first = image.split('/').next().unwrap_or_default();
    let names_registry =
        image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    if !names_registry {
        return ("registry-1.docker.io".to_string(), 443);
    }
    match first.rsplit_once(':') {
        Some((host, port)) => match port.parse() {
            Ok(port) => (host.to_string(), port),
            Err(_) => (first.to_string(), 443),
        },
        None => (first.to_string(), 443),
    }
}

/// Can we open a TCP connection to `host:port` within `budget`?
///
/// The fast "are we online at all" check in front of a registry probe. Name
/// resolution runs on its own thread because `getaddrinfo` has no timeout of its
/// own and can hang on a broken network; the whole check — resolve plus connect —
/// is bounded by `budget`. A resolver thread that overruns is abandoned, not
/// waited on.
pub fn can_reach(host: &str, port: u16, budget: Duration) -> bool {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<Vec<SocketAddr>>();
    let target = (host.to_string(), port);
    thread::spawn(move || {
        let addrs = target
            .to_socket_addrs()
            .map(|a| a.collect())
            .unwrap_or_default();
        let _ = tx.send(addrs);
    });
    let Ok(addrs) = rx.recv_timeout(budget) else {
        return false;
    };
    for addr in addrs {
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            return false;
        }
        if TcpStream::connect_timeout(&addr, left).is_ok() {
            return true;
        }
    }
    false
}

/// One structured progress event from `docker compose --progress json` — a
/// per-layer (or per-image) status line with byte counts. Fields mirror what
/// compose emits; everything but `id` is optional because different phases
/// carry different subsets (a `Downloading` tick has bytes, a `Pull complete`
/// doesn't).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullEvent {
    /// Layer digest prefix, or `"Image <ref>"` for the image-level envelope.
    pub id: String,
    /// Set on layer events: the `"Image <ref>"` id they belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// `Working` / `Done` / `Error` / `Warning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The phase: `Pulling fs layer`, `Downloading`, `Extracting`,
    /// `Pull complete`, `Already exists`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
}

impl PullEvent {
    /// Render as a plain log line — the fallback for reporters that only know
    /// how to append text.
    pub fn to_line(&self) -> String {
        let mut s = self.id.clone();
        if let Some(t) = &self.text {
            s.push(' ');
            s.push_str(t);
        }
        if let Some(p) = self.percent {
            s.push_str(&format!(" {p:.0}%"));
        }
        s
    }
}

/// Whether the local Compose plugin understands `--progress json` (Compose
/// v2.30+). Probed with a cheap `version` call so `pull` can fall back to the
/// plain text feed on older installs instead of failing on an unknown flag value.
pub fn compose_supports_json_progress() -> bool {
    quiet(docker().args(["compose", "--progress", "json", "version"]))
}

/// Run to completion, streaming stdout/stderr to the inheriting process. For
/// long, chatty operations (pull, up) where the user wants live progress.
pub fn run_inherited(mut cmd: Command, action: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to launch docker while trying to {action}"))?;
    if !status.success() {
        bail!("failed to {action} (docker exited with {status})");
    }
    Ok(())
}

/// Run to completion, forwarding every stdout/stderr line to `sink` as it
/// arrives, and fail on a non-zero exit. The streaming counterpart to
/// [`run_inherited`]: same use (long, chatty ops like pull/up) but the output is
/// relayed line-by-line instead of inheriting the terminal — so a GUI can show a
/// live log feed. Both pipes are drained on their own threads so neither blocks
/// the other.
pub fn run_streaming(mut cmd: Command, action: &str, mut sink: impl FnMut(&str)) -> Result<()> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to launch docker while trying to {action}"))?;

    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().map(|s| spawn_reader(s, tx.clone()));
    let stderr = child.stderr.take().map(|s| spawn_reader(s, tx.clone()));
    // Drop our own sender so `rx` closes once both reader threads finish.
    drop(tx);

    for line in rx {
        sink(&line);
    }
    if let Some(h) = stdout {
        let _ = h.join();
    }
    if let Some(h) = stderr {
        let _ = h.join();
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to launch docker while trying to {action}"))?;
    if !status.success() {
        bail!("failed to {action} (docker exited with {status})");
    }
    Ok(())
}

/// Forward each line of a child pipe to the shared channel until it closes.
fn spawn_reader<R: Read + Send + 'static>(
    pipe: R,
    tx: mpsc::Sender<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    })
}

/// Run, capture stdout, and fail with stderr on a non-zero exit.
pub fn run_capture(mut cmd: Command, action: &str) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to launch docker while trying to {action}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("failed to {action}:\n{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run, capture stdout, and return the *reason* on failure instead of discarding
/// it — so a caller can tell "docker isn't there" from "buildx is missing" from
/// "the registry didn't answer" and say something actionable.
///
/// [`try_capture`] collapses all three into `None`, which made the update check
/// unattributable from a bug report (aincient-labs/atelier-cms#7).
///
/// Bounded by [`LOCAL_PROBE_TIMEOUT`]; a probe that has to reach the network uses
/// [`probe_within`] with its own deadline, and a step that legitimately runs long
/// (a repair) uses [`probe_unbounded`].
pub fn probe(cmd: Command) -> std::result::Result<String, String> {
    probe_within(cmd, LOCAL_PROBE_TIMEOUT)
}

/// [`probe`] with an explicit deadline. A timeout comes back as its attributed
/// message (which command, after how long).
pub fn probe_within(mut cmd: Command, deadline: Duration) -> std::result::Result<String, String> {
    let out = output_within(&mut cmd, deadline).map_err(|e| e.to_string())?;
    probe_result(out)
}

/// [`probe`] without a deadline — for steps that are not probes but want the same
/// "stdout or the reason" result (doctor's repairs: `compose up`, `chown -R`,
/// `drush updatedb`), which must run for as long as they need.
pub fn probe_unbounded(mut cmd: Command) -> std::result::Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("couldn't run docker: {e}"))?;
    probe_result(out)
}

fn probe_result(out: Output) -> std::result::Result<String, String> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("docker exited with {}", out.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run, capture stdout, but return `None` on any failure instead of erroring.
/// For best-effort probes (image digests, container labels); bounded by
/// [`LOCAL_PROBE_TIMEOUT`], and a timeout is just another `None`.
pub fn try_capture(mut cmd: Command) -> Option<String> {
    let out = output_within(&mut cmd, LOCAL_PROBE_TIMEOUT).ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::{can_reach, output_within, registry_endpoint, PullEvent, RunError};
    use std::net::{Ipv4Addr, TcpListener};
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// Is a process with this pid still there (a zombie counts as there)?
    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The deadline kills a hung child — and the grandchild it spawned, the way
    /// `docker` spawns `docker-buildx` — and reaps it, well inside the deadline's
    /// order of magnitude, with an error naming the command and the deadline.
    #[cfg(unix)]
    #[test]
    fn a_hung_child_is_killed_and_reaped_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("aincient-deadline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pids = dir.join("pids");
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!(
            "sleep 30 & echo \"$$ $!\" > '{}'; wait",
            pids.display()
        ));

        let started = Instant::now();
        let err = output_within(&mut c, Duration::from_millis(300)).unwrap_err();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        assert!(matches!(err, RunError::TimedOut { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("`sh -c"), "{msg}");
        assert!(msg.contains("within 300ms"), "{msg}");

        let recorded = std::fs::read_to_string(&pids).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (child, grandchild) = recorded.trim().split_once(' ').unwrap();
        // The child is reaped synchronously (a zombie would still answer `kill -0`);
        // the orphaned grandchild is reaped by init, so give it a moment.
        assert!(!alive(child), "child {child} left behind");
        let gone_by = Instant::now() + Duration::from_secs(2);
        while alive(grandchild) && Instant::now() < gone_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(grandchild), "grandchild {grandchild} left behind");
    }

    #[cfg(unix)]
    #[test]
    fn a_plain_sleep_is_killed_within_the_deadline() {
        let started = Instant::now();
        let err =
            output_within(Command::new("sleep").arg("30"), Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            err.to_string(),
            "`sleep 30` didn't finish within 300ms and was stopped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fast_command_has_both_streams_captured() {
        let out = output_within(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    /// Far more output than a pipe buffer holds: waiting for exit before reading
    /// would deadlock here.
    #[cfg(unix)]
    #[test]
    fn a_chatty_command_cannot_deadlock_on_a_full_pipe() {
        let out = output_within(
            Command::new("sh").args([
                "-c",
                "head -c 1000000 /dev/zero; head -c 300000 /dev/zero >&2",
            ]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1_000_000);
        assert_eq!(out.stderr.len(), 300_000);
    }

    #[test]
    fn a_missing_program_is_an_attributed_error() {
        let err = output_within(
            &mut Command::new("aincient-no-such-program"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .starts_with("couldn't run aincient-no-such-program:"));
    }

    #[test]
    fn the_timeout_message_names_the_command_not_its_format_template() {
        let mut c = Command::new("docker");
        c.args([
            "buildx",
            "imagetools",
            "inspect",
            "ghcr.io/aincient-labs/atelier-cms:stable",
            "--format",
            "{{.Manifest.Digest}}",
        ]);
        assert_eq!(
            super::describe(&c),
            "docker buildx imagetools inspect ghcr.io/aincient-labs/atelier-cms:stable"
        );
    }

    #[test]
    fn connectivity_check_sees_an_open_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(can_reach("127.0.0.1", port, Duration::from_secs(2)));
    }

    #[test]
    fn connectivity_check_fails_on_a_closed_port() {
        // Bind then drop: the port is free, so nothing is listening on it.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!can_reach("127.0.0.1", port, Duration::from_secs(2)));
    }

    /// A blackholed address never answers; the budget, not the OS connect
    /// timeout (often a minute+), decides when we give up.
    #[test]
    fn connectivity_check_is_bounded_by_its_budget() {
        let started = Instant::now();
        assert!(!can_reach("10.255.255.1", 443, Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn registry_endpoint_reads_the_registry_off_the_image() {
        let ep = |i: &str| registry_endpoint(i);
        assert_eq!(
            ep("ghcr.io/aincient-labs/atelier-cms:stable"),
            ("ghcr.io".to_string(), 443)
        );
        assert_eq!(
            ep("localhost:5000/atelier:dev"),
            ("localhost".to_string(), 5000)
        );
        assert_eq!(
            ep("registry.example.com:8443/a/b"),
            ("registry.example.com".to_string(), 8443)
        );
        assert_eq!(
            ep("library/postgres:16"),
            ("registry-1.docker.io".to_string(), 443)
        );
        assert_eq!(ep("postgres:16"), ("registry-1.docker.io".to_string(), 443));
    }

    /// Real lines captured from `docker compose --progress json pull`.
    #[test]
    fn pull_event_parses_compose_json() {
        let layer: PullEvent = serde_json::from_str(
            r#"{"id":"3f26bc2dec0b","parent_id":"Image alpine:3.20","status":"Working","text":"Downloading","details":"2.309MB","current":2309499,"total":4092319,"percent":56}"#,
        )
        .unwrap();
        assert_eq!(layer.parent_id.as_deref(), Some("Image alpine:3.20"));
        assert_eq!(layer.current, Some(2309499));
        assert_eq!(layer.percent, Some(56.0));

        let image: PullEvent =
            serde_json::from_str(r#"{"id":"Image alpine:3.20","status":"Done","text":"Pulled"}"#)
                .unwrap();
        assert!(image.parent_id.is_none());
        assert_eq!(image.status.as_deref(), Some("Done"));
        assert_eq!(image.to_line(), "Image alpine:3.20 Pulled");
    }
}
