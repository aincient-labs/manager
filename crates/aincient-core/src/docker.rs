//! Thin wrapper over the `docker` / `docker compose` CLIs.
//!
//! The manager never talks to the Docker socket directly — it shells out, exactly
//! as a user would. This keeps behaviour identical to `install.sh` and avoids a
//! heavy API-client dependency.

use std::process::{Command, Stdio};

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
    let docker_installed = quiet(Command::new("docker").arg("--version"));
    let docker_running = docker_installed && quiet(Command::new("docker").arg("info"));
    let compose_available =
        docker_installed && quiet(Command::new("docker").args(["compose", "version"]));
    Preflight {
        docker_installed,
        docker_running,
        compose_available,
    }
}

/// A `docker compose` invocation rooted at the stack directory, so it picks up
/// the stack's `compose.yaml` and `.env` exactly as a manual run would.
pub fn compose(stack: &Stack) -> Command {
    let mut c = Command::new("docker");
    c.arg("compose").current_dir(&stack.home);
    c
}

/// A bare `docker` invocation.
pub fn docker() -> Command {
    Command::new("docker")
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
