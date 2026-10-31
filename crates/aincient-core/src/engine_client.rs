//! The Manager's client for the engine sidecar's HTTP API (EXPERIMENTAL,
//! DECISIONS 0483): `https://<site domain>/_atelier/engine/v1/…` with a bearer
//! token. The server is the `aincient-engine` crate; this side never links it.
//!
//! The floor (m7-4, design floor 5), enforced here and not tunable:
//!
//! - **HTTPS only.** A base that isn't `https://` is refused before anything is
//!   sent, unless its host is loopback (`localhost`, `127.0.0.1`, `[::1]`):
//!   tokens and codes never cross a network in clear text.
//! - **No redirects.** A 3xx is an error ([`EngineError::Redirect`]), never
//!   followed: a redirect could carry the next request (and its token) to
//!   another host or to plain HTTP.
//! - **TLS verified normally** ([`Https`]: rustls with the webpki roots). There
//!   is no "accept invalid certificates" setting, here or anywhere else.
//! - **Bounded**: a connect timeout, one deadline per request, and a cap on the
//!   response body ([`MAX_RESPONSE`]).
//! - **Typed errors** per status (401, 403, 404, 405, 409, 429, 5xx), each with
//!   the engine's own message.
//! - **Version first (m3-3, C-08)**: every action reads `GET /v1/version` once
//!   and refuses when the engine's API major differs from [`CLIENT_API_MAJOR`]
//!   ([`check_api`]).
//!
//! The wire is behind [`HttpTransport`], so the rules above are tested against
//! a fake. Pairing (M4) adds `pair`, `pair/request`, `pair/collect` and
//! `pairings/approve` as thin calls over [`EngineClient::call`] with
//! [`Auth::None`] or [`Auth::Token`]; nothing here needs reshaping for them.

use std::cell::RefCell;
use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::engine::CLIENT_API_MAJOR;
use crate::ssh_key::SecretStore;

/// Where edge exposes the engine on a site.
pub const PREFIX: &str = "/_atelier/engine/v1/";
/// The most a response body may be. The largest real answer (`audit`, 200
/// entries of at most 1 KB) is well under it.
pub const MAX_RESPONSE: u64 = 1024 * 1024;
/// TCP connect + TLS handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The whole request, end to end. Slow verbs answer 202 at once and run in
/// the engine's background.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why an engine call didn't do what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// A plain `http://` base for a host that isn't loopback.
    Insecure { url: String },
    /// The base isn't a usable site address.
    BadAddress(String),
    /// The engine (or something in front of it) answered with a redirect,
    /// which is never followed.
    Redirect { status: u16 },
    /// The action needs a pairing token and this Manager has none for the site.
    NotPaired,
    /// 401: no token, a wrong one, or a revoked one.
    Unauthorized(String),
    /// 403: refused for this pairing (not the primary, a senior pairing) or
    /// while locked down (pairing closed, wake by code).
    Forbidden(String),
    /// 404: no engine at this site (the default install has none), or no
    /// such thing.
    NotFound(String),
    /// 405: the engine has the path but not for this method.
    MethodNotAllowed(String),
    /// 409: locked down, or busy with another job.
    Conflict(String),
    /// 429: too many requests or bad tokens; wait a minute.
    TooManyRequests(String),
    /// Any 5xx.
    Server { status: u16, message: String },
    /// A status the API never sends.
    Unexpected { status: u16, message: String },
    /// The response body passed [`MAX_RESPONSE`].
    TooLarge,
    /// A body the Manager can't read.
    Malformed(String),
    /// Couldn't connect, TLS failed, timed out.
    Transport(String),
    /// The engine speaks a newer API major: update the Manager (C-08).
    ManagerTooOld { engine_api: String },
    /// The engine speaks an older API major: update the site.
    SiteTooOld { engine_api: String },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Insecure { url } => write!(
                f,
                "refusing to talk to the engine over plain HTTP ({url}): its token and codes \
                 travel only over HTTPS. Record the site's domain with https://"
            ),
            EngineError::BadAddress(why) => write!(f, "not a usable site address: {why}"),
            EngineError::Redirect { status } => write!(
                f,
                "the site answered the engine call with a redirect ({status}); the Manager \
                 never follows one. Check the domain recorded for this site (https, no www \
                 mismatch)"
            ),
            EngineError::NotPaired => write!(f, "this Manager isn't paired with the site's engine"),
            EngineError::Unauthorized(m) => write!(f, "the engine refused the token (401): {m}"),
            EngineError::Forbidden(m) => write!(f, "the engine refused (403): {m}"),
            EngineError::NotFound(m) => write!(
                f,
                "no engine answered there (404: {m}); it is experimental and off unless \
                 enabled on the site"
            ),
            EngineError::MethodNotAllowed(m) => {
                write!(f, "the engine refused the method (405): {m}")
            }
            EngineError::Conflict(m) => write!(f, "the engine can't do that now (409): {m}"),
            EngineError::TooManyRequests(m) => write!(f, "too many requests (429): {m}"),
            EngineError::Server { status, message } => {
                write!(f, "the engine failed ({status}): {message}")
            }
            EngineError::Unexpected { status, message } => {
                write!(f, "the engine answered {status}: {message}")
            }
            EngineError::TooLarge => write!(
                f,
                "the engine's answer was larger than {} KB; refused",
                MAX_RESPONSE / 1024
            ),
            EngineError::Malformed(m) => write!(f, "the engine's answer can't be read: {m}"),
            EngineError::Transport(m) => write!(f, "couldn't reach the engine: {m}"),
            EngineError::ManagerTooOld { engine_api } => write!(
                f,
                "the site's engine speaks API {engine_api}, newer than this Manager understands \
                 (API {CLIENT_API_MAJOR}.x). Update the Manager, then try again; nothing was done"
            ),
            EngineError::SiteTooOld { engine_api } => write!(
                f,
                "the site's engine speaks API {engine_api}, older than this Manager understands \
                 (API {CLIENT_API_MAJOR}.x). Update the site, then try again; nothing was done"
            ),
        }
    }
}

