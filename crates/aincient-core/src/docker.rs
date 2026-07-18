//! Thin wrapper over the `docker` / `docker compose` CLIs.
//!
//! The manager never talks to the Docker socket directly — it shells out, exactly
//! as a user would. This keeps behaviour identical to `install.sh` and avoids a
//! heavy API-client dependency.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Context, Result};
use serde::Serialize;

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
    let compose_available =
        docker_installed && quiet(docker().args(["compose", "version"]));
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
    c.arg("compose").current_dir(&stack.home);
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
            "/usr/local/bin",                                   // Docker Desktop, Homebrew (Intel), colima
            "/opt/homebrew/bin",                                // Homebrew (Apple Silicon)
            "/home/linuxbrew/.linuxbrew/bin",                   // Homebrew (Linux)
            "/Applications/Docker.app/Contents/Resources/bin",  // Docker Desktop (macOS)
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

/// Run a command, discard output, return whether it succeeded.
fn quiet(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Run, capture stdout, but return `None` on any failure instead of erroring.
/// For best-effort probes (image digests, registry inspection).
pub fn try_capture(mut cmd: Command) -> Option<String> {
    let out = cmd.output().ok()?;
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
