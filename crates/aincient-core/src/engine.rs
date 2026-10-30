//! The engine sidecar as an **experimental opt-in** (DECISIONS 0483, plan
//! m7-1): the files that add it to a stack, and the two helpers that write and
//! remove them. The engine itself (`atelier engine serve`) lives in the
//! `aincient-engine` crate.
//!
//! The default stack never mentions the engine: [`COMPOSE_TEMPLATE`] and
//! [`EDGE_CONF_TEMPLATE`] stay byte for byte what `install.sh` writes. An owner
//! who opts in gets two more files next to them, and `.env` names the first in
//! `COMPOSE_FILE`, so every `docker compose` in the stack directory (the
//! Manager's, the engine's, or one typed over SSH) merges it over
//! `compose.yaml`:
//!
//! | file | what it does |
//! |---|---|
//! | [`ENGINE_COMPOSE_FILE`] | adds the `engine` service (Docker socket, no published port, internal `engine` network), puts `edge` on that network, gives `app` the inbox (rw) and outbox (ro) volumes, and mounts [`ENGINE_EDGE_FILE`] over edge's `default.conf` |
//! | [`ENGINE_EDGE_FILE`] | [`EDGE_CONF_TEMPLATE`] plus the engine's rate-limit zones and its `/_atelier/engine/` locations |
//!
//! Without the opt-in, edge has no `/_atelier/engine/` location at all: such a
//! path falls to edge's `location /` like any other (the snapshot's 404 when
//! frozen, Drupal's 404 when live, the "starting" 503 while `app` is down) and
//! no engine exists to reach. Compose merges a service's volumes by their
//! target, so the edge mount above *replaces* `edge.conf` rather than adding a
//! second file; `edge.conf` itself is never touched.
//!
//! Network layout (E-05): only `edge` and `engine` are on `engine`; `app` and
//! `db` never join it, and the engine also refuses any TCP peer that isn't edge.
//! Volumes: `engine-inbox` (app writes, the engine reads), `engine-outbox` (the
//! engine writes, app mounts read-only), `engine-state` (the engine only).

use crate::stack::{Stack, EDGE_CONF_TEMPLATE};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The opt-in compose file, merged over `compose.yaml` through `COMPOSE_FILE`.
pub const ENGINE_COMPOSE_FILE: &str = "compose.engine.yaml";
/// The edge config used instead of `edge.conf` while the engine is on.
pub const ENGINE_EDGE_FILE: &str = "edge.engine.conf";
/// The engine image: built locally by `engine/build.sh`, never pulled (slice C
/// replaces this with a digest-pinned reference, m7-4).
pub const ENGINE_IMAGE: &str = "atelier-engine:dev";
/// The engine refuses to start unless this is `1` (m7-2). Only
/// [`ENGINE_COMPOSE_TEMPLATE`] sets it, so a copied compose file that drops the
/// line can't start an engine by accident.
pub const EXPERIMENTAL_ENV: &str = "ATELIER_ENGINE_EXPERIMENTAL";
/// The `.env` value of `COMPOSE_FILE` while the engine is on.
pub const COMPOSE_FILE_VALUE: &str = "compose.yaml:compose.engine.yaml";
/// The `.env` keys [`enable`] writes and [`disable`] removes.
pub const ENV_KEYS: [&str; 4] = [
    "COMPOSE_FILE",
    "COMPOSE_PATH_SEPARATOR",
    "ATELIER_STACK_DIR",
    "ATELIER_PROJECT",
];

/// The engine's routes under `/_atelier/engine/v1/`, as anchored regex
/// alternatives for edge. `pair` and `wake` are not here: they have exact
/// locations of their own, each with a code-try limit. The engine's router
/// (`aincient-engine`'s `server::route`) is tested against this list.
pub const EDGE_ROUTES: &[&str] = &[
    "version",
    "status",
    "freeze",
    "live",
    "rest",
    "lockdown",
    "unlock",
    "pair/request",
    "pair/collect",
    "pairings",
    "pairings/self",
    "pairings/requests",
    "pairings/approve",
    "pairings/[0-9a-z]{12}",
    "audit",
    "wake/paired",
    "wake/resume",
];