impl std::error::Error for EngineError {}

/// The version rule (m3-3, C-08), as the engine's `API_VERSION` doc states
/// it: the contract is the **major**. A newer major is refused with "update
/// the Manager", an older one with "update the site"; any minor or patch of
/// the same major is accepted (a minor only adds).
pub fn check_api(engine_api: &str) -> Result<(), EngineError> {
    let major = engine_api
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .filter(|_| engine_api.trim().split('.').count() == 3)
        .ok_or_else(|| EngineError::Malformed(format!("API version {engine_api:?}")))?;
    match major.cmp(&CLIENT_API_MAJOR) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(EngineError::ManagerTooOld {
            engine_api: engine_api.trim().to_string(),
        }),
        std::cmp::Ordering::Less => Err(EngineError::SiteTooOld {
            engine_api: engine_api.trim().to_string(),
        }),
    }
}

/// A site's engine base: `https://host[:port]` (or loopback `http://`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineUrl {
    https: bool,
    authority: String,
}

/// Whether `authority` (`host[:port]`) names this machine.
fn is_loopback(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().map(|h| format!("[{h}]"))
    } else {
        authority.split(':').next().map(str::to_string)
    };
    matches!(
        host.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("localhost" | "127.0.0.1" | "[::1]")
    )
}

impl EngineUrl {
    /// From a site's recorded domain (`cms.example.com`, `https://host:8443`):
    /// https unless a loopback `http://` was given. A non-loopback `http://`
    /// is [`EngineError::Insecure`]; anything with a path, credentials or
    /// odd characters is [`EngineError::BadAddress`].
    pub fn from_domain(domain: &str) -> Result<Self, EngineError> {
        let d = domain.trim().trim_end_matches('/');
        let (https, authority) = if let Some(rest) = d.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = d.strip_prefix("http://") {
            (false, rest)
        } else if d.contains("://") {
            return Err(EngineError::BadAddress(format!("{domain:?}: https only")));
        } else {
            (true, d)
        };
        let bracketed_v6 = authority.starts_with('[')
            && authority[1..].split(']').next().is_some_and(|h| {
                !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
            });
        if !(crate::sites::is_valid_domain(authority) || bracketed_v6) || authority.is_empty() {
            return Err(EngineError::BadAddress(format!(
                "{domain:?}: a host name, or https://host[:port]"
            )));
        }
        if !https && !is_loopback(authority) {
            return Err(EngineError::Insecure {
                url: format!("http://{authority}"),
            });
        }
        Ok(EngineUrl {
            https,
            authority: authority.to_ascii_lowercase(),
        })
    }

