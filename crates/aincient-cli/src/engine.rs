//! `atelier engine …`: the engine sidecar, EXPERIMENTAL and opt-in only
//! (DECISIONS 0483). Hidden from `--help` until it is on by default.
//!
//! - `serve`, `admin …`: inside the engine container (the server lives in the
//!   `aincient-engine` crate).
//! - `enable`, `disable`: on the host, for a local stack: write or remove the
//!   opt-in files ([`aincient_core::engine`]) and apply them with compose.
//! - `probe <domain>`: the Manager's engine client against a site's URL,
//!   read-only (`version`, or with `--ask` an anonymous pairing request):
//!   shows the floor the client keeps (https only, no redirects, the API
//!   major) on a real server before pairing is in the Manager (M4).
//! - `remove`: the Manager's one action that takes the engine off and puts
//!   today's stack back (m3-6, C-09): wake, disable, `up --remove-orphans`,
//!   verify ([`aincient_core::engine_admin::remove`]). For a remote site the
//!   Manager confirms here, then the server runs its own `engine remove`.
//!
//! Every subcommand that changes the stack takes `--experimental` (m7-3), so
//! no one adds or takes away the root-holding sidecar by a stray command.

use aincient_core::docker::{compose, run_inherited};
use aincient_core::engine::{self as optin, ENGINE_IMAGE};
use aincient_core::engine_admin::{self, LocalRemove};
use aincient_core::{ChangeGuard, Stack};
use aincient_engine::admin::AdminCommand;
use anyhow::Result;
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
    /// Remove the engine's files and restart the stack without it (no wake,
    /// no checks: `remove` is the Manager's action). Its volumes stay.
    Disable {
        /// Say you mean to change the experimental engine.
        #[arg(long)]
        experimental: bool,
    },
    /// Call a site's engine with the Manager's client: `GET version` and the
    /// API-major check, or with --ask an anonymous pairing request. Changes
    /// nothing on this computer. The client refuses plain http:// unless
    /// the host is loopback.
    Probe {
        /// The site: a host name, https://host[:port], or loopback http://.
        domain: String,
        /// Ask to be paired under this name (POST pair/request): the engine
        /// answers a code to show and a handle, never a key.
        #[arg(long)]
        ask: Option<String>,
    },
    /// Take the engine off this site and put today's stack back: wake a
    /// resting site, disable, restart, check edge, app and db run. Refused
    /// while locked down. Its volumes stay.
    Remove {
        /// Say you mean to change the experimental engine.
        #[arg(long)]
        experimental: bool,
        /// Don't ask first. A live site on a server still needs its name:
        /// typed, or given with --confirm-slug.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// The refusal for an engine change without `--experimental` (m7-3).
pub fn needs_experimental(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the engine is EXPERIMENTAL (DECISIONS 0483): it holds the host's Docker socket, which \
         is root on that machine. `engine {what}` changes it; re-run with --experimental if \
         that is what you want."
    )
}

/// Refuse an engine change without `--experimental`; `Ok` for the rest.
pub fn check_experimental(cmd: &EngineCommand) -> Result<()> {
    match cmd {
        EngineCommand::Enable {
            experimental: false,
        } => Err(needs_experimental("enable")),
        EngineCommand::Disable {
            experimental: false,
        } => Err(needs_experimental("disable")),
        EngineCommand::Remove {
            experimental: false,
            ..
        } => Err(needs_experimental("remove")),
        _ => Ok(()),
    }
}

/// The question `engine remove` asks, `{site}` naming the site and its host.
pub const REMOVE_QUESTION: &str = "Remove the engine from {site}?";
pub const REMOVE_DETAIL: &str = "The engine is taken off and the stack restarted as edge, app \
     and db (a resting site is woken first). Paired Managers stop working; its volumes are kept.";

/// Whether the command runs without a site lookup: inside the container, or
/// `probe`, which names its site by URL.
pub fn needs_no_site(cmd: &EngineCommand) -> bool {
    matches!(
        cmd,
        EngineCommand::Serve { .. } | EngineCommand::Admin { .. } | EngineCommand::Probe { .. }
    )
}

pub fn run_without_site(cmd: EngineCommand) -> Result<()> {
    match cmd {
        EngineCommand::Serve { listen } => {
            aincient_engine::serve(&listen, |k| std::env::var(k).ok())
        }
        EngineCommand::Admin { command } => aincient_engine::admin(command),
        EngineCommand::Probe { domain, ask } => probe(&domain, ask.as_deref()),
        EngineCommand::Enable { .. }
        | EngineCommand::Disable { .. }
        | EngineCommand::Remove { .. } => {
            unreachable!("site commands go through run_on_host")
        }
    }
}