/// The opt-in compose file, merged over [`COMPOSE_TEMPLATE`].
pub const ENGINE_COMPOSE_TEMPLATE: &str = r#"# Atelier engine: EXPERIMENTAL, opt-in only (DECISIONS 0483).
#
# Written by `atelier engine enable`, removed by `atelier engine disable`; never
# part of the default stack. `.env` names this file in COMPOSE_FILE, so every
# `docker compose` in this directory merges it over compose.yaml.
#
# The engine holds the host's Docker socket: that is root on this server. It
# publishes no port and shares the internal `engine` network with edge only;
# `app` and `db` never join it. Edge reaches it at /_atelier/engine/v1/ through
# edge.engine.conf, which replaces edge.conf only while this file is in use.
services:
  app:
    volumes:
      - engine-inbox:/opt/drupal/engine-inbox
      - engine-outbox:/opt/drupal/engine-outbox:ro
  edge:
    networks:
      - default
      - engine
    volumes:
      - ./edge.engine.conf:/etc/nginx/conf.d/default.conf:ro
  engine:
    image: atelier-engine:dev
    pull_policy: never
    environment:
      ATELIER_ENGINE_EXPERIMENTAL: "1"
      ATELIER_STACK_DIR: ${ATELIER_STACK_DIR:?run atelier engine enable}
      ATELIER_PROJECT: ${ATELIER_PROJECT:?run atelier engine enable}
      DOCKER_CONFIG: /tmp/docker
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      # The same path as on the host, so compose's relative bind mounts
      # (./edge.conf) resolve to the host's files. Read-only.
      - ${ATELIER_STACK_DIR}:${ATELIER_STACK_DIR}:ro
      - engine-inbox:/inbox
      - engine-outbox:/outbox
      - engine-state:/state
    networks:
      - engine
    # Bounded, so a flood can't exhaust the host's processes or memory.
    pids_limit: 128
    mem_limit: 256m
    read_only: true
    tmpfs:
      - /tmp
    security_opt:
      - no-new-privileges:true
    restart: unless-stopped
volumes:
  engine-inbox:
  engine-outbox:
  engine-state:
networks:
  default:
  engine:
    # No route out: the engine talks to Docker over the socket, not the network.
    internal: true
"#;

const EDGE_ZONES: &str = r#"# Atelier engine (EXPERIMENTAL, DECISIONS 0483): limits for its prefix. This
# file replaces edge.conf only while the engine is enabled. An empty key isn't
# limited, so each map picks which requests a zone counts.
#
# Code tries count per endpoint and only as POSTs: a pairing-code guesser
# doesn't spend the owner's wake tries, and viewing the wake page isn't a try.
map $request_method $engine_try {
    POST    $binary_remote_addr;
    default "";
}
# Everything else on those two (viewing the wake page) has a bucket of its
# own too, so a flood of other anonymous calls can't keep the owner from it.
map $request_method $engine_view {
    POST    "";
    default $binary_remote_addr;
}
# A request with a well-formed token is limited per token, so anonymous
# traffic from the owner's address can't starve a paired Manager; made-up
# tokens each get a bucket, so a per-address cap on all token requests bounds
# those. Anything else, a junk Authorization header included, is anonymous.
map $http_authorization $engine_anon {
    "~^Bearer aet_[0-9a-f]{64}$" "";
    default $binary_remote_addr;
}
map $http_authorization $engine_token {
    "~^Bearer aet_[0-9a-f]{64}$" $http_authorization;
    default "";
}
map $http_authorization $engine_token_ip {
    "~^Bearer aet_[0-9a-f]{64}$" $binary_remote_addr;
    default "";
}
# A zone can't change its key on reload: nginx refuses the whole reload and
# keeps the old config while `nginx -t` passes. Give a changed key a new zone
# name, and recreate or restart edge rather than trusting a reload.
limit_req_zone $engine_anon zone=engine_anon:1m rate=60r/m;
limit_req_zone $engine_token zone=engine_token:2m rate=120r/m;
limit_req_zone $engine_token_ip zone=engine_token_ip:1m rate=1200r/m;
limit_req_zone $engine_view zone=engine_view:1m rate=60r/m;
limit_req_zone $engine_try zone=engine_pair:1m rate=5r/m;
limit_req_zone $engine_try zone=engine_wake:1m rate=5r/m;
# Edge is the engine's only trusted peer: `app` must not steer it into the
# engine's paths with an internal redirect.
proxy_ignore_headers X-Accel-Redirect;