    pub fn is_https(&self) -> bool {
        self.https
    }

    /// `https://host[:port]` (or loopback `http://…`), no trailing slash:
    /// how the registry records an engine-paired site.
    pub fn base(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}", self.authority)
    }

    /// `host[:port]`, for messages.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The full URL of an API path (`version`, `pairings/self`).
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!(
            "{scheme}://{}{PREFIX}{}",
            self.authority,
            path.trim_start_matches('/')
        )
    }
}

/// Refuse a URL the floor forbids: anything but `https://`, except loopback.
/// Checked again right before each send, whatever built the URL.
pub fn ensure_secure(url: &str) -> Result<(), EngineError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let loopback = url
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .is_some_and(is_loopback);
    if loopback {
        Ok(())
    } else {
        Err(EngineError::Insecure {
            url: url.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// One request on the wire.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// The pairing token, sent as `Authorization: Bearer …`.
    pub bearer: Option<String>,
    /// A JSON body.
    pub body: Option<Vec<u8>>,
}

/// Never prints the token.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// One response, as the transport got it: the status and at most
/// [`MAX_RESPONSE`] bytes of body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The wire. Implementations must not follow redirects (hand the 3xx back),
/// must verify TLS, must time out, and must fail with
/// [`EngineError::TooLarge`] past [`MAX_RESPONSE`].
pub trait HttpTransport {
    fn send(&self, req: &Request) -> Result<Response, EngineError>;
}

impl<T: HttpTransport + ?Sized> HttpTransport for &T {
    fn send(&self, req: &Request) -> Result<Response, EngineError> {
        (**self).send(req)
    }
}

/// Who a call authenticates as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No token: `version`, and pairing's `pair`, `pair/request`, `pair/collect`.
    None,
    /// The pairing token: every other verb.
    Token,
}

/// `GET /v1/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The API version, semver (`1.0.0`).
    pub api: String,
    /// The engine build.
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub experimental: bool,
}

