//! The engine's HTTP API: a closed list of verbs (design "Closed verbs").
//!
//! Edge rewrites `/_atelier/engine/v1/<x>` to `/v1/<x>`, and nothing else
//! reaches this listener: the engine shares its network with edge only (E-05)
//! and closes any other TCP peer at accept. Routing is an exact match on the
//! path; no request field is passed to Docker or Compose (floor 2). The only
//! request values kept are a pairing name (stored, never executed) and a
//! pattern-checked pairing id; codes and handles are only compared.
//!
//! | Method + path | Auth |
//! |---|---|
//! | `GET /v1/version` | none |
//! | `POST /v1/pair` `{code, name}` | install code |
//! | `POST /v1/pair/request` `{name}` | none |
//! | `POST /v1/pair/collect` `{handle}` | handle |
//! | `GET /v1/wake` | none (the wake page) |
//! | `POST /v1/wake` `{code}` or form | wake code |
//! | `GET /v1/wake/status` | none: `waking`, `ready` or `failed`, nothing more |
//! | `POST /v1/wake/paired` | token |
//! | `POST /v1/wake/code` · `DELETE /v1/wake/code` | token: rotate (shown once) or revoke |
//! | `GET /v1/status` | token |
//! | `POST /v1/freeze` · `live` · `rest` · `lockdown` · `unlock` | token |
//! | `GET /v1/pairings` · `DELETE /v1/pairings/self` · `DELETE /v1/pairings/<id>` | token |
//! | `GET /v1/pairings/requests` · `POST /v1/pairings/approve` `{code}` | primary token |
//! | `GET /v1/audit` | token |
//! | `POST /v1/wake/resume` | primary token |
//!
//! Anything else is a 404, a listed path with another method a 405. JSON
//! bodies refuse unknown fields.
//!
//! **Lockdown** (D-09, floor 11) refuses every verb but `status`, `pairings`,
//! revoke, `audit`, `unlock` and `lockdown` itself, which is never refused.
//!
//! **Resting** (M5, D-11, D-12): `rest` freezes, then marks the site resting
//! (edge's marker, [`Inbox::signal_edge`]) and stops `app` and `db`; a wake
//! clears the mark first and starts them. While a woken site gets no console
//! request for the `.env`'s idle minutes, the engine rests it again
//! ([`Engine::tick`]).

use crate::codes::{self, now};
use crate::http::{self, Reply, Request};
use crate::inbox::Inbox;
use crate::rest;
use crate::site::{service_state, service_version, Drush, Site};
use crate::state::{self, Asked, Collected, Pairing, Redeemed, Revoked, Revoker, Store, WakeGate};
use crate::{API_VERSION, ENGINE_VERSION};
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const WAKE_BUDGET: Duration = Duration::from_secs(120);
/// How long a run of identical refusals collapses into one audit line.
const NOISY_WINDOW: u64 = 60;
/// `status` asks compose at most this often; callers in between share it.
const STATES_FRESH: Duration = Duration::from_secs(2);
/// Bad tokens per address per minute before the engine stops checking them.
pub const AUTH_FAILS_PER_MINUTE: u32 = 30;
/// The inbox loop's tick is a second; every this many it makes sure a
/// lockdown has stopped the site and republishes the pairings list.
pub const ENFORCE_EVERY: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Freeze,
    Live,
    Rest,
    Lockdown,
    Unlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Version,
    /// Pair with the install code: the primary.
    Pair,
    /// A new Manager asks to be paired, then polls for its key.
    PairRequest,
    PairCollect,
    WakePage,
    /// Wake by code: public, rate-limited hard at edge.
    Wake,
    /// How a wake goes, for the progress page: public, three words only.
    WakeStatus,
    /// A new wake code, shown once; or none (revoked).
    WakeCode,
    WakeCodeRevoke,
    /// Wake by a paired Manager: its own path, outside the code limit.
    WakePaired,
    Status,
    Verb(Verb),
    Pairings,
    /// Pending requests, and approving one by its code: the primary only.
    Requests,
    Approve,
    RevokeSelf,
    Revoke(String),
    Audit,
    ResumeWake,
    MethodNotAllowed,
    NotFound,
}

/// Every path the API has, without its `/v1/` prefix (the pairing id route
/// as a pattern). Edge names the same list ([`aincient_core::engine::EDGE_ROUTES`]
/// plus the exact `pair` and `wake` locations); a test keeps them in step.
pub const PATHS: &[&str] = &[
    "version",
    "pair",
    "pair/request",
    "pair/collect",
    "wake",
    "wake/paired",
    "wake/status",
    "wake/code",
    "status",
    "freeze",
    "live",
    "rest",
    "lockdown",
    "unlock",
    "pairings",
    "pairings/self",
    "pairings/requests",
    "pairings/approve",
    "pairings/[0-9a-z]{12}",
    "audit",
    "wake/resume",
];