"#;

/// One proxied engine location's body (after its own `limit_req` lines).
const EDGE_PROXY: &str = r#"            client_max_body_size 4k;
            set $engine http://engine:8080;
            REWRITE
            proxy_pass $engine;
            proxy_http_version 1.0;
            proxy_set_header Host engine;
            proxy_set_header X-Real-IP $remote_addr;
            proxy_set_header Cookie "";
            proxy_set_header Connection "";
            proxy_buffering on;
            error_page 502 503 504 = @engine_down;
"#;

/// Exact `=` locations win over the `^~` prefix, and `^~` stops nginx from
/// trying any other location, so the snapshot rules never see these paths.
/// Every route is named: `/pair` and `/wake` exactly, the rest in one anchored
/// regex nested in the prefix, which answers 404 to anything else. Routes end
/// at `\z`, never `$`: `$uri` is decoded, and `$` also matches before a final
/// newline (`/pair%0A` once slipped past the exact location and its limit).
/// `proxy_set_header` in a location drops every inherited one, so the list is
/// the whole set the engine gets: no Cookie, no X-Forwarded-*.
fn edge_locations() -> String {
    let proxied = |rewrite: &str| {
        EDGE_PROXY
            .replace("REWRITE", rewrite)
            .lines()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
    };
    let exact = |name: &str| {
        format!(
            "    location = /_atelier/engine/v1/{name} {{\n\
             \x20       limit_req zone=engine_view burst=30 nodelay;\n\
             \x20       limit_req zone=engine_{name} burst=3 nodelay;\n\
             \x20       limit_req_status 429;\n{}    }}\n",
            proxied(&format!("rewrite ^ /v1/{name} break;"))
                .lines()
                .map(|l| format!("{}\n", &l[4..]))
                .collect::<String>()
        )
    };
    format!(
        "\n    location ^~ /_atelier/engine/ {{\n\
         \x20       location ~ \"^/_atelier/engine(/v1/({routes}))\\z\" {{\n\
         \x20           limit_req zone=engine_anon burst=30 nodelay;\n\
         \x20           limit_req zone=engine_token burst=60 nodelay;\n\
         \x20           limit_req zone=engine_token_ip burst=200 nodelay;\n\
         \x20           limit_req_status 429;\n{proxy}\
         \x20       }}\n\
         \x20       return 404;\n\
         \x20   }}\n{pair}{wake}\
         \x20   location @engine_down {{\n\
         \x20       default_type application/json;\n\
         \x20       add_header Cache-Control \"no-store\" always;\n\
         \x20       return 503 '{{\"error\":\"engine unavailable\"}}';\n\
         \x20   }}\n",
        routes = EDGE_ROUTES.join("|"),
        proxy = proxied("rewrite \"^/_atelier/engine(/v1/[0-9a-z/]+)\\z\" $1 break;"),
        pair = exact("pair"),
        wake = exact("wake"),
    )
}

fn insert_at(base: &str, anchor: &str, text: &str, before: bool) -> String {
    assert_eq!(
        base.matches(anchor).count(),
        1,
        "edge template anchor gone or repeated: {anchor:?}"
    );
    let with = if before {
        format!("{text}{anchor}")
    } else {
        format!("{anchor}{text}")
    };
    base.replacen(anchor, &with, 1)
}

/// [`EDGE_CONF_TEMPLATE`] with the engine added: derived, so it can't drift
/// from the shipped rule silently (each insertion asserts its anchor).
pub fn edge_conf() -> String {
    let s = insert_at(EDGE_CONF_TEMPLATE, "server {\n", EDGE_ZONES, true);
    insert_at(&s, "    absolute_redirect off;\n", &edge_locations(), false)
}

pub fn compose_path(stack: &Stack) -> PathBuf {
    stack.home.join(ENGINE_COMPOSE_FILE)
}

pub fn edge_path(stack: &Stack) -> PathBuf {
    stack.home.join(ENGINE_EDGE_FILE)
}

/// Whether the stack uses the engine: `.env` names the opt-in compose file.
pub fn is_enabled(stack: &Stack) -> bool {
    stack.read_env().get("COMPOSE_FILE").map(String::as_str) == Some(COMPOSE_FILE_VALUE)
}