/// The job a slow verb started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub verb: String,
    #[serde(default)]
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /v1/status`. Fields a later minor adds are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub api: String,
    /// `running`, `exited`, … as compose reports it, or `missing`.
    pub app: String,
    pub db: String,
    pub lockdown: bool,
    #[serde(default)]
    pub wake_paused: bool,
    #[serde(default)]
    pub wake_code_set: bool,
    #[serde(default)]
    pub job: Option<JobView>,
    /// `locked`, `waking`, `resting`, `awake` or `stopped` (M5); empty from
    /// an engine that predates resting mode.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub resting: bool,
    /// The idle timer's minutes (0 = off) and when the site was last woken.
    #[serde(default)]
    pub idle_rest_minutes: Option<u64>,
    #[serde(default)]
    pub woken_at: Option<u64>,
}

/// What a lifecycle verb answered: a started job (202) or, for `unlock`, the
/// new flag.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Acted {
    #[serde(default)]
    pub job: Option<String>,
    /// The job it queued behind (a lockdown during another job).
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub lockdown: Option<bool>,
}

/// The lifecycle verbs the engine owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Freeze,
    Live,
    Rest,
    /// `wake/paired`: a paired Manager's own wake, outside the code limit.
    Wake,
    Lockdown,
    Unlock,
}

impl Verb {
    pub fn path(self) -> &'static str {
        match self {
            Verb::Freeze => "freeze",
            Verb::Live => "live",
            Verb::Rest => "rest",
            Verb::Wake => "wake/paired",
            Verb::Lockdown => "lockdown",
            Verb::Unlock => "unlock",
        }
    }
}

/// The engine's `{"error": "…"}`, or the body's start when it isn't that.
fn message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            match text.char_indices().nth(200) {
                Some((i, _)) => format!("{}…", &text[..i]),
                None => text.to_string(),
            }
        })
}

/// Map a response to its body or a typed error.
pub fn classify(resp: Response) -> Result<Vec<u8>, EngineError> {
    let m = || message(&resp.body);
    match resp.status {
        200..=299 => Ok(resp.body),
        300..=399 => Err(EngineError::Redirect {
            status: resp.status,
        }),
        401 => Err(EngineError::Unauthorized(m())),
        403 => Err(EngineError::Forbidden(m())),
        404 => Err(EngineError::NotFound(m())),
        405 => Err(EngineError::MethodNotAllowed(m())),
        409 => Err(EngineError::Conflict(m())),
        429 => Err(EngineError::TooManyRequests(m())),
        500..=599 => Err(EngineError::Server {
            status: resp.status,
            message: m(),
        }),
        status => Err(EngineError::Unexpected {
            status,
            message: m(),
        }),
    }
}

/// A client for one site's engine.
pub struct EngineClient<T: HttpTransport> {
    base: EngineUrl,
    token: Option<String>,
    transport: T,
    /// The version read this session, once compatible.
    version: RefCell<Option<VersionInfo>>,
}

impl<T: HttpTransport> EngineClient<T> {
    pub fn new(base: EngineUrl, transport: T) -> Self {
        EngineClient {
            base,
            token: None,
            transport,
            version: RefCell::new(None),
        }
    }

    /// The pairing token for [`Auth::Token`] calls.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// One call: the floor check, the request, the status mapping and the
    /// JSON parse. Doesn't check the version: the typed calls below do,
    /// through [`Self::ensure_compatible`].
    pub fn call<J: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        auth: Auth,
        body: Option<&Value>,
    ) -> Result<J, EngineError> {
        let url = self.base.url(path);
        ensure_secure(&url)?;
        let bearer = match auth {
            Auth::None => None,
            Auth::Token => Some(self.token.clone().ok_or(EngineError::NotPaired)?),
        };
        let req = Request {
            method,
            url,
            bearer,
            body: body.map(|b| b.to_string().into_bytes()),
        };
        let resp = self.transport.send(&req)?;
        if resp.body.len() as u64 > MAX_RESPONSE {
            return Err(EngineError::TooLarge);
        }
        let bytes = classify(resp)?;
        serde_json::from_slice(&bytes).map_err(|e| EngineError::Malformed(e.to_string()))
    }

    /// `GET /v1/version`, unauthenticated, as it is.
    pub fn version(&self) -> Result<VersionInfo, EngineError> {
        self.call(Method::Get, "version", Auth::None, None)
    }

    /// The version, read once and checked by [`check_api`]; every action
    /// below calls this first, so a newer or older major refuses before any
    /// verb is sent.
    pub fn ensure_compatible(&self) -> Result<VersionInfo, EngineError> {
        if let Some(v) = self.version.borrow().clone() {
            return Ok(v);
        }
        let v = self.version()?;
        check_api(&v.api)?;
        *self.version.borrow_mut() = Some(v.clone());
        Ok(v)
    }

    /// `GET /v1/status`.
    pub fn status(&self) -> Result<EngineStatus, EngineError> {
        self.ensure_compatible()?;
        self.call(Method::Get, "status", Auth::Token, None)
    }

    /// One lifecycle verb (`POST`).
    pub fn act(&self, verb: Verb) -> Result<Acted, EngineError> {
        self.ensure_compatible()?;
        self.call(Method::Post, verb.path(), Auth::Token, None)
    }

    /// A new wake code (m5-3), returned once; the engine keeps its hash.
    pub fn rotate_wake_code(&self) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Made {
            code: String,
        }
        self.ensure_compatible()?;
        let m: Made = self.call(Method::Post, "wake/code", Auth::Token, None)?;
        Ok(m.code)
    }

    /// No wake code: waking by code is refused until a new one is made.
    pub fn revoke_wake_code(&self) -> Result<(), EngineError> {
        self.ensure_compatible()?;
        let _: Value = self.call(Method::Delete, "wake/code", Auth::Token, None)?;
        Ok(())
    }
}

/// The real wire: `ureq` over rustls with the webpki roots. No redirects, no
/// proxy from the environment (the engine call goes straight to the site),
/// timeouts and the body cap from this module.
#[cfg(feature = "engine-client")]
pub struct Https {
    agent: ureq::Agent,
}

#[cfg(feature = "engine-client")]
impl Default for Https {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "engine-client")]
impl Https {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .max_response_header_size(64 * 1024)
            .user_agent(format!("atelier-manager/{}", env!("CARGO_PKG_VERSION")))
            .build();
        Https {
            agent: config.into(),
        }
    }
}

#[cfg(feature = "engine-client")]
impl HttpTransport for Https {
    fn send(&self, req: &Request) -> Result<Response, EngineError> {
        ensure_secure(&req.url)?;
        let wire = |e: ureq::Error| match e {
            ureq::Error::BodyExceedsLimit(_) => EngineError::TooLarge,
            ureq::Error::Timeout(_) => EngineError::Transport("timed out".into()),
            other => EngineError::Transport(other.to_string()),
        };
        let bearer = req.bearer.as_ref().map(|t| format!("Bearer {t}"));
        let result = match (req.method, &req.body) {
            (Method::Get, _) => {
                let mut r = self
                    .agent
                    .get(&req.url)
                    .header("Accept", "application/json");
                if let Some(b) = &bearer {
                    r = r.header("Authorization", b);
                }
                r.call()
            }
            (Method::Delete, _) => {
                let mut r = self
                    .agent
                    .delete(&req.url)
                    .header("Accept", "application/json");
                if let Some(b) = &bearer {
                    r = r.header("Authorization", b);
                }
                r.call()
            }
            (Method::Post, body) => {
                let mut r = self
                    .agent
                    .post(&req.url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                if let Some(b) = &bearer {
                    r = r.header("Authorization", b);
                }
                r.send(body.as_deref().unwrap_or(b"{}"))
            }
        };
        let mut resp = result.map_err(wire)?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .map_err(wire)?;
        Ok(Response { status, body })
    }
}

/// The keychain account a site's pairing token is kept under (M4 writes it):
/// one per site and Manager, never on disk.
pub fn token_account(site_slug: &str) -> String {
    format!("engine-token:{site_slug}")
}

/// The stored pairing token for `site_slug`, if any.
pub fn load_token(store: &dyn SecretStore, site_slug: &str) -> anyhow::Result<Option<String>> {
    store.get(&token_account(site_slug))
}

pub fn save_token(store: &dyn SecretStore, site_slug: &str, token: &str) -> anyhow::Result<()> {
    store.set(&token_account(site_slug), token)
}

pub fn forget_token(store: &dyn SecretStore, site_slug: &str) -> anyhow::Result<()> {
    store.delete(&token_account(site_slug))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Answers from a script, records what was sent.
    #[derive(Default)]
    struct Fake {
        replies: RefCell<VecDeque<Result<Response, EngineError>>>,
        sent: RefCell<Vec<Request>>,
    }

    impl Fake {
        fn with(replies: Vec<(u16, &str)>) -> Self {
            let f = Fake::default();
            for (status, body) in replies {
                f.replies.borrow_mut().push_back(Ok(Response {
                    status,
                    body: body.as_bytes().to_vec(),
                }));
            }
            f
        }
        fn paths(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .map(|r| format!("{} {}", r.method.as_str(), r.url))
                .collect()
        }
    }

    impl HttpTransport for Fake {
        fn send(&self, req: &Request) -> Result<Response, EngineError> {
            self.sent.borrow_mut().push(req.clone());
            self.replies
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(EngineError::Transport("no reply scripted".into())))
        }
    }

    const V1: &str = r#"{"api":"1.0.0","engine":"0.12.0","experimental":true}"#;

    fn client(fake: &Fake) -> EngineClient<&Fake> {
        EngineClient::new(EngineUrl::from_domain("cms.example.com").unwrap(), fake)
            .with_token("aet_secret")
    }

    #[test]
    fn plain_http_is_refused_unless_loopback() {
        for d in [
            "http://cms.example.com",
            "http://10.0.0.5:8080",
            "HTTP://x.com",
        ] {
            let e = EngineUrl::from_domain(d).unwrap_err();
            assert!(
                matches!(e, EngineError::Insecure { .. } | EngineError::BadAddress(_)),
                "{d}: {e:?}"
            );
        }
        assert!(matches!(
            EngineUrl::from_domain("http://cms.example.com").unwrap_err(),
            EngineError::Insecure { .. }
        ));
        for d in [
            "ftp://x.com",
            "https://u:p@x.com",
            "https://x.com/path",
            "x.com?a=b",
        ] {
            assert!(
                matches!(EngineUrl::from_domain(d), Err(EngineError::BadAddress(_))),
                "{d}"
            );
        }
        let u = EngineUrl::from_domain("cms.example.com").unwrap();
        assert!(u.is_https());
        assert_eq!(
            u.url("version"),
            "https://cms.example.com/_atelier/engine/v1/version"
        );
        assert_eq!(
            EngineUrl::from_domain("https://CMS.example.com:8443/")
                .unwrap()
                .url("status"),
            "https://cms.example.com:8443/_atelier/engine/v1/status"
        );
        for d in ["http://localhost:8080", "http://127.0.0.1:8080"] {
            assert!(!EngineUrl::from_domain(d).unwrap().is_https(), "{d}");
        }
        // Not loopback, however it looks.
        for u in [
            "http://localhost.evil.com/x",
            "http://127.0.0.1.nip.io/x",
            "http://example.com/x",
        ] {
            assert!(ensure_secure(u).is_err(), "{u}");
        }
        assert!(ensure_secure("http://localhost:8080/_atelier/engine/v1/x").is_ok());
        assert!(ensure_secure("http://[::1]:8080/x").is_ok());
        assert!(ensure_secure("https://x.com/y").is_ok());
    }

    #[test]
    fn redirects_are_errors_and_never_followed() {
        for status in [301, 302, 303, 307, 308] {
            let fake = Fake::with(vec![(status, "")]);
            let e = client(&fake).version().unwrap_err();
            assert_eq!(e, EngineError::Redirect { status });
            assert_eq!(fake.sent.borrow().len(), 1, "one request, nothing followed");
        }
    }

    #[test]
    fn every_status_maps_to_its_error_with_the_engines_message() {
        type Check = fn(&EngineError) -> bool;
        let cases: Vec<(u16, Check)> = vec![
            (
                401,
                |e| matches!(e, EngineError::Unauthorized(m) if m == "boom"),
            ),
            (
                403,
                |e| matches!(e, EngineError::Forbidden(m) if m == "boom"),
            ),
            (
                404,
                |e| matches!(e, EngineError::NotFound(m) if m == "boom"),
            ),
            (
                405,
                |e| matches!(e, EngineError::MethodNotAllowed(m) if m == "boom"),
            ),
            (
                409,
                |e| matches!(e, EngineError::Conflict(m) if m == "boom"),
            ),
            (
                429,
                |e| matches!(e, EngineError::TooManyRequests(m) if m == "boom"),
            ),
            (500, |e| {
                matches!(e, EngineError::Server { status: 500, .. })
            }),
            (503, |e| {
                matches!(e, EngineError::Server { status: 503, .. })
            }),
            (418, |e| {
                matches!(e, EngineError::Unexpected { status: 418, .. })
            }),
        ];
        for (status, ok) in cases {
            let fake = Fake::with(vec![(200, V1), (status, r#"{"error":"boom"}"#)]);
            let e = client(&fake).status().unwrap_err();
            assert!(ok(&e), "{status}: {e:?}");
        }
        // A body that isn't the engine's shape still says something.
        let fake = Fake::with(vec![(502, "<html>Bad Gateway</html>")]);
        let e = client(&fake).version().unwrap_err();
        assert!(e.to_string().contains("Bad Gateway"), "{e}");
    }

    #[test]
    fn a_newer_major_asks_for_a_manager_update_and_sends_nothing_else() {
        let fake = Fake::with(vec![(200, r#"{"api":"2.0.0","engine":"0.20.0"}"#)]);
        let c = client(&fake);
        let e = c.act(Verb::Lockdown).unwrap_err();
        assert_eq!(
            e,
            EngineError::ManagerTooOld {
                engine_api: "2.0.0".into()
            }
        );
        assert!(e.to_string().contains("Update the Manager"), "{e}");
        assert_eq!(
            fake.paths(),
            ["GET https://cms.example.com/_atelier/engine/v1/version"]
        );
    }

    #[test]
    fn an_older_major_asks_for_a_site_update() {
        let fake = Fake::with(vec![(200, r#"{"api":"0.9.0"}"#)]);
        let e = client(&fake).status().unwrap_err();
        assert!(matches!(e, EngineError::SiteTooOld { .. }));
        assert!(e.to_string().contains("Update the site"), "{e}");
        assert_eq!(fake.sent.borrow().len(), 1);
    }

    #[test]
    fn the_version_rule_is_the_major_only() {
        assert!(check_api("1.0.0").is_ok());
        assert!(check_api("1.7.3").is_ok(), "a later minor only adds");
        assert!(matches!(
            check_api("2.0.0"),
            Err(EngineError::ManagerTooOld { .. })
        ));
        assert!(matches!(
            check_api("0.1.0"),
            Err(EngineError::SiteTooOld { .. })
        ));
        for junk in ["", "1", "1.0", "v1.0.0", "one.0.0", "1.0.0.0"] {
            assert!(
                matches!(check_api(junk), Err(EngineError::Malformed(_))),
                "{junk:?}"
            );
        }
    }

    #[test]
    fn the_version_is_read_once_then_verbs_carry_the_token() {
        let fake = Fake::with(vec![
            (200, V1),
            (202, r#"{"job":"freeze"}"#),
            (200, r#"{"lockdown":false}"#),
        ]);
        let c = client(&fake);
        assert_eq!(c.act(Verb::Freeze).unwrap().job.as_deref(), Some("freeze"));
        assert_eq!(c.act(Verb::Unlock).unwrap().lockdown, Some(false));
        assert_eq!(
            fake.paths(),
            [
                "GET https://cms.example.com/_atelier/engine/v1/version",
                "POST https://cms.example.com/_atelier/engine/v1/freeze",
                "POST https://cms.example.com/_atelier/engine/v1/unlock",
            ]
        );
        let sent = fake.sent.borrow();
        assert_eq!(sent[0].bearer, None, "version carries no token");
        assert_eq!(sent[1].bearer.as_deref(), Some("aet_secret"));
        assert!(!format!("{:?}", sent[1]).contains("aet_secret"), "redacted");
    }

    #[test]
    fn a_token_call_without_a_token_sends_nothing() {
        let fake = Fake::with(vec![(200, V1)]);
        let c = EngineClient::new(EngineUrl::from_domain("x.com").unwrap(), &fake);
        assert_eq!(c.status().unwrap_err(), EngineError::NotPaired);
        assert_eq!(fake.sent.borrow().len(), 1, "only the version");
    }

    #[test]
    fn oversized_and_malformed_bodies_are_refused() {
        let big = "x".repeat(MAX_RESPONSE as usize + 1);
        let fake = Fake::with(vec![(200, &big)]);
        assert_eq!(client(&fake).version().unwrap_err(), EngineError::TooLarge);
        let fake = Fake::with(vec![(200, "not json")]);
        assert!(matches!(
            client(&fake).version().unwrap_err(),
            EngineError::Malformed(_)
        ));
    }

    #[test]
    fn the_wake_code_is_rotated_and_revoked_over_the_api() {
        let fake = Fake::with(vec![
            (200, V1),
            (201, r#"{"code":"ABCD-EFGH-JKMN-PQRS"}"#),
            (200, r#"{"wake_code_set":false}"#),
        ]);
        let c = client(&fake);
        assert_eq!(c.rotate_wake_code().unwrap(), "ABCD-EFGH-JKMN-PQRS");
        c.revoke_wake_code().unwrap();
        assert_eq!(
            fake.paths()[1..],
            [
                "POST https://cms.example.com/_atelier/engine/v1/wake/code",
                "DELETE https://cms.example.com/_atelier/engine/v1/wake/code"
            ]
        );
        assert!(fake.sent.borrow()[1].bearer.is_some());
        let fake = Fake::with(vec![
            (200, V1),
            (409, r#"{"error":"locked down: unlock first"}"#),
        ]);
        assert!(matches!(
            client(&fake).rotate_wake_code(),
            Err(EngineError::Conflict(_))
        ));
        // Resting mode's status fields, and their absence from an older engine.
        let fake = Fake::with(vec![
            (200, V1),
            (
                200,
                r#"{"api":"1.1.0","app":"exited","db":"exited","lockdown":false,"state":"resting","resting":true,"idle_rest_minutes":30,"woken_at":null}"#,
            ),
        ]);
        let st = client(&fake).status().unwrap();
        assert_eq!(
            (st.state.as_str(), st.resting, st.idle_rest_minutes),
            ("resting", true, Some(30))
        );
    }

    #[test]
    fn status_parses_and_ignores_fields_a_later_minor_adds() {
        let fake = Fake::with(vec![
            (200, r#"{"api":"1.3.0"}"#),
            (
                200,
                r#"{"api":"1.3.0","app":"running","db":"running","lockdown":false,
                   "wake_paused":false,"job":{"verb":"rest","started":5},"new_field":1}"#,
            ),
        ]);
        let st = client(&fake).status().unwrap();
        assert_eq!(st.app, "running");
        assert_eq!(st.job.unwrap().verb, "rest");
    }

    #[test]
    fn tokens_live_in_the_secret_store_per_site() {
        let store = crate::ssh_key::tests::Memory::default();
        assert_eq!(load_token(&store, "blog").unwrap(), None);
        save_token(&store, "blog", "aet_1").unwrap();
        assert_eq!(
            load_token(&store, "blog").unwrap().as_deref(),
            Some("aet_1")
        );
        assert_eq!(load_token(&store, "shop").unwrap(), None);
        forget_token(&store, "blog").unwrap();
        assert_eq!(load_token(&store, "blog").unwrap(), None);
    }

    /// The real transport refuses plain HTTP itself, before connecting.
    #[cfg(feature = "engine-client")]
    #[test]
    fn the_real_transport_refuses_plain_http_before_connecting() {
        let r = Https::new().send(&Request {
            method: Method::Get,
            url: "http://example.com/_atelier/engine/v1/version".into(),
            bearer: Some("aet_x".into()),
            body: None,
        });
        assert!(matches!(r, Err(EngineError::Insecure { .. })));
    }

    /// A loopback server answering each connection with `reply`; returns its
    /// base URL and a counter of the requests it got.
    #[cfg(feature = "engine-client")]
    fn serve_loopback(
        reply: &'static [u8],
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = s.write_all(reply);
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    #[cfg(feature = "engine-client")]
    #[test]
    fn the_real_transport_hands_back_a_redirect_without_following_it() {
        use std::sync::atomic::Ordering;
        let (base, hits) = serve_loopback(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let fake_site = EngineClient::new(EngineUrl::from_domain(&base).unwrap(), Https::new());
        assert_eq!(
            fake_site.version().unwrap_err(),
            EngineError::Redirect { status: 302 }
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "engine-client")]
    #[test]
    fn the_real_transport_caps_the_body() {
        // More than the cap; the read stops at it.
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE + 10
        )
        .into_bytes();
        reply.extend(std::iter::repeat_n(b' ', MAX_RESPONSE as usize + 10));
        let (base, _) = serve_loopback(Box::leak(reply.into_boxed_slice()));
        let c = EngineClient::new(EngineUrl::from_domain(&base).unwrap(), Https::new());
        let e = c.version().unwrap_err();
        assert!(matches!(e, EngineError::TooLarge), "{e:?}");
    }
}
