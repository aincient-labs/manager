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
//! | [`ENGINE_COMPOSE_FILE`] | adds the `engine` service (Docker socket, no published port, internal `engine` network), puts `edge` on that network, gives `app` the inbox (rw) and outbox (ro) volumes, and mounts [`ENGINE_EDGE_FILE`] over edge's `default.conf`; with trusted proxies, also publishes edge on loopback only ([`engine_compose`], m7-10) |
//! | [`ENGINE_EDGE_FILE`] | [`EDGE_CONF_TEMPLATE`] plus the engine's rate-limit zones, its `/_atelier/engine/` locations and resting mode ([`edge_conf`]) |
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
//! engine writes, app mounts read-only), `engine-state` (the engine only),
//! and for resting mode (M5) `engine-signal` (the engine writes its resting
//! markers, edge mounts it read-only) and `engine-activity` (edge logs console
//! requests, the engine reads the log's time). `app` mounts neither.

use crate::stack::{Stack, EDGE_CONF_TEMPLATE};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The engine API major this Manager speaks (m3-3, C-08). The engine's own
/// `API_MAJOR` (`aincient-engine`) is tested equal to it, so a major bump on
/// one side can't ship without the other.
pub const CLIENT_API_MAJOR: u32 = 1;

/// The opt-in compose file, merged over `compose.yaml` through `COMPOSE_FILE`.
pub const ENGINE_COMPOSE_FILE: &str = "compose.engine.yaml";
/// The edge config used instead of `edge.conf` while the engine is on.
pub const ENGINE_EDGE_FILE: &str = "edge.engine.conf";
/// The tag `engine/build.sh` gives a locally built engine image: the
/// developer path, used only by a Manager build that carries no released
/// image ([`released_engine_image`]). Never pulled, and never run by this
/// name: [`enable`]'s caller resolves it to the image ID once and pins that
/// (m7-4).
pub const ENGINE_IMAGE: &str = "atelier-engine:dev";
/// Where the Manager's release workflow publishes the engine image, built
/// from the same tag as the Manager (DECISIONS 0487). Only ever run by
/// digest ([`released_engine_image`]), never by a tag.
pub const ENGINE_IMAGE_REPO: &str = "ghcr.io/aincient-labs/atelier-engine";
/// The build-time environment variable carrying the released engine image's
/// digest (`sha256:<64 hex>`, of its multi-arch index), which the release
/// workflow sets for every Manager build (0487).
pub const ENGINE_DIGEST_ENV: &str = "ATELIER_ENGINE_DIGEST";
/// The `.env` key holding the pinned engine image, in one of two immutable
/// forms ([`is_pinned_image`]): the released image by digest,
/// `ghcr.io/aincient-labs/atelier-engine@sha256:<64 hex>` (0487), or, on a
/// developer build, a locally built image's content-addressed ID,
/// `sha256:<64 hex>` (a local build has no registry digest, and
/// `name@sha256:<id>` doesn't resolve for one; the bare ID does). Never a
/// tag: no pull, rebuild or retag can change what either form names.
/// [`ENGINE_COMPOSE_TEMPLATE`] runs exactly this; doctor flags a running
/// engine whose image differs ([`pinned_image`]).
pub const IMAGE_ENV: &str = "ATELIER_ENGINE_IMAGE";
/// The `.env` key listing the TLS proxies in front of edge whose
/// `X-Forwarded-For` edge believes (m7-4): IPs or CIDRs, separated by commas
/// or spaces. The owner's key: unset or empty trusts nobody, so the client is
/// the TCP peer. Edge then hands the engine that one address as `X-Real-IP`.
/// List only addresses no untrusted client can arrive as: a proxy on the
/// host's network reaching edge's published port arrives as the Docker
/// network's gateway, and so does every other connection to that port that
/// goes through docker-proxy (IPv6 clients, host processes). The m7-e3 run
/// found the port open over IPv6, so while this list is non-empty the engine's
/// compose file publishes edge on `127.0.0.1` only ([`engine_compose`], m7-10):
/// the only way in from outside is the proxy. Host processes can still claim
/// an address; that is the owner's machine.
pub const TRUSTED_PROXIES_ENV: &str = "ATELIER_TRUSTED_PROXIES";
/// The engine refuses to start unless this is `1` (m7-2). Only
/// [`ENGINE_COMPOSE_TEMPLATE`] sets it, so a copied compose file that drops the
/// line can't start an engine by accident.
pub const EXPERIMENTAL_ENV: &str = "ATELIER_ENGINE_EXPERIMENTAL";
/// The `.env` key for the idle timer (D-11, m5-5): minutes without a console
/// request before a woken site goes back to rest; `0` turns it off. Read by
/// the engine on each check, so a change needs no restart.
pub const IDLE_REST_ENV: &str = "ATELIER_IDLE_REST_MINUTES";
/// The idle timer when `.env` doesn't set it (the design's 30 minutes).
pub const IDLE_REST_DEFAULT_MINUTES: u64 = 30;

/// [`IDLE_REST_ENV`]'s value as minutes: unset or unreadable is the default
/// (a typo must not silently turn the timer off), capped at a week.
pub fn idle_rest_minutes(value: Option<&str>) -> u64 {
    value
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(IDLE_REST_DEFAULT_MINUTES, |m| m.min(7 * 24 * 60))
}