/// Whether both opt-in files are exactly what this Manager writes.
pub fn files_current(stack: &Stack) -> bool {
    read(&compose_path(stack)).as_deref() == Some(ENGINE_COMPOSE_TEMPLATE)
        && read(&edge_path(stack)) == Some(edge_conf())
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// The checks both helpers share: an installed stack whose own files are the
/// shipped templates (doctor's "stack files match"), so the derived edge
/// config can't drop an owner's edits to `edge.conf`.
fn refuse_unless_shipped(stack: &Stack) -> Result<()> {
    if !stack.exists() {
        bail!(
            "{} has no installed stack — install it first",
            stack.home.display()
        );
    }
    if !stack.stack_files_current() {
        bail!(
            "compose.yaml / edge.conf differ from this Manager's templates (doctor: \
             \"Stack files match this manager\"). Run `atelier doctor --fix` first, or \
             put your changes aside: the engine's files are derived from the shipped ones."
        );
    }
    Ok(())
}

/// Turn the engine on (m7-1): write [`ENGINE_EDGE_FILE`] and
/// [`ENGINE_COMPOSE_FILE`], then name them in `.env`. Each file is written
/// atomically, and `.env` last, so `COMPOSE_FILE` never names a file that isn't
/// there. Idempotent: running it again rewrites the files from this Manager's
/// templates (after an update changed them). It starts nothing; the caller runs
/// `compose up -d`.
///
/// Refuses when the stack files aren't the shipped templates, when `.env`
/// already sets `COMPOSE_FILE` to something else, and when a
/// `compose.override.yaml` is present (`COMPOSE_FILE` would silently stop
/// loading it).
pub fn enable(stack: &Stack) -> Result<()> {
    refuse_unless_shipped(stack)?;
    let env = stack.read_env();
    if let Some(v) = env.get("COMPOSE_FILE").filter(|v| *v != COMPOSE_FILE_VALUE) {
        bail!(
            ".env already sets COMPOSE_FILE={v}; the engine needs \
             COMPOSE_FILE={COMPOSE_FILE_VALUE}. Remove yours first."
        );
    }
    for name in ["compose.override.yaml", "compose.override.yml"] {
        if stack.home.join(name).exists() {
            bail!(
                "{name} is present; with the engine, compose loads only the files \
                 COMPOSE_FILE names, so it would stop applying. Move it aside first."
            );
        }
    }
    let home = stack
        .home
        .canonicalize()
        .context("resolve the stack directory")?;
    let home = home
        .to_str()
        .filter(|h| !h.contains(':') && !h.contains(char::is_whitespace))
        .context("the stack directory's path has a ':' or a space, which compose can't mount")?
        .to_string();
    write_atomic(&edge_path(stack), &edge_conf(), 0o644)?;
    write_atomic(&compose_path(stack), ENGINE_COMPOSE_TEMPLATE, 0o644)?;
    let mut text = env_without_ours(stack);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(ENV_MARKER);
    for (k, v) in [
        ("COMPOSE_FILE", COMPOSE_FILE_VALUE.to_string()),
        ("COMPOSE_PATH_SEPARATOR", ":".to_string()),
        ("ATELIER_STACK_DIR", home),
        ("ATELIER_PROJECT", stack.project_name()),
    ] {
        text.push_str(&format!("{k}={v}\n"));
    }
    write_atomic(&stack.env_path(), &text, 0o600)
}

/// The comment [`enable`] puts above its `.env` lines.
const ENV_MARKER: &str =
    "# Atelier engine, EXPERIMENTAL (DECISIONS 0483): `atelier engine disable` removes these.\n";

/// `.env` as it is, minus the lines [`enable`] adds: the owner's own lines
/// stay byte for byte, in their order.
fn env_without_ours(stack: &Stack) -> String {
    let text = read(&stack.env_path()).unwrap_or_default();
    text.split_inclusive('\n')
        .filter(|line| {
            let l = line.trim();
            l != ENV_MARKER.trim()
                && !l
                    .split_once('=')
                    .is_some_and(|(k, _)| ENV_KEYS.contains(&k.trim()))
        })
        .collect()
}

/// Turn the engine off: take its keys out of `.env` first (so compose stops
/// merging it at once), then remove the two opt-in files. Touches nothing
/// else: `compose.yaml`, `edge.conf` and the engine's volumes stay. The caller
/// runs `compose up -d --remove-orphans`, which removes the engine container
/// and recreates edge on `edge.conf`.
pub fn disable(stack: &Stack) -> Result<()> {
    refuse_unless_shipped(stack)?;
    let env = stack.read_env();
    if let Some(v) = env.get("COMPOSE_FILE").filter(|v| *v != COMPOSE_FILE_VALUE) {
        bail!(".env sets COMPOSE_FILE={v}, which this Manager didn't write; leaving it alone");
    }
    let text = env_without_ours(stack);
    if read(&stack.env_path()).as_deref() != Some(text.as_str()) {
        write_atomic(&stack.env_path(), &text, 0o600)?;
    }
    for p in [compose_path(stack), edge_path(stack)] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", p.display())),
        }
    }
    Ok(())
}