/// `confirm` asks the D-07 question through [`ChangeGuard`] (the CLI's
/// `confirm_change`): `(guard, detail, question, yes)`.
pub type Confirm<'a> = &'a dyn Fn(&ChangeGuard, &str, &str, bool) -> Result<bool>;

pub fn run_on_host(cmd: EngineCommand, stack: &Stack, confirm: Confirm) -> Result<()> {
    check_experimental(&cmd)?;
    match cmd {
        EngineCommand::Enable { experimental } => enable(stack, experimental),
        EngineCommand::Disable { .. } => disable(stack),
        EngineCommand::Remove { yes, .. } => {
            if optin::is_enabled(stack)
                && !confirm(
                    &ChangeGuard::local(stack),
                    REMOVE_DETAIL,
                    REMOVE_QUESTION,
                    yes,
                )?
            {
                println!("Aborted — nothing was changed.");
                return Ok(());
            }
            engine_admin::remove(&mut LocalRemove::new(stack), &mut |line| println!("{line}"))?;
            Ok(())
        }
        EngineCommand::Serve { .. } | EngineCommand::Admin { .. } | EngineCommand::Probe { .. } => {
            unreachable!("these go through run_without_site")
        }
    }
}

fn enable(stack: &Stack, experimental: bool) -> Result<()> {
    if !experimental {
        return Err(needs_experimental("enable"));
    }
    let id = engine_admin::local_image_id()?;
    optin::enable(stack, &id)?;
    println!("Engine image pinned: {ENGINE_IMAGE} = {id}");
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

/// `engine probe`: the Manager's client, its floor checked before anything
/// is sent ([`EngineUrl::from_domain`], then `ensure_secure` per request).
fn probe(domain: &str, ask: Option<&str>) -> Result<()> {
    use aincient_core::engine_client::{Auth, EngineClient, EngineUrl, Https, Method};
    let base = EngineUrl::from_domain(domain).map_err(|e| anyhow::anyhow!("refused: {e}"))?;
    let client = EngineClient::new(base.clone(), Https::new());
    let v = client
        .ensure_compatible()
        .map_err(|e| anyhow::anyhow!("{}: {e}", base.url("version")))?;
    println!(
        "{} → api {}, engine {}, experimental {}",
        base.url("version"),
        v.api,
        v.engine,
        v.experimental
    );
    if let Some(name) = ask {
        let out: serde_json::Value = client
            .call(
                Method::Post,
                "pair/request",
                Auth::None,
                Some(&serde_json::json!({ "name": name })),
            )
            .map_err(|e| anyhow::anyhow!("{}: {e}", base.url("pair/request")))?;
        println!(
            "{} → pending; show the code {} to the primary (expires in {}s)",
            base.url("pair/request"),
            out["code"].as_str().unwrap_or("?"),
            out["expires_in"]
        );
    }
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
        assert!(needs_no_site(&parse(&["serve"])));
        assert!(needs_no_site(&parse(&["admin", "install-code"])));
        assert!(needs_no_site(&parse(&["admin", "approve", "ABCD-EFGH"])));
        assert!(needs_no_site(&parse(&["probe", "example.com"])));
        assert!(!needs_no_site(&parse(&["enable", "--experimental"])));
        assert!(!needs_no_site(&parse(&["disable", "--experimental"])));
        assert!(!needs_no_site(&parse(&["remove", "--experimental"])));
    }

    #[test]
    fn enable_needs_the_experimental_flag_and_touches_nothing_without_it() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-engine-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let never = |_: &ChangeGuard, _: &str, _: &str, _: bool| -> Result<bool> {
            panic!("nothing is confirmed without --experimental")
        };
        for args in [&["enable"][..], &["disable"], &["remove", "--yes"]] {
            let e = run_on_host(parse(args), &stack, &never)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("EXPERIMENTAL") && e.contains("root") && e.contains("--experimental"),
                "{args:?}: {e}"
            );
        }
        assert!(!dir.exists());
        // In-container commands need no flag: the container is the opt-in.
        assert!(check_experimental(&parse(&["admin", "status"])).is_ok());
    }

    #[test]
    fn remove_on_a_stack_without_the_engine_asks_nothing_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-rm-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let never = |_: &ChangeGuard, _: &str, _: &str, _: bool| -> Result<bool> {
            panic!("no engine, no question")
        };
        run_on_host(parse(&["remove", "--experimental"]), &stack, &never).unwrap();
        assert!(!dir.exists());
    }
}