/// The `.env` value of `COMPOSE_FILE` while the engine is on.
pub const COMPOSE_FILE_VALUE: &str = "compose.yaml:compose.engine.yaml";
/// The `.env` keys [`enable`] writes and [`disable`] removes.
pub const ENV_KEYS: [&str; 5] = [
    "COMPOSE_FILE",
    "COMPOSE_PATH_SEPARATOR",
    "ATELIER_STACK_DIR",
    "ATELIER_PROJECT",
    IMAGE_ENV,
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
    "wake/status",
    "wake/code",
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
  db:
    # Resting mode stops db; a wake starts it again, and app waits for it to be
    # healthy (compose.yaml's depends_on). The shipped check runs every 10s, so
    # a wake sat ~10s on a database that was ready in under one. While it
    # starts, check every second; the check itself is unchanged.
    healthcheck:
      start_period: 60s
      start_interval: 1s
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
      # Resting mode: the engine's markers (read-only here) and the console
      # requests edge logs for the idle timer.
      - engine-signal:/run/atelier-engine:ro
      - engine-activity:/var/log/atelier-activity
  engine:
    # The image `atelier engine enable` pinned: the released image by digest
    # (enable pulls it) or, on a developer build, a local build's image ID.
    # Never a tag, so nothing changes what runs until enable runs again.
    image: ${ATELIER_ENGINE_IMAGE:?run atelier engine enable}
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
      - engine-signal:/signal
      - engine-activity:/activity
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
  engine-signal:
  engine-activity:
networks:
  default:
  engine:
    # No route out: the engine talks to Docker over the socket, not the network.
    internal: true
"#;

/// What [`engine_compose`] adds to edge while [`TRUSTED_PROXIES_ENV`] lists
/// a proxy (m7-10). `!override` replaces compose.yaml's `ports` list instead
/// of appending to it (Compose 2.24+; [`enable`] checks the merged result).
/// `HTTP_PORT` stays a port number: the Manager's URLs read it as one.
pub const EDGE_LOOPBACK_PORTS: &str = r#"    # ATELIER_TRUSTED_PROXIES is set: edge believes X-Forwarded-For from those
    # addresses, and every connection to the published port arrives as the
    # Docker gateway. So the port is published on loopback only, and the proxy
    # is the only way in from outside (m7-10). `!override` replaces the ports of
    # compose.yaml rather than adding to them.
    ports: !override
      - "127.0.0.1:${HTTP_PORT:-41221}:80"
"#;

/// The opt-in compose file for edge behind the `trusted` proxies:
/// [`ENGINE_COMPOSE_TEMPLATE`] as is with none, plus [`EDGE_LOOPBACK_PORTS`]
/// with any.
pub fn engine_compose(trusted: &[String]) -> String {
    if trusted.is_empty() {
        return ENGINE_COMPOSE_TEMPLATE.to_string();
    }
    insert_at(
        ENGINE_COMPOSE_TEMPLATE,
        "  edge:\n",
        EDGE_LOOPBACK_PORTS,
        false,
    )
}

/// Whether a published port's host address is loopback. An empty or
/// missing one means every interface.
fn is_loopback_host(ip: &str) -> bool {
    ip.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The ports `service` publishes beyond loopback, from
/// `docker compose config --format json` (`host_ip:published` each).
pub fn config_ports_beyond_loopback(config_json: &str, service: &str) -> Result<Vec<String>> {
    let v: serde_json::Value =
        serde_json::from_str(config_json).context("read `docker compose config` output")?;
    let svc = v
        .get("services")
        .and_then(|s| s.get(service))
        .with_context(|| format!("`docker compose config` has no {service} service"))?;
    Ok(svc
        .get("ports")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let ip = p.get("host_ip").and_then(|i| i.as_str()).unwrap_or("");
            let port = match p.get("published") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(n) => n.to_string(),
                None => String::new(),
            };
            (!is_loopback_host(ip))
                .then(|| format!("{}:{port}", if ip.is_empty() { "*" } else { ip }))
        })
        .collect())
}

/// The host bindings beyond loopback in a container's
/// `docker inspect --format '{{json .NetworkSettings.Ports}}'`
/// (`{"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"41221"}]}`), as `ip:port`.
pub fn bindings_beyond_loopback(ports_json: &str) -> Result<Vec<String>> {
    let v: serde_json::Value =
        serde_json::from_str(ports_json.trim()).context("read the container's port bindings")?;
    let mut out = Vec::new();
    for bindings in v.as_object().into_iter().flat_map(|m| m.values()) {
        for b in bindings.as_array().into_iter().flatten() {
            let ip = b.get("HostIp").and_then(|i| i.as_str()).unwrap_or("");
            let port = b.get("HostPort").and_then(|p| p.as_str()).unwrap_or("");
            if !is_loopback_host(ip) {
                out.push(format!("{}:{port}", if ip.is_empty() { "*" } else { ip }));
            }
        }
    }
    Ok(out)
}

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

# Resting mode (M5). The idle timer's signal (D-11): a console request Drupal
# answered 2xx, which only a signed-in user gets (anonymous ones get 403 or a
# redirect). The engine reads only the log's modification time.
log_format engine_activity "$msec";
map "$upstream_status $uri" $engine_activity {
    default 0;
    "~^2[0-9][0-9] /atelier(/|\z)" 1;
}
# The resting copy (C-07). Only /atelier offers the wake form, and not while
# locked down ($engine_wake is 0 then).
map "$engine_wake$uri" $engine_rest_page {
    default "RESTING_PAGE";
    "~^1/atelier(/|\z)" "WAKE_PAGE";
}

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
/// the whole set the engine gets: no Cookie, no X-Forwarded-*. Likewise
/// `add_header`: `@engine_down` has one, so it repeats the shipped edge's
/// security headers (the proxied locations have none and inherit them).
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
         \x20       add_header X-Content-Type-Options nosniff always;\n\
         \x20       add_header Referrer-Policy strict-origin-when-cross-origin always;\n\
         \x20       add_header X-Frame-Options SAMEORIGIN always;\n\
         \x20       add_header Permissions-Policy \"camera=(), microphone=(), geolocation=(), payment=()\" always;\n\
         \x20       add_header Strict-Transport-Security $hsts always;\n\
         \x20       return 503 '{{\"error\":\"engine unavailable\"}}';\n\
         \x20   }}\n",
        routes = EDGE_ROUTES.join("|"),
        proxy = proxied("rewrite \"^/_atelier/engine(/v1/[0-9a-z/]+)\\z\" $1 break;"),
        pair = exact("pair"),
        wake = exact("wake"),
    )
}

/// The shipped rule's snapshot switch: a session cookie, a Drupal-owned path
/// or a non-GET method bypasses the snapshot.
const FRZ_ROOT_SHIPPED: &str = r#"map "$frz_cookie$frz_path$frz_method" $frz_root {
    default /srv/private/frozen/current;
    "~1" /nonexistent/frozen-bypass;
}
"#;

/// The same switch with resting mode in front (m5-2): while resting, a
/// session cookie alone no longer bypasses, so a signed-in reader still gets
/// the snapshot's pages; Drupal-owned paths and non-GET methods still do, and
/// then meet the resting copy. Awake, it is the shipped rule.
const FRZ_ROOT_ENGINE: &str = r#"map "$engine_resting$frz_cookie$frz_path$frz_method" $frz_root {
    default /srv/private/frozen/current;
    # Resting: a Drupal path or method bypasses (to the resting copy).
    "~^1.(1|.1)" /nonexistent/frozen-bypass;
    # Resting: a cookie alone reads the snapshot.
    "~^1" /srv/private/frozen/current;
    # Awake: the shipped rule.
    "~1" /nonexistent/frozen-bypass;
}
"#;