/// Write `path` through a fresh temp file in the same directory, renamed over
/// it: a reader sees the old file or the new one, never half of either.
fn write_atomic(path: &Path, contents: &str, _mode: u32) -> Result<()> {
    let dir = path.parent().context("a file path has a directory")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("a file path has a name")?;
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        std::fs::write(&tmp, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(_mode))?;
        }
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{InstallOptions, COMPOSE_TEMPLATE};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempStack(Stack);
    impl TempStack {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "atelier-engine-optin-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let s = Stack::at(&dir);
            s.ensure_scaffold(&InstallOptions::default()).unwrap();
            Self(s)
        }
    }
    impl Drop for TempStack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.home);
        }
    }

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut all: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        all.sort();
        all
    }

    #[test]
    fn the_default_templates_know_nothing_of_the_engine() {
        assert!(!COMPOSE_TEMPLATE.contains("engine"));
        assert!(!COMPOSE_TEMPLATE.contains("docker.sock"));
        assert!(!EDGE_CONF_TEMPLATE.contains("_atelier/engine"));
        assert!(!EDGE_CONF_TEMPLATE.contains("engine"));
    }

    #[test]
    fn enable_writes_only_the_opt_in_files_and_env_keys() {
        let t = TempStack::new();
        let s = &t.0;
        let env_before = s.read_env();
        enable(s).unwrap();
        assert!(is_enabled(s) && files_current(s));
        assert_eq!(
            std::fs::read_to_string(s.compose_path()).unwrap(),
            COMPOSE_TEMPLATE,
            "compose.yaml byte-identical"
        );
        assert_eq!(
            std::fs::read_to_string(s.edge_conf_path()).unwrap(),
            EDGE_CONF_TEMPLATE,
            "edge.conf byte-identical"
        );
        let env = s.read_env();
        for (k, v) in &env_before {
            assert_eq!(env.get(k), Some(v), "{k} kept");
        }
        assert_eq!(env["COMPOSE_FILE"], COMPOSE_FILE_VALUE);
        assert_eq!(env["ATELIER_PROJECT"], s.project_name());
        assert!(Path::new(&env["ATELIER_STACK_DIR"]).is_absolute());
        let names: Vec<String> = snapshot(&s.home).into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&ENGINE_COMPOSE_FILE.to_string()));
        assert!(names.contains(&ENGINE_EDGE_FILE.to_string()));
        assert!(!names.iter().any(|n| n.contains(".tmp-")), "{names:?}");
        enable(s).unwrap();
        assert!(files_current(s), "idempotent");
    }

    #[test]
    fn disable_puts_the_stack_back_exactly() {
        let t = TempStack::new();
        let s = &t.0;
        let before = snapshot(&s.home);
        enable(s).unwrap();
        disable(s).unwrap();
        assert!(!is_enabled(s));
        assert_eq!(snapshot(&s.home), before);
        disable(s).unwrap();
        assert_eq!(snapshot(&s.home), before, "idempotent");
    }

    #[test]
    fn both_refuse_a_stack_that_isnt_the_shipped_templates() {
        let t = TempStack::new();
        let s = &t.0;
        std::fs::write(s.edge_conf_path(), "# edited by hand\n").unwrap();
        let e = enable(s).unwrap_err().to_string();
        assert!(e.contains("doctor"), "{e}");
        assert!(!compose_path(s).exists() && !edge_path(s).exists());
        assert!(disable(s).is_err());
        assert!(enable(&Stack::at(s.home.join("missing"))).is_err());
    }

    #[test]
    fn enable_refuses_a_foreign_compose_file_or_an_override() {
        let t = TempStack::new();
        let s = &t.0;
        std::fs::write(s.home.join("compose.override.yaml"), "services: {}\n").unwrap();
        assert!(enable(s).unwrap_err().to_string().contains("override"));
        std::fs::remove_file(s.home.join("compose.override.yaml")).unwrap();
        let mut text = std::fs::read_to_string(s.env_path()).unwrap();
        text.push_str("COMPOSE_FILE=mine.yaml\n");
        std::fs::write(s.env_path(), text).unwrap();
        assert!(enable(s).is_err());
        assert!(disable(s).is_err(), "not ours to remove");
        assert_eq!(s.read_env()["COMPOSE_FILE"], "mine.yaml");
        assert!(!compose_path(s).exists());
    }

    #[test]
    fn the_engine_publishes_no_port_and_app_never_joins_its_network() {
        let c = ENGINE_COMPOSE_TEMPLATE;
        assert!(!c.contains("ports:"), "floor 1");
        // `networks:` appears for edge, engine and the top level only.
        let services = c.split("\nvolumes:\n").next().unwrap();
        let app = services
            .split("  app:\n")
            .nth(1)
            .unwrap()
            .split("\n  edge:\n")
            .next()
            .unwrap();
        assert!(!app.contains("networks"), "E-05: app stays on default");
        assert!(!c.contains("  db:"), "db is untouched");
        assert!(c.contains("  edge:\n    networks:\n      - default\n      - engine\n"));
        assert!(c.contains("    networks:\n      - engine\n    # Bounded"));
        assert!(c.contains("  engine:\n    # No route out: the engine talks to Docker over the socket, not the network.\n    internal: true\n"));
        assert!(c.contains("      - engine-outbox:/opt/drupal/engine-outbox:ro\n"));
        assert_eq!(
            c.matches("engine-state").count(),
            2,
            "engine mount + volume"
        );
        assert_eq!(c.matches("docker.sock").count(), 2, "one mount line");
        assert!(c.contains(&format!(
            "    image: {ENGINE_IMAGE}\n    pull_policy: never\n"
        )));
        assert!(c.contains(&format!("      {EXPERIMENTAL_ENV}: \"1\"\n")));
        assert!(c.contains(&format!(
            "./{ENGINE_EDGE_FILE}:/etc/nginx/conf.d/default.conf:ro"
        )));
    }

    #[test]
    fn the_engine_edge_routes_only_named_paths_without_cookies() {
        let e = edge_conf();
        assert!(e.starts_with(&EDGE_CONF_TEMPLATE[..200]));
        assert_eq!(e.matches("proxy_set_header Cookie \"\";").count(), 3);
        assert_eq!(e.matches("set $engine http://engine:8080;").count(), 3);
        assert_eq!(e.matches("proxy_http_version 1.0;").count(), 3);
        assert!(!e.contains("(/v1/.*)$"));
        assert_eq!(
            e.matches("\\z\"").count(),
            2,
            "every route regex ends at \\z"
        );
        assert!(e.contains("        return 404;\n    }\n    location = /_atelier/engine/v1/pair {"));
        assert!(e.contains("    location = /_atelier/engine/v1/wake {\n"));
        assert!(e.contains("limit_req zone=engine_pair burst=3 nodelay;"));
        assert!(e.contains("limit_req zone=engine_wake burst=3 nodelay;"));
        assert!(e.contains("rewrite ^ /v1/pair break;") && e.contains("rewrite ^ /v1/wake break;"));
        assert!(e.find("location ^~ /_atelier/engine/").unwrap() < e.find("location / {").unwrap());
        assert!(e.find("limit_req_zone").unwrap() < e.find("server {").unwrap());
        // Balanced braces: a template slip would fail `nginx -t` on a tester's box.
        assert_eq!(e.matches('{').count(), e.matches('}').count());
        // Removing the engine's additions gives back the shipped rule.
        assert_eq!(
            e.replace(EDGE_ZONES, "").replace(&edge_locations(), ""),
            EDGE_CONF_TEMPLATE
        );
    }
}