/// The whole API surface. The query string is ignored: no verb takes one.
pub fn route(method: &str, url: &str) -> Route {
    let path = url.split('?').next().unwrap_or("");
    let (want, r) = match path {
        "/v1/version" => ("GET", Route::Version),
        "/v1/pair" => ("POST", Route::Pair),
        "/v1/pair/request" => ("POST", Route::PairRequest),
        "/v1/pair/collect" => ("POST", Route::PairCollect),
        "/v1/wake" if method == "GET" => ("GET", Route::WakePage),
        "/v1/wake" => ("POST", Route::Wake),
        "/v1/wake/paired" => ("POST", Route::WakePaired),
        "/v1/wake/status" => ("GET", Route::WakeStatus),
        "/v1/wake/code" if method == "DELETE" => ("DELETE", Route::WakeCodeRevoke),
        "/v1/wake/code" => ("POST", Route::WakeCode),
        "/v1/status" => ("GET", Route::Status),
        "/v1/freeze" => ("POST", Route::Verb(Verb::Freeze)),
        "/v1/live" => ("POST", Route::Verb(Verb::Live)),
        "/v1/rest" => ("POST", Route::Verb(Verb::Rest)),
        "/v1/lockdown" => ("POST", Route::Verb(Verb::Lockdown)),
        "/v1/unlock" => ("POST", Route::Verb(Verb::Unlock)),
        "/v1/pairings" => ("GET", Route::Pairings),
        "/v1/pairings/self" => ("DELETE", Route::RevokeSelf),
        "/v1/pairings/requests" => ("GET", Route::Requests),
        "/v1/pairings/approve" => ("POST", Route::Approve),
        "/v1/audit" => ("GET", Route::Audit),
        "/v1/wake/resume" => ("POST", Route::ResumeWake),
        p => match p.strip_prefix("/v1/pairings/") {
            Some(id) if codes::is_pairing_id(id) => ("DELETE", Route::Revoke(id.to_string())),
            _ => return Route::NotFound,
        },
    };
    if method == want {
        r
    } else {
        Route::MethodNotAllowed
    }
}

/// What may run while locked down; everything else is a 409.
fn allowed_in_lockdown(r: &Route) -> bool {
    matches!(
        r,
        Route::Status
            | Route::Pairings
            | Route::RevokeSelf
            | Route::Revoke(_)
            | Route::Audit
            | Route::Verb(Verb::Unlock)
            | Route::Verb(Verb::Lockdown)
    )
}

/// The slow verbs, one at a time, in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Freeze,
    Live,
    Rest,
    Wake,
    Lockdown,
}

impl JobKind {
    fn name(self) -> &'static str {
        match self {
            JobKind::Freeze => "freeze",
            JobKind::Live => "live",
            JobKind::Rest => "rest",
            JobKind::Wake => "wake",
            JobKind::Lockdown => "lockdown",
        }
    }
}

#[derive(Debug, Clone)]
struct Job {
    kind: JobKind,
    started: u64,
    finished: Option<u64>,
    error: Option<String>,
}

pub struct Engine {
    store: Store,
    inbox: Inbox,
    site: Box<dyn Site>,
    /// Edge's name on the engine network: the only peer admitted.
    edge: String,
    /// Verified in place of a missing wake code, so timing can't tell.
    dummy_wake: String,
    job: Mutex<Option<Job>>,
    /// Edge's resolved addresses and when they were resolved.
    edge_ips: Mutex<(Option<Instant>, Vec<IpAddr>)>,
    /// Refusals collapse per minute, so they can't flood the log.
    noisy: Mutex<HashMap<String, (u64, u32)>>,
    /// Bad tokens per address in the current minute.
    auth_fails: Mutex<HashMap<String, (u64, u32)>>,
    /// The site's service states and when compose was last asked. Held while
    /// asking, so a burst of `status` calls runs one `compose ps`.
    states: Mutex<(Option<Instant>, Value)>,
    /// One Argon2 verification at a time (19 MiB each, in a 256 MiB container).
    argon: Mutex<()>,
}

fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        body: body.to_string(),
        content_type: "application/json",
        headers: vec![],
    }
}

fn error(status: u16, msg: &str) -> Reply {
    reply(status, json!({ "error": msg }))
}

/// A poisoned lock is still a lock: a panicked handler must not take the
/// engine down with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Parse a JSON body into one of the closed request shapes, or a 400 naming it.
fn body<T: DeserializeOwned>(bytes: &[u8], shape: &str) -> Result<T, Reply> {
    serde_json::from_slice(bytes).map_err(|_| error(400, &format!("send {shape}")))
}

fn name_field(name: &str) -> Result<&str, Reply> {
    let name = name.trim();
    if codes::valid_name(name) {
        Ok(name)
    } else {
        Err(error(400, "name: 1 to 64 visible characters"))
    }
}

fn code_field(code: &str) -> Result<&str, Reply> {
    if code.len() <= codes::CODE_FIELD_MAX {
        Ok(code)
    } else {
        Err(error(400, "code: too long"))
    }
}

impl Engine {
    pub fn new(store: Store, inbox: Inbox, site: Box<dyn Site>, edge: &str) -> Result<Self> {
        Ok(Self {
            store,
            inbox,
            site,
            edge: edge.to_string(),
            dummy_wake: codes::hash_wake_code(&codes::mint_wake_code(), true)?,
            job: Mutex::new(None),
            edge_ips: Mutex::new((None, vec![])),
            noisy: Mutex::default(),
            auth_fails: Mutex::default(),
            states: Mutex::new((None, Value::Null)),
            argon: Mutex::new(()),
        })
    }

