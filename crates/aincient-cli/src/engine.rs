//! `atelier engine …`: the engine sidecar, EXPERIMENTAL and opt-in only
//! (DECISIONS 0483). Hidden from `--help` until it is on by default.
//!
//! - `serve`, `admin …`: inside the engine container (the server lives in the
//!   `aincient-engine` crate).
//! - `enable`, `disable`: on the host, for a local stack: write or remove the
//!   opt-in files ([`aincient_core::engine`]) and apply them with compose.

use aincient_core::docker::{compose, docker, run_capture, run_inherited};
use aincient_core::engine::{self as optin, ENGINE_IMAGE};
use aincient_core::Stack;
use aincient_engine::admin::AdminCommand;
use anyhow::{bail, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum EngineCommand {
    /// Run the engine API (inside the engine container). Refuses to start
    /// unless ATELIER_ENGINE_EXPERIMENTAL=1.
    Serve {
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
    },
    /// Owner commands over `docker exec` (inside the engine container).
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Add the EXPERIMENTAL engine to this site. It holds the host's Docker
    /// socket: root on this machine.
    Enable {
        /// Say you understand the engine is experimental and root on the host.
        #[arg(long)]
        experimental: bool,
    },
    /// Remove the engine and put the stack back as it was. Its volumes stay.
    Disable,
}

/// Whether the command runs inside the container, before any site lookup.
pub fn is_in_container(cmd: &EngineCommand) -> bool {
    matches!(
        cmd,
        EngineCommand::Serve { .. } | EngineCommand::Admin { .. }
    )
}

pub fn run_in_container(cmd: EngineCommand) -> Result<()> {
    match cmd {
        EngineCommand::Serve { listen } => {
            aincient_engine::serve(&listen, |k| std::env::var(k).ok())
        }
        EngineCommand::Admin { command } => aincient_engine::admin(command),
        EngineCommand::Enable { .. } | EngineCommand::Disable => {
            unreachable!("host commands go through run_on_host")
        }
    }
}

pub fn run_on_host(cmd: EngineCommand, stack: &Stack) -> Result<()> {
    match cmd {
        EngineCommand::Enable { experimental } => enable(stack, experimental),
        EngineCommand::Disable => disable(stack),
        EngineCommand::Serve { .. } | EngineCommand::Admin { .. } => {
            unreachable!("container commands go through run_in_container")
        }
    }
}

fn enable(stack: &Stack, experimental: bool) -> Result<()> {
    if !experimental {
        bail!(
            "the engine is EXPERIMENTAL (DECISIONS 0483): it holds this host's Docker socket, \
             which is root here, and is reachable through edge at /_atelier/engine/. \
             Re-run with --experimental if that is what you want."
        );
    }
    let mut probe = docker();
    probe.args(["image", "inspect", ENGINE_IMAGE]);
    if run_capture(probe, "find the engine image").is_err() {
        bail!("no {ENGINE_IMAGE} image — build it first with engine/build.sh");
    }
    optin::enable(stack)?;
    let mut c = compose(stack);
    c.args(["up", "-d", "--remove-orphans"]);
    run_inherited(c, "start the stack with the engine")?;
    println!(
        "Engine enabled (EXPERIMENTAL). API: http://localhost:{}/_atelier/engine/v1/version",
        stack.http_port()
    );
    println!(
        "Pair the first Manager with: docker exec {}-engine-1 atelier engine admin install-code",
        stack.project_name()
    );
    Ok(())
}

fn disable(stack: &Stack) -> Result<()> {
    optin::disable(stack)?;
    // The engine container is now an orphan, and edge's config changed back
    // to edge.conf, so `up` removes the one and recreates the other.
    let mut c = compose(stack);
    c.args(["up", "-d", "--remove-orphans"]);
    run_inherited(c, "restart the stack without the engine")?;
    println!(
        "Engine disabled. Its volumes stay: docker volume rm {p}_engine-inbox {p}_engine-outbox {p}_engine-state",
        p = stack.project_name()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    fn parse(args: &[&str]) -> EngineCommand {
        let mut all = vec!["atelier", "engine"];
        all.extend_from_slice(args);
        match Cli::try_parse_from(all).unwrap().command {
            Command::Engine { command } => command,
            _ => panic!("not an engine command"),
        }
    }

    #[test]
    fn container_commands_skip_the_site_lookup() {
        assert!(is_in_container(&parse(&["serve"])));
        assert!(is_in_container(&parse(&["admin", "install-code"])));
        assert!(is_in_container(&parse(&["admin", "approve", "ABCD-EFGH"])));
        assert!(!is_in_container(&parse(&["enable", "--experimental"])));
        assert!(!is_in_container(&parse(&["disable"])));
    }

    #[test]
    fn enable_needs_the_experimental_flag_and_touches_nothing_without_it() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-engine-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let e = run_on_host(
            EngineCommand::Enable {
                experimental: false,
            },
            &stack,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("EXPERIMENTAL") && e.contains("root"), "{e}");
        assert!(!dir.exists());
    }
}
