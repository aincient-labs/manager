//! The Atelier engine sidecar: a closed HTTP API in front of the host's
//! Docker socket, which makes it **root on the host**. EXPERIMENTAL and
//! opt-in only (DECISIONS 0483): it runs only from the `compose.engine.yaml`
//! that `atelier engine enable` writes ([`aincient_core::engine`]), and
//! refuses to start without `ATELIER_ENGINE_EXPERIMENTAL=1`.
//!
//! Design: `studies/manager-sidecar-security.md` (umbrella), ADR 0482. Built
//! fresh from the `engine-poc` proof of concept with tests (plan m3-1, m3-2).
//! Everything that runs with the socket is in this crate, so a reviewer has
//! one place to read:
//!
//! | module | what |
//! |---|---|
//! | [`server`] | routes, auth per route, jobs, lockdown, the wake page |
//! | [`http`] | HTTP for one client: peer check at accept, deadline, caps |
//! | [`state`] | pairings by seniority, requests, codes, wake caps |
//! | [`audit`] | the append-only audit log and its retention |
//! | [`inbox`] | the Drupal ↔ engine files (inbox carries only `revoke.request`) |
//! | [`site`] | the only Docker/Compose calls the engine makes |
//! | [`proc`] | subprocesses with a deadline, an output cap and a group kill |
//! | [`codes`] | codes, tokens, names, hashing |
//! | [`admin`] | the owner's commands over `docker exec` |
#![cfg(unix)]

pub mod admin;
pub mod audit;
pub mod codes;
pub mod http;
pub mod inbox;
pub mod proc;
pub mod server;
pub mod site;
pub mod state;

use aincient_core::engine::EXPERIMENTAL_ENV;
use aincient_core::Stack;
use anyhow::{bail, Context, Result};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

/// The API version (m3-3), semver as a string, served by `GET /v1/version`.
///
/// The contract with the Manager is the **major** version, which is also the
/// `/v1/` in every path:
/// - a **minor** bump adds (a verb, a response field, an optional request
///   field); a Manager built for `1.x` keeps working against any later `1.y`;
/// - a **patch** bump changes no shape;
/// - a **major** bump may remove or change anything, and moves the paths to
///   `/v2/`. A Manager facing a newer major refuses every action and offers
///   its own update (C-08); one facing an older major offers the site's.
///
/// So: never remove or rename a field, verb or path, or tighten a request,
/// without bumping [`API_MAJOR`].
pub const API_VERSION: &str = "1.0.0";
/// The major of [`API_VERSION`]: the `/v1/` path prefix.
pub const API_MAJOR: u32 = 1;
/// The engine build: this crate's (the workspace's) version.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the engine container mounts its volumes.
pub const STATE_DIR: &str = "/state";
pub const INBOX_DIR: &str = "/inbox";
pub const OUTBOX_DIR: &str = "/outbox";
/// Edge on the engine network: the only peer admitted.
pub const EDGE: &str = "edge:80";

/// The experimental gate (m7-2): the engine starts only when
/// `ATELIER_ENGINE_EXPERIMENTAL` is exactly `1`, which only the opt-in
/// compose file sets. A copied compose file that drops it starts nothing.
pub fn experimental_gate(get: impl Fn(&str) -> Option<String>) -> Result<()> {
    if get(EXPERIMENTAL_ENV).as_deref() == Some("1") {
        return Ok(());
    }
    bail!(
        "engine: refusing to start: {EXPERIMENTAL_ENV}=1 is not set. The engine is \
         experimental (DECISIONS 0483) and holds the host's Docker socket; it runs only \
         from the compose.engine.yaml that `atelier engine enable` writes."
    )
}

/// `atelier engine serve`: the gate first, before any state is touched or a
/// port is bound, then the API on `listen`.
pub fn serve(listen: &str, get: impl Fn(&str) -> Option<String>) -> Result<()> {
    experimental_gate(&get)?;
    let home = get("ATELIER_STACK_DIR").context("ATELIER_STACK_DIR is not set")?;
    let project = get("ATELIER_PROJECT").context("ATELIER_PROJECT is not set")?;
    let state_dir = get("ATELIER_ENGINE_STATE").unwrap_or_else(|| STATE_DIR.into());
    let mut stack = Stack::at(&home);
    stack.project = Some(project);
    let store = state::Store::at(PathBuf::from(state_dir))?;
    let site = site::ComposeSite {
        stack,
        edge: EDGE.into(),
    };
    let engine = server::Engine::new(
        store,
        inbox::Inbox::at(INBOX_DIR, OUTBOX_DIR),
        Box::new(site),
        EDGE,
    )?;
    let listener = TcpListener::bind(listen).with_context(|| format!("listen on {listen}"))?;
    eprintln!(
        "engine: EXPERIMENTAL (DECISIONS 0483), listening on {listen} (API {API_VERSION}, engine {ENGINE_VERSION})"
    );
    Arc::new(engine).serve(listener)
}