/// Server-wide, before every location: whether the site rests, from the
/// engine's markers in `engine-signal`, which edge mounts read-only and `app`
/// not at all. Two `stat`s per request; no request header, cookie or path
/// can set either variable. Then the access logs: the image's own, and the
/// idle timer's.
const EDGE_RESTING_SERVER: &str = r#"
    # Resting mode (Atelier engine, M5): the engine's markers decide, never a
    # request. `locked` (lockdown, D-09) is resting with the wake form off.
    set $engine_resting 0;
    set $engine_wake 0;
    if (-f /run/atelier-engine/resting) {
        set $engine_resting 1;
        set $engine_wake 1;
    }
    if (-f /run/atelier-engine/locked) {
        set $engine_resting 1;
        set $engine_wake 0;
    }
    access_log /var/log/nginx/access.log main;
    access_log /var/log/atelier-activity/console.log engine_activity if=$engine_activity;
"#;

/// In `@miss`, before Drupal is asked: a resting site has no Drupal, so the
/// request gets the resting copy instead of the "starting" page.
const EDGE_MISS_RESTING: &str = r#"        # Resting (Atelier engine): no Drupal to ask; the resting copy (C-07).
        error_page 418 = @engine_resting;
        if ($engine_resting) {
            return 418;
        }
"#;

/// The resting copy itself (C-07): a calm page, 503 so nothing caches or
/// indexes it as the page; the wake form only on `/atelier`. Its own
/// `add_header`s drop the server-level security headers, so it repeats them,
/// with the stricter `X-Frame-Options DENY` in place of `SAMEORIGIN` (two
/// different values would cancel each other out in a browser).
const EDGE_RESTING_LOCATION: &str = r#"
    location @engine_resting {
        default_type text/html;
        charset utf-8;
        add_header Cache-Control "no-store" always;
        add_header Retry-After 3600 always;
        add_header X-Content-Type-Options nosniff always;
        add_header Referrer-Policy strict-origin-when-cross-origin always;
        add_header X-Frame-Options DENY always;
        add_header Permissions-Policy "camera=(), microphone=(), geolocation=(), payment=()" always;
        add_header Strict-Transport-Security $hsts always;
        add_header Content-Security-Policy "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'" always;
        return 503 $engine_rest_page;
    }
"#;

const RESTING_STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;margin:15vh auto;max-width:32rem;padding:0 1.5rem;color:#222;background:#fff}h1{font-size:1.4rem}input,button{font:inherit;padding:.4rem .6rem}@media (prefers-color-scheme:dark){body{color:#eee;background:#161616}a{color:#9cf}}";

/// The resting page anyone gets for what needs Drupal. Single-quoted
/// attributes and no `$`: it sits inside a double-quoted nginx string whose
/// `$` would read as a variable.
pub fn resting_page() -> String {
    resting_html(
        "<p>Sign-in and forms are paused while it rests. Its pages are still here to \
         read.</p>",
    )
}

/// The resting page on `/atelier` (C-07): the same words, and the wake form,
/// which posts to the engine's `wake` (m5-3).
pub fn wake_page() -> String {
    resting_html(
        "<p>Sign-in and forms are paused while it rests. If you work on this site, wake \
         it with its wake code. Waking starts the site; it doesn't sign you in.</p>\
         <form method='post' action='/_atelier/engine/v1/wake'><label for='code'>Wake \
         code</label> <input id='code' name='code' autocomplete='off' required> \
         <button>Wake</button></form><p>A Manager paired with this site can wake it \
         too.</p>",
    )
}