    /// Serve on `listener` until the process ends; the inbox loop runs beside.
    pub fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        self.inbox.prepare();
        let me = Arc::clone(&self);
        thread::Builder::new()
            .name("inbox".into())
            .spawn(move || {
                let mut tick = 0u32;
                loop {
                    me.tick(tick);
                    tick = tick.wrapping_add(1);
                    thread::sleep(Duration::from_secs(1));
                }
            })
            .context("start the inbox loop")?;
        let admit = Arc::clone(&self);
        let handle = Arc::clone(&self);
        // Docker's network separation is not enough on its own: Docker
        // Desktop / OrbStack route between bridge networks, so `app` reached
        // this port by IP in the POC run. Only edge's own address may connect,
        // and anyone else is closed before a byte is read.
        http::serve(
            listener,
            move |peer| {
                let ok = admit.is_edge(peer);
                if !ok {
                    admit.audit_noisy(
                        "peer",
                        &peer.ip().to_string(),
                        "connect",
                        "closed: not edge",
                    );
                }
                ok
            },
            move |req| handle.handle(req),
        );
        Ok(())
    }

    /// The client's address, which keys every per-address cap and goes in
    /// the audit log (m7-4). Edge's `X-Real-IP` is its `$remote_addr`: the TCP
    /// peer, or behind a listed TLS proxy the address that proxy forwarded
    /// (`set_real_ip_from`, [`aincient_core::engine::edge_conf`]). The header
    /// is believed only when the peer is edge and it is one well-formed
    /// address, sent once; otherwise the peer is the client. Edge overwrites
    /// whatever a client sent under that name, so nothing past edge can pick
    /// its own address.
    pub fn client_ip(&self, req: &Request) -> String {
        let mut sent = req
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("X-Real-IP"))
            .map(|(_, v)| v.parse::<IpAddr>().ok());
        let forwarded = match (sent.next(), sent.next()) {
            (Some(Some(ip)), None) if self.is_edge(&req.peer) => Some(ip),
            _ => None,
        };
        forwarded.unwrap_or_else(|| req.peer.ip()).to_string()
    }

    pub fn handle(self: &Arc<Self>, req: Request) -> Reply {
        let ip = self.client_ip(&req);
        let route = route(&req.method, &req.url);
        let form = req
            .header("Content-Type")
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
        let b = &req.body;
        match route {
            Route::NotFound => return error(404, "not found"),
            Route::MethodNotAllowed => return error(405, "method not allowed"),
            Route::Version => {
                return reply(
                    200,
                    json!({ "api": API_VERSION, "engine": ENGINE_VERSION, "experimental": true }),
                )
            }
            Route::WakePage => return wake_page(None),
            Route::WakeStatus => return self.wake_status(),
            Route::Pair => return self.pair(b, &ip).unwrap_or_else(|r| r),
            Route::PairRequest => return self.ask(b, &ip).unwrap_or_else(|r| r),
            Route::PairCollect => return self.collect(b, &ip).unwrap_or_else(|r| r),
            Route::Wake => return self.wake_by_code(b, form, &ip),
            _ => {}
        }

        // Everything below needs a paired Manager. A valid token always gets
        // in: limiting by address first would let anyone sharing the owner's
        // address lock the owner's Manager out. Past AUTH_FAILS_PER_MINUTE
        // made-up tokens an address is told to back off.
        let bearer = req
            .header("Authorization")
            .and_then(|v| v.strip_prefix("Bearer "));
        let pairing = match bearer.map(|t| self.store.lock().and_then(|l| l.authenticate(t))) {
            Some(Ok(Some(p))) => p,
            Some(Err(e)) => return self.internal(&e),
            guessed => {
                if guessed.is_some() && self.auth_failures(&ip) > AUTH_FAILS_PER_MINUTE {
                    self.audit_noisy("anonymous", &ip, "auth", "429 too many bad tokens");
                    return error(429, "too many bad tokens; wait a minute");
                }
                self.audit_noisy("anonymous", &ip, &format!("{route:?}"), "401");
                return error(401, "pair this Manager first");
            }
        };
        self.authed(route, b, &pairing, &ip)
            .unwrap_or_else(|e| self.internal(&e))
    }

    /// A failure inside the engine: the detail goes to the log and the audit,
    /// never to the client (compose and drush errors name paths and hosts).
    fn internal(&self, e: &anyhow::Error) -> Reply {
        eprintln!("engine: {e:#}");
        self.store.audit("engine", "-", "error", &format!("{e:#}"));
        error(
            500,
            "internal error; the owner can read it in the engine's audit log",
        )
    }

    /// Write a refusal once a minute per kind, with a count of the ones it
    /// stands for.
    fn audit_noisy(&self, actor: &str, ip: &str, verb: &str, outcome: &str) {
        let key = format!("{actor}|{verb}|{outcome}");
        let at = now();
        let mut seen = lock(&self.noisy);
        if seen.len() > 1024 {
            seen.clear();
        }
        let entry = seen.entry(key).or_insert((0, 0));
        if at.saturating_sub(entry.0) < NOISY_WINDOW {
            entry.1 += 1;
            return;
        }
        let outcome = match entry.1 {
            0 => outcome.to_string(),
            n => format!("{outcome} (and {n} more like it in the minute before)"),
        };
        *entry = (at, 0);
        drop(seen);
        self.store.audit(actor, ip, verb, &outcome);
    }

    /// Count one more bad token from `ip`; returns this minute's count.
    fn auth_failures(&self, ip: &str) -> u32 {
        let minute = now() / 60;
        let mut all = lock(&self.auth_fails);
        if all.len() > 4096 {
            all.retain(|_, (m, _)| *m == minute);
        }
        let e = all.entry(ip.to_string()).or_insert((minute, 0));
        if e.0 != minute {
            *e = (minute, 0);
        }
        e.1 += 1;
        e.1
    }

    /// Whether the TCP peer is edge. Resolved again at most once a second
    /// when a peer doesn't match (edge gets a new address when recreated).
    pub fn is_edge(&self, peer: &SocketAddr) -> bool {
        use std::net::ToSocketAddrs;
        let mut cache = lock(&self.edge_ips);
        if cache.1.contains(&peer.ip()) {
            return true;
        }
        if cache
            .0
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return false;
        }
        let ips = self
            .edge
            .to_socket_addrs()
            .map(|a| a.map(|a| a.ip()).collect())
            .unwrap_or_default();
        *cache = (Some(Instant::now()), ips);
        cache.1.contains(&peer.ip())
    }

    fn locked_down(&self) -> bool {
        // Unreadable state counts as locked down: refuse rather than run.
        self.store
            .lock()
            .and_then(|l| l.flags())
            .map(|f| f.lockdown)
            .unwrap_or(true)
    }

    fn authed(
        self: &Arc<Self>,
        route: Route,
        b: &[u8],
        pairing: &Pairing,
        ip: &str,
    ) -> Result<Reply> {
        let me = pairing.id.as_str();
        let actor = format!("pairing:{me}");
        let audit = |verb: &str, outcome: &str| self.store.audit(&actor, ip, verb, outcome);
        let primary_only = matches!(route, Route::Requests | Route::Approve | Route::ResumeWake);
        if primary_only && !pairing.primary {
            // Only the owner's Manager lets another in or lifts a wake pause:
            // a pairing approved by mistake must not approve the next one.
            self.audit_noisy(&actor, ip, &format!("{route:?}"), "403 not the primary");
            return Ok(error(403, "only the primary pairing can do that"));
        }
        if !allowed_in_lockdown(&route) && self.locked_down() {
            self.audit_noisy(&actor, ip, &format!("{route:?}"), "409 locked down");
            return Ok(error(409, "locked down: unlock first"));
        }
        Ok(match route {
            Route::Status => reply(200, self.status()?),
            Route::WakePaired => self.start_job(JobKind::Wake, &actor, ip),
            Route::Verb(Verb::Freeze) => self.start_job(JobKind::Freeze, &actor, ip),
            Route::Verb(Verb::Live) => self.start_job(JobKind::Live, &actor, ip),
            Route::Verb(Verb::Rest) => self.start_job(JobKind::Rest, &actor, ip),
            Route::Verb(Verb::Lockdown) => {
                // "Drupal is compromised": whatever is junior to this Manager
                // may be the attacker's, so it goes, every pending request
                // dies, and pairing stays closed until unlock. One lock, which
                // adding a pairing takes too.
                let gone = self.store.lock()?.lock_down(me)?;
                rest::sync_edge(&self.store, &self.inbox);
                self.publish_pairings();
                audit(
                    "lockdown",
                    &format!("flag set; revoked junior pairings: {gone:?}"),
                );
                self.start_job(JobKind::Lockdown, &actor, ip)
            }
            Route::Verb(Verb::Unlock) => {
                // Only this Manager and senior ones survive a lockdown, and
                // none can pair during one, so whoever calls this was paired
                // before the incident.
                // Back to resting with wake open (D-09), never straight awake:
                // lockdown set `resting`, and unlock leaves it.
                self.store.lock()?.update_flags(|f| f.lockdown = false)?;
                rest::sync_edge(&self.store, &self.inbox);
                audit("unlock", "ok, resting");
                reply(200, json!({ "lockdown": false }))
            }
            Route::ResumeWake => {
                self.store.lock()?.update_flags(|f| f.wake_resumed())?;
                audit("wake/resume", "ok");
                reply(200, json!({ "wake_paused": false }))
            }
            Route::Pairings => {
                let all = self.store.lock()?.pairings()?;
                reply(200, json!({ "self": me, "pairings": state::views(&all) }))
            }
            Route::Requests => {
                let all = self.store.lock()?.requests()?;
                reply(200, json!({ "requests": state::request_views(&all) }))
            }
            Route::Approve => self.approve(b, me, &actor, ip)?,
            Route::WakeCode => {
                let code = codes::mint_wake_code();
                let hash = {
                    let _one = lock(&self.argon);
                    codes::hash_wake_code(&code, true)?
                };
                self.store.lock()?.update_flags(|f| {
                    f.wake_hash = Some(hash);
                    f.wake_generated = true;
                    f.wake_resumed();
                })?;
                audit("wake/code", "rotated");
                // Shown once: the engine keeps only its hash (floor 4).
                reply(201, json!({ "code": code }))
            }
            Route::WakeCodeRevoke => {
                self.store.lock()?.update_flags(|f| {
                    f.wake_hash = None;
                    f.wake_generated = false;
                })?;
                audit("wake/code", "revoked");
                reply(200, json!({ "wake_code_set": false }))
            }
            Route::RevokeSelf => self.revoke(Revoker::Pairing(me), me, &actor, ip)?,
            Route::Revoke(id) => self.revoke(Revoker::Pairing(me), &id, &actor, ip)?,
            Route::Audit => reply(200, json!({ "entries": self.store.audit.tail(200) })),
            Route::Version
            | Route::Pair
            | Route::PairRequest
            | Route::PairCollect
            | Route::WakePage
            | Route::Wake
            | Route::WakeStatus
            | Route::NotFound
            | Route::MethodNotAllowed => unreachable!("answered before authentication"),
        })
    }

    pub(crate) fn revoke(&self, by: Revoker, id: &str, actor: &str, ip: &str) -> Result<Reply> {
        let r = self.store.lock()?.revoke(by, id)?;
        match r {
            Revoked::Yes => {
                self.store.audit(actor, ip, &format!("revoke {id}"), "ok");
                self.publish_pairings();
            }
            // Made-up ids cost nothing to send: collapsed, so a loop of them
            // can't push real entries out of the log.
            Revoked::NotFound => self.audit_noisy(actor, ip, "revoke", "404"),
            Revoked::Refused(why) => self.audit_noisy(actor, ip, "revoke", &format!("403 {why}")),
        }
        Ok(match r {
            Revoked::Yes => reply(200, json!({ "revoked": id })),
            Revoked::NotFound => error(404, "no such pairing"),
            Revoked::Refused(why) => error(403, why),
        })
    }

    fn closed(&self, ip: &str, verb: &str) -> Reply {
        self.audit_noisy("anonymous", ip, verb, "403 locked down");
        error(403, "this site is locked down: pairing is closed")
    }

    /// Pair with the install code (E-02). Only SSH makes one, so this pairs
    /// the owner, as the primary. Wrong and missing codes get one answer.
    fn pair(&self, b: &[u8], ip: &str) -> Result<Reply, Reply> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            code: String,
            name: String,
        }
        let req: Body = body(b, "{\"code\", \"name\"}")?;
        let name = name_field(&req.name)?;
        let code = code_field(&req.code)?;
        Ok(
            match self
                .store
                .lock()
                .and_then(|l| l.redeem_install_code(code, name))
            {
                Ok(Redeemed::Paired(p, token)) => {
                    self.store.audit(
                        &format!("pairing:{}", p.id),
                        ip,
                        "pair",
                        &format!("ok, name {name:?}, install code, primary"),
                    );
                    self.publish_pairings();
                    reply(
                        201,
                        json!({ "id": p.id, "token": token, "api": API_VERSION }),
                    )
                }
                Ok(Redeemed::Wrong) => {
                    self.audit_noisy("anonymous", ip, "pair", "401");
                    error(401, "wrong install code")
                }
                Ok(Redeemed::Closed) => self.closed(ip, "pair"),
                Err(e) => self.internal(&e),
            },
        )
    }

    /// A new Manager asks to be paired. Anyone who reaches the site may ask:
    /// the answer is a code to show and a handle to poll with, never a key.
    fn ask(&self, b: &[u8], ip: &str) -> Result<Reply, Reply> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            name: String,
        }
        let req: Body = body(b, "{\"name\"}")?;
        let name = name_field(&req.name)?;
        Ok(match self.store.lock().and_then(|l| l.ask(name, ip)) {
            Ok(Asked::Pending(code, handle)) => {
                self.store.audit(
                    "anonymous",
                    ip,
                    "pair/request",
                    &format!("pending, name {name:?}"),
                );
                reply(
                    201,
                    json!({
                        "code": codes::group(&code, 4),
                        "handle": handle,
                        "expires_in": state::REQUEST_TTL,
                    }),
                )
            }
            Ok(Asked::TooMany) => {
                self.audit_noisy("anonymous", ip, "pair/request", "429 too many pending");
                error(429, "too many pending requests; try again in a few minutes")
            }
            Ok(Asked::Closed) => self.closed(ip, "pair/request"),
            Err(e) => self.internal(&e),
        })
    }

    /// The new Manager polls with its handle; once approved it gets its key,
    /// once.
    fn collect(&self, b: &[u8], ip: &str) -> Result<Reply, Reply> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            handle: String,
        }
        let req: Body = body(b, "{\"handle\"}")?;
        if !codes::is_handle_shaped(&req.handle) {
            return Err(error(400, "handle: 64 hex characters"));
        }
        Ok(
            match self.store.lock().and_then(|l| l.collect(&req.handle)) {
                Ok(Collected::Waiting) => reply(202, json!({ "approved": false })),
                Ok(Collected::Paired(p, token)) => {
                    self.store.audit(
                        &format!("pairing:{}", p.id),
                        ip,
                        "pair",
                        &format!("ok, name {:?}, {}", p.name, p.via),
                    );
                    self.publish_pairings();
                    reply(
                        201,
                        json!({ "id": p.id, "token": token, "api": API_VERSION }),
                    )
                }
                Ok(Collected::Gone) => {
                    self.audit_noisy("anonymous", ip, "pair/collect", "404");
                    error(404, "no such request: it expired, or the site locked down")
                }
                Err(e) => self.internal(&e),
            },
        )
    }

    /// The primary approves a request by the code the new Manager shows.
    fn approve(&self, b: &[u8], me: &str, actor: &str, ip: &str) -> Result<Reply> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            code: String,
        }
        let req: Body = match body(b, "{\"code\"}") {
            Ok(r) => r,
            Err(r) => return Ok(r),
        };
        let code = match code_field(&req.code) {
            Ok(c) => c,
            Err(r) => return Ok(r),
        };
        Ok(match self.store.lock()?.approve(code, me)? {
            Some(r) => {
                self.store.audit(
                    actor,
                    ip,
                    "pairings/approve",
                    &format!("ok, name {:?}, asked from {}", r.name, r.ip),
                );
                reply(200, json!({ "approved": r.name }))
            }
            None => {
                self.audit_noisy(actor, ip, "pairings/approve", "404");
                error(404, "no pending request has that code")
            }
        })
    }

    /// Wake by code (D-10). It wakes and does nothing else (floor 6): no
    /// sign-in, no token, no other verb. Every refusal but lockdown and the
    /// pause is the same 401.
    fn wake_by_code(self: &Arc<Self>, b: &[u8], form: bool, ip: &str) -> Reply {
        let page = |status: u16, msg: &str| {
            if form {
                let mut r = wake_page(Some(msg));
                r.status = status;
                r
            } else {
                error(status, msg)
            }
        };
        let typed = if form {
            form_field(b, "code").unwrap_or_default()
        } else {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Body {
                code: String,
            }
            match body::<Body>(b, "{\"code\"}") {
                Ok(r) => r.code,
                Err(r) => return r,
            }
        };
        if typed.len() > codes::CODE_FIELD_MAX {
            return page(400, "That code is too long.");
        }
        // The try is counted before the code is checked, under the lock, so
        // tries sent at once can't all get in under the cap.
        let at = now();
        let gate = self.store.lock().and_then(|l| {
            let mut f = l.flags()?;
            if f.lockdown {
                return Ok((None, f));
            }
            let g = f.wake_try(ip, at);
            l.set_flags(&f)?;
            Ok((Some(g), f))
        });
        let (gate, flags) = match gate {
            Ok(g) => g,
            Err(e) => return self.internal(&e),
        };
        let gate = match gate {
            None => {
                self.audit_noisy("wake-code", ip, "wake", "403 locked down");
                return page(
                    403,
                    "This site is locked down. Its owner can unlock it from the Manager.",
                );
            }
            Some((gate, true)) => {
                self.store.audit(
                    "engine",
                    "-",
                    "wake",
                    &format!(
                        "paused for everyone until {} after too many tries",
                        flags.wake_pause.until
                    ),
                );
                gate
            }
            Some((gate, false)) => gate,
        };
        if gate != WakeGate::Go {
            self.audit_noisy("wake-code", ip, "wake", &format!("429 {gate:?}"));
            return page(
                429,
                "Waking by code is paused for a while after too many wrong codes. Try again later.",
            );
        }
        // Verified outside the state lock (Argon2id takes a while), one at a
        // time.
        let right = {
            let _one = lock(&self.argon);
            codes::verify_wake_code(
                flags.wake_hash.as_deref(),
                flags.wake_generated,
                &typed,
                &self.dummy_wake,
            )
        };
        if !right {
            self.audit_noisy("wake-code", ip, "wake", "401");
            return page(401, "That code didn't work.");
        }
        if let Err(e) = self
            .store
            .lock()
            .and_then(|l| l.update_flags(|f| f.wake_succeeded(ip)))
        {
            return self.internal(&e);
        }
        let r = self.start_job(JobKind::Wake, "wake-code", ip);
        match (form, r.status) {
            (true, 202) => progress_page(),
            (true, status) => page(
                status,
                "The site is busy with another change. Try again in a minute.",
            ),
            (false, _) => r,
        }
    }

    fn status(&self) -> Result<Value> {
        let flags = self.store.lock()?.flags()?;
        let job = lock(&self.job).clone();
        let states = self.states(false);
        let app = service_state(&states, "app");
        let waking = job
            .as_ref()
            .is_some_and(|j| j.kind == JobKind::Wake && j.finished.is_none());
        Ok(json!({
            "api": API_VERSION,
            "state": rest::state_word(&flags, waking, &app),
            "resting": flags.resting,
            "idle_rest_minutes": self.site.idle_rest_minutes(),
            "woken_at": flags.woken_at,
            "app": app,
            "db": service_state(&states, "db"),
            "version": service_version(&states, "app"),
            "lockdown": flags.lockdown,
            "wake_paused": flags.wake_paused(now()),
            "wake_paused_until": flags.wake_pause.until,
            "wake_code_set": flags.wake_hash.is_some(),
            "install_code_unused": flags.install_hash.is_some(),
            "job": job.map(|j| json!({
                "verb": j.kind.name(), "started": j.started, "finished": j.finished, "error": j.error,
            })),
        }))
    }

    /// The service states, asked again only when the last answer is older
    /// than [`STATES_FRESH`] (or `fresh` says so). A failed `compose ps`
    /// reads as every service missing.
    fn states(&self, fresh: bool) -> Value {
        let mut cache = lock(&self.states);
        if !fresh && cache.0.is_some_and(|t| t.elapsed() < STATES_FRESH) {
            return cache.1.clone();
        }
        let v = self.site.states().unwrap_or_else(|e| {
            eprintln!("engine: {e:#}");
            Value::Null
        });
        *cache = (Some(Instant::now()), v);
        cache.1.clone()
    }

    /// One slow verb at a time, in the background; `status` reports it.
    /// While locked down only lockdown runs. Lockdown is never refused: if a
    /// job is running, it is cut short and lockdown runs straight after.
    fn start_job(self: &Arc<Self>, kind: JobKind, actor: &str, ip: &str) -> Reply {
        let verb = kind.name();
        if kind != JobKind::Lockdown && self.locked_down() {
            self.audit_noisy(actor, ip, verb, "409 locked down");
            return error(409, "locked down: unlock first");
        }
        let mut slot = lock(&self.job);
        if let Some(j) = slot.as_ref().filter(|j| j.finished.is_none()) {
            if kind == JobKind::Lockdown {
                self.store
                    .audit(actor, ip, verb, &format!("queued after {}", j.kind.name()));
                return reply(202, json!({ "job": verb, "after": j.kind.name() }));
            }
            self.store
                .audit(actor, ip, verb, &format!("409 busy with {}", j.kind.name()));
            return error(409, &format!("busy: {} is running", j.kind.name()));
        }
        let me = Arc::clone(self);
        let who = actor.to_string();
        // The slot is filled only once the thread exists: a job that never
        // started must not hold it, or every verb would be "busy" for good.
        // The new thread waits on the slot's lock, held here until then.
        let spawned = thread::Builder::new()
            .name(verb.into())
            .spawn(move || me.run_jobs(kind, &who));
        if let Err(e) = spawned {
            drop(slot);
            self.store
                .audit(actor, ip, verb, &format!("503 no thread: {e}"));
            return error(503, "the engine is overloaded; try again");
        }
        *slot = Some(Job {
            kind,
            started: now(),
            finished: None,
            error: None,
        });
        drop(slot);
        self.store.audit(actor, ip, verb, "started");
        reply(202, json!({ "job": verb }))
    }

    /// Run `kind`, then lockdown if it came in meanwhile. The flag is read
    /// under the job lock that lockdown's `start_job` takes after setting it,
    /// so a lockdown is either seen here or starts its own job.
    fn run_jobs(&self, mut kind: JobKind, actor: &str) {
        drop(lock(&self.job));
        loop {
            // A panic ends this job as a failure, never with the slot held.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(kind)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the job panicked")));
            // The services changed: the next `status` asks compose again.
            *lock(&self.states) = (None, Value::Null);
            let error = result.as_ref().err().map(|e| format!("{e:#}"));
            self.store.audit(
                actor,
                "-",
                kind.name(),
                &error
                    .clone()
                    .map_or("done".into(), |e| format!("failed: {e}")),
            );
            let mut slot = lock(&self.job);
            if kind != JobKind::Lockdown && self.locked_down() {
                kind = JobKind::Lockdown;
                *slot = Some(Job {
                    kind,
                    started: now(),
                    finished: None,
                    error: None,
                });
                continue;
            }
            if let Some(j) = slot.as_mut() {
                j.finished = Some(now());
                j.error = error;
            }
            return;
        }
    }

    fn run(&self, kind: JobKind) -> Result<()> {
        // Any job but lockdown is cut short when a lockdown comes in, so the
        // lockdown runs at once instead of waiting for, say, a hung drush.
        let cut = || self.locked_down();
        match kind {
            JobKind::Freeze => self.site.drush(Drush::Freeze, &cut),
            JobKind::Live => self.site.drush(Drush::GoLive, &cut),
            // D-11, D-12: freeze, then rest; a failed freeze keeps it awake.
            JobKind::Rest => rest::rest(self.site.as_ref(), &self.store, &self.inbox, &cut),
            // Incident first: stop, and run nothing of Drupal's on the way
            // (a compromised Drupal could make a freeze hang or flood). Edge
            // then serves the last snapshot made before, with the resting
            // copy (no wake form) for everything else.
            JobKind::Lockdown => {
                rest::sync_edge(&self.store, &self.inbox);
                self.site.stop_app_and_db(&|| false)
            }
            JobKind::Wake => {
                rest::begin_wake(&self.store, &self.inbox)?;
                let r = self
                    .site
                    .start_db_and_app(&cut)
                    .and_then(|()| self.wait_ready());
                if r.is_err() && !self.locked_down() {
                    // Back to a clean resting site rather than half awake
                    // behind no resting copy.
                    rest::wake_failed(self.site.as_ref(), &self.store, &self.inbox);
                }
                r
            }
        }
    }

    /// While locked down, `app` and `db` stay stopped: checked when the
    /// engine starts and every [`ENFORCE_EVERY`] seconds, so a lockdown cut
    /// off by a restart, or a site started by hand, is stopped again.
    fn enforce_lockdown(self: &Arc<Self>) {
        if !self.locked_down() {
            return;
        }
        let busy = lock(&self.job)
            .as_ref()
            .is_some_and(|j| j.finished.is_none());
        let states = self.states(true);
        let up = ["app", "db"]
            .iter()
            .any(|s| !matches!(service_state(&states, s).as_str(), "exited" | "missing"));
        if !busy && up {
            self.start_job(JobKind::Lockdown, "engine", "-");
        }
    }

    /// Ready when Drupal answers through edge. A lockdown ends the wait (the
    /// lockdown then runs).
    fn wait_ready(&self) -> Result<()> {
        let t0 = Instant::now();
        while t0.elapsed() < WAKE_BUDGET {
            if self.locked_down() {
                bail!("a lockdown came in during the wake");
            }
            if self.site.answers() {
                eprintln!("engine: awake in {:.1}s", t0.elapsed().as_secs_f32());
                return Ok(());
            }
            thread::sleep(Duration::from_millis(500));
        }
        bail!("the site didn't come up within {}s", WAKE_BUDGET.as_secs())
    }

    fn busy(&self) -> bool {
        lock(&self.job)
            .as_ref()
            .is_some_and(|j| j.finished.is_none())
    }

    /// A resting site whose `app` runs with no job of the engine's was
    /// started by hand (`atelier app start`, compose): it is awake, so edge
    /// stops showing the resting copy, and the idle timer runs for it.
    fn reconcile_resting(&self) {
        let resting = self
            .store
            .lock()
            .and_then(|l| l.flags())
            .is_ok_and(|f| f.resting && !f.lockdown);
        if !resting || self.busy() {
            return;
        }
        if service_state(&self.states(true), "app") != "running" {
            return;
        }
        let at = now();
        let changed = self.store.lock().and_then(|l| {
            l.update_flags(|f| {
                let still = f.resting && !f.lockdown;
                if still {
                    f.resting = false;
                    f.woken_at = Some(at);
                }
                still
            })
        });
        if changed.unwrap_or(false) {
            rest::sync_edge(&self.store, &self.inbox);
            self.store.audit(
                "engine",
                "-",
                "wake",
                "app running: woken outside the engine",
            );
        }
    }

    /// The idle timer (D-11, m5-5): a site the engine woke goes back to rest
    /// once nobody signed in has used the console for the `.env`'s idle
    /// minutes. "Used" is edge's activity log: console requests Drupal
    /// answered 2xx, which an anonymous visitor can't produce (the console
    /// answers them 403 or a redirect). A rest that fails (the freeze) keeps
    /// the site awake and is tried again one idle period later, not at once.
    pub fn idle_check(self: &Arc<Self>, at: u64) {
        let Ok(f) = self.store.lock().and_then(|l| l.flags()) else {
            return;
        };
        let Some(woken) = f.woken_at.filter(|_| !f.resting && !f.lockdown) else {
            return;
        };
        let minutes = self.site.idle_rest_minutes();
        if minutes == 0 || self.busy() {
            return;
        }
        let last = woken.max(self.inbox.last_activity().unwrap_or(0));
        if at < last.saturating_add(minutes * 60) {
            return;
        }
        if let Err(e) = self
            .store
            .lock()
            .and_then(|l| l.update_flags(|f| f.woken_at = Some(at)))
        {
            eprintln!("engine: {e:#}");
            return;
        }
        self.store.audit(
            "idle timer",
            "-",
            "rest",
            &format!("no console request for {minutes} min"),
        );
        self.start_job(JobKind::Rest, "idle timer", "-");
    }

    /// `waking`, `ready` or `failed`, for the wake progress page (m5-4):
    /// anyone may ask, so it says nothing else (no services, no job, no
    /// error) and runs no compose: it reads the flags and the job slot.
    fn wake_status(&self) -> Reply {
        let job = lock(&self.job).clone();
        let flags = self.store.lock().and_then(|l| l.flags());
        let word = match (&job, &flags) {
            (Some(j), _) if j.kind == JobKind::Wake && j.finished.is_none() => "waking",
            (Some(j), _) if j.kind == JobKind::Wake && j.error.is_some() => "failed",
            (_, Ok(f))
                if !f.resting
                    && !f.lockdown
                    && job.as_ref().is_none_or(|j| j.finished.is_some()) =>
            {
                "ready"
            }
            _ => "failed",
        };
        let mut r = reply(200, json!({ "state": word }));
        r.headers.push(("Cache-Control", "no-store"));
        r
    }

    fn publish_pairings(&self) {
        if let Ok(all) = self.store.lock().and_then(|l| l.pairings()) {
            self.inbox.publish(
                "pairings.json",
                json!(state::views(&all)).to_string().as_bytes(),
            );
        }
    }

    /// One second of the inbox loop: the console's revoke request, and every
    /// [`ENFORCE_EVERY`] ticks the lockdown check and the pairings list (idle
    /// expiry changes it).
    pub fn tick(self: &Arc<Self>, n: u32) {
        if n % ENFORCE_EVERY == 0 {
            self.enforce_lockdown();
            self.reconcile_resting();
            self.idle_check(now());
            rest::sync_edge(&self.store, &self.inbox);
            self.publish_pairings();
        }
        match self.inbox.take_revoke() {
            Some(Ok(r)) if codes::is_pairing_id(&r.id) => {
                let _ = self.revoke(Revoker::Console, &r.id, "console (its claim)", "inbox");
            }
            Some(Ok(_)) => self.audit_noisy("console", "inbox", "revoke", "refused: bad id"),
            Some(Err(e)) => self.audit_noisy(
                "inbox",
                "-",
                crate::inbox::REVOKE_REQUEST,
                &format!("refused: {e}"),
            ),
            None => {}
        }
    }
}