/// `atelier engine admin …`, inside the engine container: `docker exec`
/// inherits the container's environment, so the site is the one it serves.
pub fn admin(cmd: admin::AdminCommand) -> Result<()> {
    let state_dir = std::env::var("ATELIER_ENGINE_STATE").unwrap_or_else(|_| STATE_DIR.into());
    let store = state::Store::at(state_dir)?;
    let site: Box<dyn site::Site> = match (
        std::env::var("ATELIER_STACK_DIR"),
        std::env::var("ATELIER_PROJECT"),
    ) {
        (Ok(home), Ok(project)) => {
            let mut stack = Stack::at(&home);
            stack.project = Some(project);
            Box::new(site::ComposeSite {
                stack,
                edge: EDGE.into(),
            })
        }
        _ => Box::new(site::NoSite),
    };
    admin::run(
        cmd,
        &store,
        &inbox::Inbox::at(INBOX_DIR, OUTBOX_DIR),
        site.as_ref(),
        &mut std::io::stdin(),
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aincient_core::engine::ENGINE_COMPOSE_TEMPLATE;
    use std::collections::HashMap;

    #[test]
    fn the_engine_and_the_managers_client_agree_on_the_major() {
        assert_eq!(API_MAJOR, aincient_core::engine::CLIENT_API_MAJOR);
    }

    #[test]
    fn the_api_version_is_semver_and_its_major_is_the_path() {
        let parts: Vec<u32> = API_VERSION.split('.').map(|p| p.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], API_MAJOR);
        assert!(server::PATHS.iter().all(|p| !p.starts_with('/')));
        assert_eq!(
            server::route("GET", &format!("/v{API_MAJOR}/version")),
            server::Route::Version
        );
    }

    /// The engine's environment as the opt-in compose file sets it.
    fn compose_env(template: &str) -> HashMap<String, String> {
        let engine = template.split("\n  engine:\n").nth(1).unwrap();
        let env = engine.split("    environment:\n").nth(1).unwrap();
        env.lines()
            .take_while(|l| l.starts_with("      "))
            .filter_map(|l| l.trim().split_once(": "))
            .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
            .collect()
    }

    #[test]
    fn the_opt_in_compose_file_passes_the_gate_and_a_copy_without_it_does_not() {
        let env = compose_env(ENGINE_COMPOSE_TEMPLATE);
        assert_eq!(env[EXPERIMENTAL_ENV], "1");
        assert!(experimental_gate(|k| env.get(k).cloned()).is_ok());

        let copied =
            ENGINE_COMPOSE_TEMPLATE.replace("      ATELIER_ENGINE_EXPERIMENTAL: \"1\"\n", "");
        assert_ne!(copied, ENGINE_COMPOSE_TEMPLATE);
        let env = compose_env(&copied);
        let get = |k: &str| env.get(k).cloned();
        let e = experimental_gate(get).unwrap_err().to_string();
        assert!(
            e.contains("refusing to start") && e.contains(EXPERIMENTAL_ENV),
            "{e}"
        );
        for v in ["0", "true", "yes", " 1", ""] {
            assert!(experimental_gate(|_| Some(v.to_string())).is_err(), "{v:?}");
        }
    }

    #[test]
    fn serve_without_the_flag_binds_nothing_and_touches_no_state() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap();
        drop(free);
        let state = std::env::temp_dir().join(format!(
            "atelier-engine-gate-{}",
            codes::hex(&codes::random_bytes::<6>())
        ));
        let env: HashMap<&str, String> = [
            ("ATELIER_STACK_DIR", "/srv/site".to_string()),
            ("ATELIER_PROJECT", "atelier".to_string()),
            ("ATELIER_ENGINE_STATE", state.display().to_string()),
        ]
        .into();
        let e = serve(&addr.to_string(), |k| env.get(k).cloned()).unwrap_err();
        assert!(e.to_string().contains("refusing to start"));
        assert!(!state.exists(), "no state made");
        // The port is still free: nothing listened.
        TcpListener::bind(addr).expect("the gate bound no port");
    }
}