fn resting_html(words: &str) -> String {
    format!(
        "<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' \
         content='width=device-width,initial-scale=1'><title>This site is resting</title>\
         <style>{RESTING_STYLE}</style></head><body><main><h1>This site is resting</h1>\
         {words}<p><a href='/'>Go to the home page</a></p></main></body></html>"
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

/// The client's address behind a TLS proxy (m7-4), server-wide so every
/// limit and the `X-Real-IP` the engine gets already use it. nginx replaces
/// `$remote_addr` with the right-most `X-Forwarded-For` entry that isn't a
/// listed proxy, and only when the connection itself comes from one; from
/// anyone else the header is ignored. No list, no `set_real_ip_from`: the
/// client is the TCP peer, whatever it sends.
fn edge_real_ip(trusted: &[String]) -> String {
    let mut s = String::from(
        "\n    # The client's address (m7-4): X-Forwarded-For is believed only from the\n\
         \x20   # proxies ATELIER_TRUSTED_PROXIES in .env lists; then `atelier engine enable`.\n",
    );
    if trusted.is_empty() {
        s.push_str("    # None listed: the client is the TCP peer.\n");
        return s;
    }
    for t in trusted {
        s.push_str(&format!("    set_real_ip_from {t};\n"));
    }
    s.push_str("    real_ip_header X-Forwarded-For;\n    real_ip_recursive on;\n");
    s
}

/// [`EDGE_CONF_TEMPLATE`] with the engine added, for edge behind the
/// `trusted` proxies ([`trusted_proxies`]): derived, so it can't drift from
/// the shipped rule silently (each insertion asserts its anchor).
pub fn edge_conf(trusted: &[String]) -> String {
    let s = insert_at(EDGE_CONF_TEMPLATE, "server {\n", &edge_zones(), true);
    let s = replace_once(&s, FRZ_ROOT_SHIPPED, FRZ_ROOT_ENGINE);
    let s = insert_at(
        &s,
        "        # Live or bypassed: Drupal. A variable target is resolved per request.\n",
        EDGE_MISS_RESTING,
        true,
    );
    let tail = format!(
        "{}{EDGE_RESTING_SERVER}{}{EDGE_RESTING_LOCATION}",
        edge_real_ip(trusted),
        edge_locations()
    );
    insert_at(&s, "    absolute_redirect off;\n", &tail, false)
}

/// [`EDGE_ZONES`] with the two resting pages in place.
fn edge_zones() -> String {
    EDGE_ZONES
        .replace("RESTING_PAGE", &resting_page())
        .replace("WAKE_PAGE", &wake_page())
}

fn replace_once(base: &str, from: &str, to: &str) -> String {
    assert_eq!(
        base.matches(from).count(),
        1,
        "edge template text gone or repeated: {from:?}"
    );
    base.replacen(from, to, 1)
}

/// Parse [`TRUSTED_PROXIES_ENV`]: IPs or CIDRs, separated by commas or
/// spaces, each written back canonically (so nothing but an address reaches
/// nginx's config). Refuses anything else, a prefix past the address's
/// width, and a `/0`: "trust everyone" would let any client choose its own
/// address, so it is never accepted.
pub fn parse_trusted_proxies(value: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        let (ip, prefix) = match item.split_once('/') {
            Some((ip, p)) => (ip, Some(p)),
            None => (item, None),
        };
        let ip: std::net::IpAddr = ip.parse().with_context(|| {
            format!("{TRUSTED_PROXIES_ENV}: {item:?} is not an IP address or CIDR")
        })?;
        let width = if ip.is_ipv4() { 32 } else { 128 };
        let entry = match prefix {
            None => ip.to_string(),
            Some(p) => {
                let n: u8 = p
                    .parse()
                    .ok()
                    .filter(|n| p.bytes().all(|b| b.is_ascii_digit()) && *n <= width)
                    .with_context(|| {
                        format!("{TRUSTED_PROXIES_ENV}: {item:?} has a bad prefix length")
                    })?;
                if n == 0 {
                    bail!(
                        "{TRUSTED_PROXIES_ENV}: {item:?} trusts every address, so any client \
                         could pick its own; list your proxy's address instead"
                    );
                }
                format!("{ip}/{n}")
            }
        };
        if ip.is_unspecified() {
            bail!("{TRUSTED_PROXIES_ENV}: {item:?} is not a proxy's address");
        }
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// The stack's trusted proxies, from `.env`; none when the key is unset.
pub fn trusted_proxies(stack: &Stack) -> Result<Vec<String>> {
    parse_trusted_proxies(
        stack
            .read_env()
            .get(TRUSTED_PROXIES_ENV)
            .map(String::as_str)
            .unwrap_or(""),
    )
}

/// Whether `id` is a full image ID (or digest): `sha256:` and 64 lower-case
/// hex digits.
pub fn is_image_id(id: &str) -> bool {
    id.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Whether `image` is a pin [`enable`] accepts (0487): the released
/// [`ENGINE_IMAGE_REPO`] by digest (`<repo>@sha256:<64 hex>`), or a bare
/// image ID (the developer path). Never a tag, and no other repository: the
/// engine holds the Docker socket, so only the image this project publishes,
/// or one the owner built, may run as it.
pub fn is_pinned_image(image: &str) -> bool {
    is_image_id(image)
        || image
            .strip_prefix(ENGINE_IMAGE_REPO)
            .and_then(|r| r.strip_prefix('@'))
            .is_some_and(is_image_id)
}

/// The released engine image this Manager build pins (0487):
/// `ghcr.io/aincient-labs/atelier-engine@sha256:…`, from the digest the
/// release workflow passed at compile time as [`ENGINE_DIGEST_ENV`]. `None`
/// for a developer build (unset, empty or malformed), which falls back to a
/// local [`ENGINE_IMAGE`] build.
///
/// No `build.rs` is needed for a change of the variable to rebuild this
/// crate: rustc records every `option_env!` it reads in the dep-info file
/// (`# env-dep:ATELIER_ENGINE_DIGEST`), and Cargo rebuilds when a recorded
/// variable changes (checked with Cargo 1.88: set, changed and unset each
/// recompile).
pub fn released_engine_image() -> Option<String> {
    released_image_from(option_env!("ATELIER_ENGINE_DIGEST"))
}

/// [`released_engine_image`]'s parsing, apart from the environment.
fn released_image_from(digest: Option<&str>) -> Option<String> {
    let d = digest?.trim();
    is_image_id(d).then(|| format!("{ENGINE_IMAGE_REPO}@{d}"))
}

/// The image `.env` pins the engine to, if it is a well-formed pin
/// ([`is_pinned_image`]).
pub fn pinned_image(stack: &Stack) -> Option<String> {
    stack
        .read_env()
        .get(IMAGE_ENV)
        .filter(|v| is_pinned_image(v))
        .cloned()
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

/// Whether both opt-in files are exactly what this Manager writes for the
/// stack's `.env` (its trusted proxies), and `.env` pins an image
/// ([`is_pinned_image`]: by digest or by ID). False
/// after a Manager update changed a template, or after the owner changed
/// [`TRUSTED_PROXIES_ENV`]: `enable` again rewrites them (doctor says so).
pub fn files_current(stack: &Stack) -> bool {
    let Ok(trusted) = trusted_proxies(stack) else {
        return false;
    };
    read(&compose_path(stack)) == Some(engine_compose(&trusted))
        && read(&edge_path(stack)) == Some(edge_conf(&trusted))
        && pinned_image(stack).is_some()
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
/// [`ENGINE_COMPOSE_FILE`], then name them in `.env` with the engine pinned
/// to `image` ([`is_pinned_image`]: the released image by digest, 0487, or
/// on a developer build the ID the caller resolved from [`ENGINE_IMAGE`],
/// m7-4; the caller has made sure it is on this computer). Each file is written
/// atomically, and `.env` last, so `COMPOSE_FILE` never names a file that isn't
/// there. Idempotent: running it again rewrites the files from this Manager's
/// templates (after an update changed them). It starts nothing; the caller runs
/// `compose up -d`.
///
/// Refuses when the stack files aren't the shipped templates, when `.env`
/// already sets `COMPOSE_FILE` to something else, and when a
/// `compose.override.yaml` is present (`COMPOSE_FILE` would silently stop
/// loading it).
///
/// With trusted proxies it also refuses unless edge would be published on
/// loopback only (m7-10): the candidate compose file is merged by the real
/// Compose first, through `config` ([`ComposeConfig`]), and every port edge
/// publishes must be on a loopback address. A Compose too old for
/// `!override` (it appends, or fails) is refused here, before any file is
/// written.
pub fn enable(stack: &Stack, image: &str, config: ComposeConfig) -> Result<()> {
    refuse_unless_shipped(stack)?;
    if !is_pinned_image(image) {
        bail!(
            "the engine image must be pinned as {ENGINE_IMAGE_REPO}@sha256:<64 hex> or by a \
             local image's full ID (sha256:<64 hex>), never a tag: not {image:?}"
        );
    }
    let trusted = trusted_proxies(stack)?;
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
    let compose_text = engine_compose(&trusted);
    if !trusted.is_empty() {
        refuse_unless_loopback_only(stack, &compose_text, &home, image, config)?;
    }
    write_atomic(&edge_path(stack), &edge_conf(&trusted), 0o644)?;
    write_atomic(&compose_path(stack), &compose_text, 0o644)?;
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
        (IMAGE_ENV, image.to_string()),
    ] {
        text.push_str(&format!("{k}={v}\n"));
    }
    write_atomic(&stack.env_path(), &text, 0o600)
}

/// Runs `docker compose -f <files…> config --format json` in the stack
/// directory with these extra environment variables, and returns its stdout
/// ([`crate::engine_admin::compose_config_json`]; a fake in tests).
pub type ComposeConfig<'a> = &'a dyn Fn(&[PathBuf], &[(&str, String)]) -> Result<String>;

/// [`enable`]'s m7-10 refusal: merge compose.yaml with `compose_text` (from a
/// temporary file next to it, removed after) and refuse unless edge
/// publishes on loopback only.
fn refuse_unless_loopback_only(
    stack: &Stack,
    compose_text: &str,
    home: &str,
    image: &str,
    config: ComposeConfig,
) -> Result<()> {
    let candidate = stack.home.join(format!(
        ".{ENGINE_COMPOSE_FILE}.check-{}",
        std::process::id()
    ));
    write_atomic(&candidate, compose_text, 0o644)?;
    let merged = config(
        &[stack.compose_path(), candidate.clone()],
        &[
            (IMAGE_ENV, image.to_string()),
            ("ATELIER_STACK_DIR", home.to_string()),
            ("ATELIER_PROJECT", stack.project_name()),
        ],
    );
    let _ = std::fs::remove_file(&candidate);
    let refuse = |why: String| {
        anyhow::anyhow!(
            "{TRUSTED_PROXIES_ENV} is set, so edge must be published on loopback only              (127.0.0.1) with the TLS proxy as the only way in, but {why}. Nothing was              written. Docker Compose 2.24 or newer is needed (`docker compose version`);              or empty {TRUSTED_PROXIES_ENV} in .env."
        )
    };
    let merged = merged.map_err(|e| refuse(format!("`docker compose config` failed: {e:#}")))?;
    let beyond =
        config_ports_beyond_loopback(&merged, "edge").map_err(|e| refuse(format!("{e:#}")))?;
    if !beyond.is_empty() {
        return Err(refuse(format!(
            "Compose would also publish it on {}",
            beyond.join(", ")
        )));
    }
    Ok(())
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

    const ID: &str = "sha256:700e4087b79a9f81777900a9862f395055d6f7b8cd1ca1a94eabafc51bb17529";

    /// `docker compose config --format json`'s edge ports for these files, as
    /// Compose 2.24+ merges them: the engine file's `ports: !override`
    /// replaces compose.yaml's list, anything else leaves it.
    fn fake(files: &[PathBuf], env: &[(&str, String)]) -> Result<String> {
        assert_eq!(files.len(), 2);
        assert_eq!(std::fs::read_to_string(&files[0])?, COMPOSE_TEMPLATE);
        assert!(env.iter().any(|(k, v)| *k == IMAGE_ENV && is_image_id(v)));
        let engine = std::fs::read_to_string(&files[1])?;
        let ip = if engine.contains("    ports: !override\n      - \"127.0.0.1:") {
            r#""host_ip":"127.0.0.1","#
        } else {
            ""
        };
        Ok(format!(
            r#"{{"services":{{"edge":{{"ports":[{{"mode":"ingress",{ip}"target":80,"published":"41221","protocol":"tcp"}}]}}}}}}"#
        ))
    }

    /// Compose before 2.24 appends the engine file's list to compose.yaml's.
    fn appending(_: &[PathBuf], _: &[(&str, String)]) -> Result<String> {
        Ok(r#"{"services":{"edge":{"ports":[{"target":80,"published":"41221"},{"host_ip":"127.0.0.1","target":80,"published":"41221"}]}}}"#.to_string())
    }

    fn set_proxies(s: &Stack, value: &str) {
        let mut text = std::fs::read_to_string(s.env_path()).unwrap();
        text.push_str(&format!("{TRUSTED_PROXIES_ENV}={value}\n"));
        std::fs::write(s.env_path(), text).unwrap();
    }

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
        enable(s, ID, &fake).unwrap();
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
        enable(s, ID, &fake).unwrap();
        assert!(files_current(s), "idempotent");
    }

    #[test]
    fn disable_puts_the_stack_back_exactly() {
        let t = TempStack::new();
        let s = &t.0;
        let before = snapshot(&s.home);
        enable(s, ID, &fake).unwrap();
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
        let e = enable(s, ID, &fake).unwrap_err().to_string();
        assert!(e.contains("doctor"), "{e}");
        assert!(!compose_path(s).exists() && !edge_path(s).exists());
        assert!(disable(s).is_err());
        assert!(enable(&Stack::at(s.home.join("missing")), ID, &fake).is_err());
    }

    #[test]
    fn enable_refuses_a_foreign_compose_file_or_an_override() {
        let t = TempStack::new();
        let s = &t.0;
        std::fs::write(s.home.join("compose.override.yaml"), "services: {}\n").unwrap();
        assert!(enable(s, ID, &fake)
            .unwrap_err()
            .to_string()
            .contains("override"));
        std::fs::remove_file(s.home.join("compose.override.yaml")).unwrap();
        let mut text = std::fs::read_to_string(s.env_path()).unwrap();
        text.push_str("COMPOSE_FILE=mine.yaml\n");
        std::fs::write(s.env_path(), text).unwrap();
        assert!(enable(s, ID, &fake).is_err());
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
        // db gains only a faster check while it starts (a wake, C-03): no
        // network, volume, port or anything else.
        let db = services
            .split("  db:\n")
            .nth(1)
            .unwrap()
            .split("\n  app:\n")
            .next()
            .unwrap();
        let db: Vec<_> = db
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert_eq!(
            db,
            [
                "    healthcheck:",
                "      start_period: 60s",
                "      start_interval: 1s"
            ]
        );
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
            "    image: ${{{IMAGE_ENV}:?run atelier engine enable}}\n    pull_policy: never\n"
        )));
        assert!(
            !c.contains(&format!("image: {ENGINE_IMAGE}")),
            "never run by its mutable tag"
        );
        assert!(c.contains(&format!("      {EXPERIMENTAL_ENV}: \"1\"\n")));
        assert!(c.contains(&format!(
            "./{ENGINE_EDGE_FILE}:/etc/nginx/conf.d/default.conf:ro"
        )));
    }

    #[test]
    fn the_engine_edge_locations_keep_the_security_headers() {
        // A location with its own add_header drops the server-level ones: the
        // shipped rule repeats them in `/` and `@updating`, the engine's
        // `@engine_down` and `@engine_resting` must too.
        let e = edge_conf(&[]);
        for header in [
            "add_header X-Content-Type-Options nosniff always;",
            "add_header Referrer-Policy strict-origin-when-cross-origin always;",
            "add_header Permissions-Policy \"camera=(), microphone=(), geolocation=(), payment=()\" always;",
            "add_header Strict-Transport-Security $hsts always;",
        ] {
            assert_eq!(e.matches(header).count(), 5, "{header}");
        }
        assert_eq!(
            e.matches("add_header X-Frame-Options SAMEORIGIN always;")
                .count(),
            4
        );
        let resting = &e[e.find("location @engine_resting").unwrap()..];
        let resting = &resting[..resting.find('}').unwrap()];
        assert!(resting.contains("add_header X-Frame-Options DENY always;"));
        assert!(!resting.contains("SAMEORIGIN"));
        let down = &e[e.find("location @engine_down").unwrap()..];
        let down = &down[..down.find('}').unwrap()];
        assert!(down.contains("add_header Strict-Transport-Security $hsts always;"));
    }

    #[test]
    fn the_engine_edge_routes_only_named_paths_without_cookies() {
        let e = edge_conf(&[]);
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
            e.replace(&edge_zones(), "")
                .replace(FRZ_ROOT_ENGINE, FRZ_ROOT_SHIPPED)
                .replace(EDGE_MISS_RESTING, "")
                .replace(EDGE_RESTING_SERVER, "")
                .replace(EDGE_RESTING_LOCATION, "")
                .replace(&edge_real_ip(&[]), "")
                .replace(&edge_locations(), ""),
            EDGE_CONF_TEMPLATE
        );
    }

    /// m5-2: edge decides "resting" from the engine's marker files only; no
    /// request field feeds the variables, and every Drupal-bound request
    /// meets the resting copy, with the wake form on `/atelier` alone.
    #[test]
    fn resting_mode_is_the_engines_markers_and_never_a_request() {
        let e = edge_conf(&[]);
        let server = e.split("server {\n").nth(1).unwrap();
        // Set only from the markers, server-wide, before any location.
        assert_eq!(e.matches("set $engine_resting ").count(), 3);
        assert_eq!(e.matches("set $engine_wake ").count(), 3);
        assert!(server.contains("if (-f /run/atelier-engine/resting) {"));
        assert!(server.contains("if (-f /run/atelier-engine/locked) {\n        set $engine_resting 1;\n        set $engine_wake 0;"));
        assert!(server.find("set $engine_resting 0;").unwrap() < server.find("location ").unwrap());
        for v in ["$engine_resting", "$engine_wake"] {
            for line in e.lines().filter(|l| l.starts_with("map ") && l.contains(v)) {
                assert!(
                    !line.contains("$http_")
                        && !line.contains("$cookie_")
                        && !line.contains("$arg_"),
                    "{line}"
                );
            }
        }
        // @miss: the resting copy before Drupal is asked.
        let miss = e.split("location @miss {").nth(1).unwrap();
        assert!(miss.find("return 418;").unwrap() < miss.find("set $app http://app:80;").unwrap());
        assert!(miss.contains("error_page 418 = @engine_resting;"));
        assert!(e.contains("location @engine_resting {"));
        assert!(e.contains("return 503 $engine_rest_page;"));
        // The snapshot switch: resting lets a cookie read the snapshot, a
        // Drupal path or method bypasses (to the resting copy), awake is the
        // shipped rule; the shipped map itself is gone.
        assert!(!e.contains(FRZ_ROOT_SHIPPED));
        assert!(e.contains("map \"$engine_resting$frz_cookie$frz_path$frz_method\" $frz_root {"));
        // The pages: plain, no external asset, nothing nginx would read as
        // a variable or a quote end; the form only in the wake page.
        for page in [resting_page(), wake_page()] {
            assert!(!page.contains('$') && !page.contains('"'), "{page}");
            assert!(!page.contains("http") && !page.contains("<script"));
            assert!(
                page.contains("This site is resting")
                    && page.contains("Sign-in and forms are paused")
            );
        }
        assert!(!resting_page().contains("<form"));
        assert!(wake_page().contains("<form method='post' action='/_atelier/engine/v1/wake'>"));
        assert!(
            e.contains("\"~^1/atelier(/|\\z)\" \"<!doctype"),
            "wake form only on /atelier, only when wake is on"
        );
        // The idle timer's log: console requests Drupal answered 2xx only,
        // beside the image's own access log.
        assert!(server.contains("access_log /var/log/nginx/access.log main;"));
        assert!(server.contains(
            "access_log /var/log/atelier-activity/console.log engine_activity if=$engine_activity;"
        ));
        assert!(e.contains("\"~^2[0-9][0-9] /atelier(/|\\z)\" 1;"));
        // The shipped template knows none of it.
        assert!(!EDGE_CONF_TEMPLATE.contains("resting"));
    }

    #[test]
    fn the_resting_volumes_reach_edge_read_only_and_never_app() {
        let c = ENGINE_COMPOSE_TEMPLATE;
        let services = c.split("\nvolumes:\n").next().unwrap();
        let app = services
            .split("  app:\n")
            .nth(1)
            .unwrap()
            .split("\n  edge:\n")
            .next()
            .unwrap();
        assert!(!app.contains("engine-signal") && !app.contains("engine-activity"));
        let edge = services
            .split("\n  edge:\n")
            .nth(1)
            .unwrap()
            .split("\n  engine:\n")
            .next()
            .unwrap();
        let edge = format!("{edge}\n");
        assert!(edge.contains("      - engine-signal:/run/atelier-engine:ro\n"));
        assert!(edge.contains("      - engine-activity:/var/log/atelier-activity\n"));
        let engine = services.split("\n  engine:\n").nth(1).unwrap();
        assert!(engine.contains("      - engine-signal:/signal\n"));
        assert!(engine.contains("      - engine-activity:/activity\n"));
        assert!(c.contains("\n  engine-signal:\n  engine-activity:\n"));
    }

    #[test]
    fn the_idle_timer_reads_minutes_and_falls_back_to_the_default() {
        assert_eq!(idle_rest_minutes(None), IDLE_REST_DEFAULT_MINUTES);
        assert_eq!(idle_rest_minutes(Some("0")), 0);
        assert_eq!(idle_rest_minutes(Some(" 45 ")), 45);
        assert_eq!(idle_rest_minutes(Some("thirty")), IDLE_REST_DEFAULT_MINUTES);
        assert_eq!(idle_rest_minutes(Some("-5")), IDLE_REST_DEFAULT_MINUTES);
        assert_eq!(idle_rest_minutes(Some("999999")), 7 * 24 * 60);
    }

    #[test]
    fn enable_pins_the_image_by_id_and_refuses_a_tag() {
        let t = TempStack::new();
        let s = &t.0;
        for bad in [
            ENGINE_IMAGE,
            "atelier-engine@sha256:700e4087b79a9f81777900a9862f395055d6f7b8cd1ca1a94eabafc51bb17529",
            "sha256:700E4087B79A9F81777900A9862F395055D6F7B8CD1CA1A94EABAFC51BB17529",
            "sha256:700e4087",
            "",
            "ghcr.io/aincient-labs/atelier-engine:0.13.0",
            "ghcr.io/aincient-labs/atelier-engine:0.13.0@sha256:700e4087b79a9f81777900a9862f395055d6f7b8cd1ca1a94eabafc51bb17529",
        ] {
            assert!(enable(s, bad, &fake).is_err(), "{bad:?}");
            assert!(!compose_path(s).exists(), "nothing written for {bad:?}");
        }
        enable(s, ID, &fake).unwrap();
        assert_eq!(pinned_image(s).as_deref(), Some(ID));
        assert_eq!(s.read_env()[IMAGE_ENV], ID);
        // A pin edited into a tag no longer counts as current.
        let text = std::fs::read_to_string(s.env_path())
            .unwrap()
            .replace(ID, ENGINE_IMAGE);
        std::fs::write(s.env_path(), text).unwrap();
        assert!(pinned_image(s).is_none() && !files_current(s));
        disable(s).unwrap();
        assert!(
            !s.read_env().contains_key(IMAGE_ENV),
            "disable takes the pin out"
        );
    }

    /// 0487: a released Manager pins the published image by digest, and that
    /// counts as current exactly like a local build's ID.
    #[test]
    fn enable_pins_the_released_image_by_digest() {
        let t = TempStack::new();
        let s = &t.0;
        let pin = format!("{ENGINE_IMAGE_REPO}@{ID}");
        enable(s, &pin, &fake).unwrap();
        assert_eq!(s.read_env()[IMAGE_ENV], pin);
        assert_eq!(pinned_image(s).as_deref(), Some(pin.as_str()));
        assert!(files_current(s));
        // Edited into the version tag, the pin no longer counts.
        let text = std::fs::read_to_string(s.env_path())
            .unwrap()
            .replace(&pin, &format!("{ENGINE_IMAGE_REPO}:0.13.0"));
        std::fs::write(s.env_path(), text).unwrap();
        assert!(pinned_image(s).is_none() && !files_current(s));
    }

    #[test]
    fn a_pin_is_the_released_repo_by_digest_or_a_bare_id_never_a_tag() {
        let hex = &ID["sha256:".len()..];
        assert!(is_pinned_image(ID));
        assert!(is_pinned_image(&format!(
            "{ENGINE_IMAGE_REPO}@sha256:{hex}"
        )));
        for bad in [
            format!("{ENGINE_IMAGE_REPO}:0.13.0"),
            format!("{ENGINE_IMAGE_REPO}:latest"),
            ENGINE_IMAGE_REPO.to_string(),
            format!("{ENGINE_IMAGE_REPO}@sha256:{}", &hex[..12]),
            format!("{ENGINE_IMAGE_REPO}@sha256:{}", hex.to_uppercase()),
            format!("{ENGINE_IMAGE_REPO}@sha512:{hex}"),
            format!("{ENGINE_IMAGE_REPO}:0.13.0@sha256:{hex}"),
            format!("{ENGINE_IMAGE_REPO}-fork@sha256:{hex}"),
            format!("ghcr.io/someone-else/atelier-engine@sha256:{hex}"),
            format!("docker.io/library/alpine@sha256:{hex}"),
            format!("atelier-engine@sha256:{hex}"),
            format!(" {ID}"),
            ENGINE_IMAGE.to_string(),
            String::new(),
        ] {
            assert!(!is_pinned_image(&bad), "{bad:?}");
        }
    }

    #[test]
    fn the_released_image_comes_only_from_a_full_digest() {
        let hex = &ID["sha256:".len()..];
        assert_eq!(
            released_image_from(Some(ID)).as_deref(),
            Some(format!("ghcr.io/aincient-labs/atelier-engine@sha256:{hex}").as_str())
        );
        assert_eq!(
            released_image_from(Some(&format!("{ID}\n"))).as_deref(),
            Some(format!("{ENGINE_IMAGE_REPO}@{ID}").as_str()),
            "a trailing newline from the workflow is trimmed"
        );
        for dev in [
            None,
            Some(""),
            Some("sha256:700e4087"),
            Some(hex),
            Some("0.13.0"),
            Some("ghcr.io/aincient-labs/atelier-engine@sha256:700e4087b79a9f81777900a9862f395055d6f7b8cd1ca1a94eabafc51bb17529"),
        ] {
            assert_eq!(released_image_from(dev), None, "{dev:?}");
        }
        // Whatever this test binary was built with, it is one of the two.
        assert!(released_engine_image().is_none_or(|r| is_pinned_image(&r) && !is_image_id(&r)));
    }

    #[test]
    fn trusted_proxies_parse_strictly_and_never_trust_everyone() {
        assert_eq!(parse_trusted_proxies("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_trusted_proxies(" 172.18.0.1, 10.0.0.0/8  2001:db8::/32,172.18.0.1").unwrap(),
            ["172.18.0.1", "10.0.0.0/8", "2001:db8::/32"]
        );
        for bad in [
            "0.0.0.0/0",
            "::/0",
            "10.0.0.0/0",
            "0.0.0.0",
            "10.0.0.0/33",
            "10.0.0.0/+8",
            "10.0.0.0/",
            "edge",
            "10.0.0.1;real_ip_header X-Evil",
            "10.0.0.1}",
            "all",
        ] {
            assert!(parse_trusted_proxies(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn edge_believes_forwarded_addresses_only_from_listed_proxies() {
        let none = edge_conf(&[]);
        assert!(
            !none.contains("set_real_ip_from"),
            "empty list trusts nobody"
        );
        assert!(!none.contains("real_ip_header"));
        let one = edge_conf(&["172.18.0.1".to_string()]);
        let server = one.split("server {\n").nth(1).unwrap();
        assert!(server.contains(
            "    set_real_ip_from 172.18.0.1;\n    real_ip_header X-Forwarded-For;\n    real_ip_recursive on;\n"
        ));
        // Server-wide and before every location, so the limits' keys and the
        // X-Real-IP edge sends the engine are the client's address.
        assert!(one.find("set_real_ip_from").unwrap() < one.find("location ").unwrap());
        assert_eq!(
            one.matches("proxy_set_header X-Real-IP $remote_addr;")
                .count(),
            4
        );
        assert!(
            !one.contains("$http_x_forwarded_for;"),
            "the header is never passed raw"
        );
    }

    #[test]
    fn files_current_follows_the_trusted_proxies_in_env() {
        let t = TempStack::new();
        let s = &t.0;
        enable(s, ID, &fake).unwrap();
        assert!(files_current(s));
        let mut text = std::fs::read_to_string(s.env_path()).unwrap();
        text.push_str(&format!("{TRUSTED_PROXIES_ENV}=172.18.0.1\n"));
        std::fs::write(s.env_path(), &text).unwrap();
        assert!(!files_current(s), "a new proxy list needs enable again");
        enable(s, ID, &fake).unwrap();
        assert!(files_current(s));
        assert!(std::fs::read_to_string(edge_path(s))
            .unwrap()
            .contains("set_real_ip_from 172.18.0.1;"));
        std::fs::write(s.env_path(), text.replace("172.18.0.1", "0.0.0.0/0")).unwrap();
        assert!(!files_current(s));
        assert!(enable(s, ID, &fake)
            .unwrap_err()
            .to_string()
            .contains("every address"));
        disable(s).unwrap();
        assert_eq!(
            s.read_env().get(TRUSTED_PROXIES_ENV).map(String::as_str),
            Some("0.0.0.0/0"),
            "the owner's key stays"
        );
    }

    #[test]
    fn with_trusted_proxies_edge_is_published_on_loopback_only() {
        assert_eq!(
            engine_compose(&[]),
            ENGINE_COMPOSE_TEMPLATE,
            "no list, no change"
        );
        let c = engine_compose(&["172.18.0.1".to_string()]);
        assert!(c.contains("  edge:\n    # ATELIER_TRUSTED_PROXIES is set"));
        assert!(c.contains(
            "    ports: !override\n      - \"127.0.0.1:${HTTP_PORT:-41221}:80\"\n    networks:\n      - default\n      - engine\n"
        ));
        assert_eq!(c.matches("ports:").count(), 1);
        assert_eq!(c.replace(EDGE_LOOPBACK_PORTS, ""), ENGINE_COMPOSE_TEMPLATE);
        // The default stack stays as install.sh writes it: every interface.
        assert!(COMPOSE_TEMPLATE.contains("    ports:\n      - \"${HTTP_PORT:-41221}:80\"\n"));
        assert!(!COMPOSE_TEMPLATE.contains("127.0.0.1"));
    }

    #[test]
    fn enable_writes_the_loopback_publish_with_proxies_and_drops_it_without() {
        let t = TempStack::new();
        let s = &t.0;
        set_proxies(s, "172.18.0.1");
        enable(s, ID, &fake).unwrap();
        let written = std::fs::read_to_string(compose_path(s)).unwrap();
        assert!(written.contains("ports: !override\n      - \"127.0.0.1:"));
        assert!(files_current(s));
        // The check's temporary file is gone.
        let names: Vec<String> = snapshot(&s.home).into_iter().map(|(n, _)| n).collect();
        assert!(!names.iter().any(|n| n.contains(".check-")), "{names:?}");
        // HTTP_PORT stays a port number.
        assert_eq!(s.http_port(), crate::stack::DEFAULT_PORT);
        // The owner empties the list: doctor sees stale files, enable rewrites them.
        let text = std::fs::read_to_string(s.env_path())
            .unwrap()
            .replace(&format!("{TRUSTED_PROXIES_ENV}=172.18.0.1\n"), "");
        std::fs::write(s.env_path(), text).unwrap();
        assert!(!files_current(s));
        let never = |_: &[PathBuf], _: &[(&str, String)]| -> Result<String> {
            panic!("no proxies, no check")
        };
        enable(s, ID, &never).unwrap();
        assert_eq!(
            std::fs::read_to_string(compose_path(s)).unwrap(),
            ENGINE_COMPOSE_TEMPLATE
        );
    }

    #[test]
    fn enable_refuses_proxies_unless_compose_publishes_edge_on_loopback_only() {
        let t = TempStack::new();
        let s = &t.0;
        set_proxies(s, "172.18.0.1");
        let before = snapshot(&s.home);
        let e = enable(s, ID, &appending).unwrap_err().to_string();
        assert!(e.contains("loopback only") && e.contains("*:41221"), "{e}");
        assert_eq!(snapshot(&s.home), before, "nothing written");
        let failing = |_: &[PathBuf], _: &[(&str, String)]| -> Result<String> {
            bail!("yaml: unknown tag !override")
        };
        let e = enable(s, ID, &failing).unwrap_err().to_string();
        assert!(e.contains("unknown tag") && e.contains("2.24"), "{e}");
        assert_eq!(snapshot(&s.home), before, "nothing written");
        let junk = |_: &[PathBuf], _: &[(&str, String)]| Ok("not json".to_string());
        assert!(enable(s, ID, &junk).is_err());
        assert_eq!(snapshot(&s.home), before);
        // An already-enabled stack keeps its files when a re-enable is refused.
        enable(s, ID, &fake).unwrap();
        let on = snapshot(&s.home);
        assert!(enable(s, ID, &appending).is_err());
        assert_eq!(snapshot(&s.home), on);
    }

    #[test]
    fn published_ports_beyond_loopback_are_named() {
        let json = r#"{"services":{"edge":{"ports":[
            {"host_ip":"127.0.0.1","published":"41221"},
            {"host_ip":"::1","published":"41221"},
            {"published":"41221"},
            {"host_ip":"0.0.0.0","published":41222},
            {"host_ip":"::","published":"41223"}]}}}"#;
        assert_eq!(
            config_ports_beyond_loopback(json, "edge").unwrap(),
            ["*:41221", "0.0.0.0:41222", ":::41223"]
        );
        assert!(config_ports_beyond_loopback(json, "app").is_err());
        assert!(
            config_ports_beyond_loopback(r#"{"services":{"edge":{}}}"#, "edge")
                .unwrap()
                .is_empty()
        );
        assert!(config_ports_beyond_loopback("", "edge").is_err());

        let open = r#"{"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"41221"},{"HostIp":"::","HostPort":"41221"}]}"#;
        assert_eq!(
            bindings_beyond_loopback(open).unwrap(),
            ["0.0.0.0:41221", ":::41221"]
        );
        let closed = "{\"80/tcp\":[{\"HostIp\":\"127.0.0.1\",\"HostPort\":\"41221\"}]}\n";
        assert!(bindings_beyond_loopback(closed).unwrap().is_empty());
        assert!(bindings_beyond_loopback("{}").unwrap().is_empty());
        assert!(bindings_beyond_loopback("null").unwrap().is_empty());
        assert!(bindings_beyond_loopback("garbage").is_err());
    }
}