/// One field of an `application/x-www-form-urlencoded` body.
pub fn form_field(body: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The look every engine page shares: plain, readable, no external asset.
const PAGE_STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;margin:15vh auto;\
max-width:32rem;padding:0 1.5rem;color:#222;background:#fff}h1{font-size:1.4rem}\
input,button{font:inherit;padding:.4rem .6rem}\
@media (prefers-color-scheme:dark){body{color:#eee;background:#161616}}";

fn engine_page(status: u16, csp: &'static str, title: &str, body: &str) -> Reply {
    Reply {
        status,
        content_type: "text/html; charset=utf-8",
        // Never framed: a page on this origin can't overlay the form and
        // read the code as it is typed. Never cached: it says how things are.
        headers: vec![
            ("X-Frame-Options", "DENY"),
            ("Content-Security-Policy", csp),
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
        ],
        body: format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>{PAGE_STYLE}</style></head><body>{body}</body></html>"
        ),
    }
}

const FORM_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'";

/// The wake page a resting site's visitor or owner uses. Static text only:
/// `msg` is always one of the engine's own sentences, never request input.
fn wake_page(msg: Option<&str>) -> Reply {
    let note = msg
        .map(|m| format!("<p role=\"status\">{m}</p>"))
        .unwrap_or_default();
    engine_page(
        200,
        FORM_CSP,
        "Wake this site",
        &format!(
            "<main><h1>This site is resting</h1><p>Visitors see its saved pages. To work on \
it, enter its wake code. Waking starts the site; it doesn't sign you in.</p>{note}\
<form method=\"post\" action=\"/_atelier/engine/v1/wake\"><label for=\"code\">Wake code</label> \
<input id=\"code\" name=\"code\" autocomplete=\"off\" required> <button>Wake</button></form></main>"
        ),
    )
}

/// The progress page's one script: poll [`Engine::wake_status`] and open the
/// console once Drupal answers. It reads three words and nothing else.
pub const PROGRESS_SCRIPT: &str = "(function(){var s=document.getElementById('s'),n=0;\
function say(t){s.textContent=t;}\
function poll(){fetch('/_atelier/engine/v1/wake/status',{cache:'no-store',credentials:'omit'})\
.then(function(r){return r.json();}).then(function(v){\
if(v.state==='ready'){say('Ready. Opening the console\\u2026');location.assign('/atelier');}\
else if(v.state==='failed'){say('The site didn\\u2019t wake. Try again in a minute, or ask its owner.');}\
else if(++n<90){setTimeout(poll,2000);}\
else{say('This is taking longer than it should. Open the console again in a minute.');}})\
.catch(function(){if(++n<90)setTimeout(poll,2000);});}\
setTimeout(poll,1000);})();";

/// The script's CSP: only this script, by its hash, may run; it may fetch
/// only this origin.
fn progress_csp() -> &'static str {
    static CSP: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CSP.get_or_init(|| {
        let hash = codes::base64(&codes::sha256(PROGRESS_SCRIPT.as_bytes()));
        Box::leak(
            format!(
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{hash}'; \
                 connect-src 'self'; form-action 'self'; frame-ancestors 'none'"
            )
            .into_boxed_str(),
        )
    })
}

/// After a right wake code (m5-4): the site is starting; the page follows the
/// wake and opens the console when Drupal answers. Without scripts it still
/// says what to do.
fn progress_page() -> Reply {
    engine_page(
        202,
        progress_csp(),
        "Waking this site",
        &format!(
            "<main><h1>Waking the site</h1><p id=\"s\" role=\"status\" aria-live=\"polite\">\
Starting. This takes under a minute; the console opens by itself when it is ready.</p>\
<noscript><p>When it is ready, <a href=\"/atelier\">open the console</a>.</p></noscript>\
<script>{PROGRESS_SCRIPT}</script></main>"
        ),
    )
}

#[cfg(test)]
mod tests;
