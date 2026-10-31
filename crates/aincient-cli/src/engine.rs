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
    /// socket: root on this machine. Prints the install code that pairs the
    /// first Manager.
    Enable,
    /// Remove the engine's files and restart the stack without it (no wake,
    /// no checks: `remove` is the Manager's action). Its volumes stay.
    Disable,
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
        /// Don't ask first. A live site on a server still needs its name:
        /// typed, or given with --confirm-slug.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Make a new one-time install code (over SSH, or on this computer): it
    /// pairs a site's first Manager as its primary. Replaces an unused one.
    InstallCode,
    /// The site's wake code (m5-3, D-10): whether one is set; with --rotate
    /// a new one, shown once; with --revoke none, so waking by code is
    /// refused. It wakes the site from /atelier while it rests, and signs
    /// nobody in.
    WakeCode {
        #[arg(long, conflicts_with = "revoke")]
        rotate: bool,
        #[arg(long)]
        revoke: bool,
    },
    /// On the site's PRIMARY Manager: Managers waiting to join (name,
    /// address, age). Their codes are on their own screens, never here.
    Requests,
    /// On the PRIMARY: let a waiting Manager in. You type the code its
    /// screen shows (asked for here; never one from a website or an email).
    Approve {
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// The Managers paired with an engine-paired site: name, primary or
    /// rank, last seen.
    Managers,
    /// Revoke a Manager junior to this one (its id from `engine managers`).
    Revoke {
        id: String,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// This Manager leaves an engine-paired site: revoke its own pairing,
    /// delete its token from the keychain, drop the site from the list.
    Forget {
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

/// Refuse an engine command without `--experimental` (m7-3, M4); `Ok` for
/// the ones that need none: inside the container (the container is the
/// opt-in) and the read-only `probe`.
pub fn check_experimental(cmd: &EngineCommand, experimental: bool) -> Result<()> {
    let word = match cmd {
        EngineCommand::Serve { .. } | EngineCommand::Admin { .. } | EngineCommand::Probe { .. } => {
            return Ok(())
        }
        EngineCommand::Enable => "enable",
        EngineCommand::Disable => "disable",
        EngineCommand::Remove { .. } => "remove",
        EngineCommand::InstallCode => "install-code",
        EngineCommand::WakeCode { .. } => "wake-code",
        EngineCommand::Requests => "requests",
        EngineCommand::Approve { .. } => "approve",
        EngineCommand::Managers => "managers",
        EngineCommand::Revoke { .. } => "revoke",
        EngineCommand::Forget { .. } => "forget",
    };
    if experimental {
        Ok(())
    } else {
        Err(needs_experimental(word))
    }
}

/// The refusal for a pairing command on a site this Manager reaches without
/// a token (on this computer, or over SSH).
pub fn not_token_paired(what: &str, place: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`engine {what}` talks to a site this Manager is paired with by token (added with \
         `atelier sites add <SLUG> --url … --install-code …` or `sites join`). This site is \
         reached {place}, where whoever runs Docker is the owner: use `docker exec \
         <project>-engine-1 atelier engine admin requests|approve <CODE>|pairings|revoke <ID>`."
    )
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
        _ => unreachable!("site commands go through run_on_host"),
    }
}

/// `confirm` asks the D-07 question through [`ChangeGuard`] (the CLI's
/// `confirm_change`): `(guard, detail, question, yes)`.
pub type Confirm<'a> = &'a dyn Fn(&ChangeGuard, &str, &str, bool) -> Result<bool>;

pub fn run_on_host(
    cmd: EngineCommand,
    stack: &Stack,
    experimental: bool,
    confirm: Confirm,
) -> Result<()> {
    check_experimental(&cmd, experimental)?;
    match cmd {
        EngineCommand::Enable => enable(stack),
        EngineCommand::Disable => disable(stack),
        EngineCommand::InstallCode => {
            let code = engine_admin::install_code(
                &engine_admin::LocalDocker,
                engine_admin::probe(stack).as_ref(),
            )?;
            println!("Site:  {}", stack.console_url());
            println!("{}", engine_admin::install_code_notice(&code));
            Ok(())
        }
        EngineCommand::WakeCode { rotate, revoke } => wake_code_admin(
            &engine_admin::LocalDocker,
            engine_admin::probe(stack).as_ref(),
            wake_code_change(rotate, revoke),
        ),
        EngineCommand::Requests => Err(not_token_paired("requests", "on this computer")),
        EngineCommand::Approve { .. } => Err(not_token_paired("approve", "on this computer")),
        EngineCommand::Managers => Err(not_token_paired("managers", "on this computer")),
        EngineCommand::Revoke { .. } => Err(not_token_paired("revoke", "on this computer")),
        EngineCommand::Forget { .. } => Err(not_token_paired("forget", "on this computer")),
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

/// `--rotate` is `Some(true)`, `--revoke` `Some(false)`, neither `None`
/// (show whether one is set).
pub fn wake_code_change(rotate: bool, revoke: bool) -> Option<bool> {
    match (rotate, revoke) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

/// `engine wake-code` through `docker exec` (this computer or SSH).
pub fn wake_code_admin<D: engine_admin::DockerExec + ?Sized>(
    docker: &D,
    info: Option<&engine_admin::EngineInfo>,
    change: Option<bool>,
) -> Result<()> {
    match change {
        None => {
            let st = engine_admin::EngineAdmin::new(docker, info, "wake-code")?.status()?;
            println!("{}", wake_code_state(st.wake_code_set, st.lockdown));
        }
        Some(true) => {
            let code =
                engine_admin::wake_code(docker, info, false)?.expect("a rotate returns its code");
            println!("{}", engine_admin::wake_code_notice(&code));
        }
        Some(false) => {
            engine_admin::wake_code(docker, info, true)?;
            println!("Wake code revoked: waking by code is refused until you make a new one.");
        }
    }
    Ok(())
}

/// Whether a wake code is set, in words (never the code: only its hash is kept).
pub fn wake_code_state(set: bool, locked: bool) -> String {
    let state = if set {
        "A wake code is set (only its hash is kept; `--rotate` makes a new one)."
    } else {
        "No wake code: waking by code is refused. `--rotate` makes one."
    };
    if locked {
        format!("{state} The site is locked down, so waking by code is off until unlock.")
    } else {
        state.to_string()
    }
}

fn enable(stack: &Stack) -> Result<()> {
    // 0487: a released Manager pulls the engine image published with it, by
    // digest; a developer build carries none and pins the local build.
    let pin = match optin::released_engine_image() {
        Some(pin) => {
            println!("Pulling the engine image {pin} …");
            engine_admin::pull_image(&pin)?;
            pin
        }
        None => {
            let id = engine_admin::local_image_id()?;
            println!(
                "Developer build: this Manager carries no released engine image, so it pins \
                 the local {ENGINE_IMAGE} build ({id})."
            );
            id
        }
    };
    optin::enable(stack, &pin, &|files, env| {
        engine_admin::compose_config_json(stack, files, env)
    })?;
    println!("Engine image pinned: {pin}");
    let mut c = compose(stack);
    c.args(["up", "-d", "--remove-orphans"]);
    run_inherited(c, "start the stack with the engine")?;
    println!(
        "Engine enabled (EXPERIMENTAL). API: http://localhost:{}/_atelier/engine/v1/version",
        stack.http_port()
    );
    // m4-6: the code that pairs the first Manager, next to the site's address.
    println!("Site:  {}", stack.console_url());
    match engine_admin::install_code(
        &engine_admin::LocalDocker,
        engine_admin::probe(stack).as_ref(),
    ) {
        Ok(code) => println!("{}", engine_admin::install_code_notice(&code)),
        Err(e) => println!(
            "No install code yet ({e:#}). Make one with: atelier engine install-code \
             --experimental (or docker exec {}-engine-1 atelier engine admin install-code)",
            stack.project_name()
        ),
    }
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
        parse_flag(args).0
    }

    /// The command and the global `--experimental`.
    fn parse_flag(args: &[&str]) -> (EngineCommand, bool) {
        let mut all = vec!["atelier", "engine"];
        all.extend_from_slice(args);
        let cli = Cli::try_parse_from(all).unwrap();
        match cli.command {
            Command::Engine { command } => (command, cli.experimental),
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
        assert!(!needs_no_site(&parse(&["managers"])));
    }

    #[test]
    fn enable_needs_the_experimental_flag_and_touches_nothing_without_it() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-engine-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let never = |_: &ChangeGuard, _: &str, _: &str, _: bool| -> Result<bool> {
            panic!("nothing is confirmed without --experimental")
        };
        for args in [
            &["enable"][..],
            &["disable"],
            &["remove", "--yes"],
            &["install-code"],
            &["wake-code", "--rotate"],
            &["requests"],
            &["approve"],
            &["managers"],
            &["revoke", "a00000000001"],
            &["forget", "-y"],
        ] {
            let (cmd, flag) = parse_flag(args);
            assert!(!flag);
            let e = run_on_host(cmd, &stack, flag, &never)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("EXPERIMENTAL") && e.contains("root") && e.contains("--experimental"),
                "{args:?}: {e}"
            );
        }
        assert!(!dir.exists());
        // In-container commands need no flag: the container is the opt-in.
        assert!(check_experimental(&parse(&["admin", "status"]), false).is_ok());
        assert!(check_experimental(&parse(&["probe", "x.com"]), false).is_ok());
        // The flag is global: before or after the subcommand.
        assert!(parse_flag(&["managers", "--experimental"]).1);
        let cli = Cli::try_parse_from(["atelier", "--experimental", "engine", "managers"]).unwrap();
        assert!(cli.experimental);
    }

    #[test]
    fn pairing_commands_on_a_site_reached_without_a_token_point_at_ssh_admin() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-pair-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let never = |_: &ChangeGuard, _: &str, _: &str, _: bool| -> Result<bool> {
            panic!("refused before any question")
        };
        for args in [
            &["requests", "--experimental"][..],
            &["approve", "--experimental"],
            &["managers", "--experimental"],
            &["revoke", "a00000000001", "--experimental"],
            &["forget", "--experimental"],
        ] {
            let (cmd, flag) = parse_flag(args);
            let e = run_on_host(cmd, &stack, flag, &never)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("by token") && e.contains("admin"),
                "{args:?}: {e}"
            );
        }
        assert!(!dir.exists());
        // Approve takes no code argument: it is typed when asked.
        let mut all = vec!["atelier", "engine", "approve", "ABCD-EFGH"];
        assert!(Cli::try_parse_from(all.drain(..)).is_err());
    }

    #[test]
    fn remove_on_a_stack_without_the_engine_asks_nothing_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("atelier-cli-rm-{}", std::process::id()));
        let stack = Stack::at(&dir);
        let never = |_: &ChangeGuard, _: &str, _: &str, _: bool| -> Result<bool> {
            panic!("no engine, no question")
        };
        let (cmd, flag) = parse_flag(&["remove", "--experimental"]);
        run_on_host(cmd, &stack, flag, &never).unwrap();
        assert!(!dir.exists());
    }
}
