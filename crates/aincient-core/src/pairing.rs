//! Pairing a Manager with a site's engine without SSH (M4, E-02),
//! EXPERIMENTAL (DECISIONS 0483). The Manager side of the design's
//! "Pairing" section, as thin calls over [`EngineClient::call`]:
//!
//! - **First Manager**: the install code only SSH makes ([`pair`],
//!   `POST pair {code, name}`); that pairing is the primary.
//! - **Every later Manager** asks ([`request_join`], `POST pair/request
//!   {name}`), shows the 8-symbol code, and polls ([`wait_for_approval`] over
//!   [`collect`], `POST pair/collect {handle}`: 202 until approved, then the
//!   token once). The owner types that code into the PRIMARY ([`approve`]);
//!   the primary's list ([`requests`]) shows name and address, never a code.
//! - **Revoke**: a Manager revokes itself ([`forget_self`]) or a junior one
//!   ([`revoke`]); a revoked token fails its next call with 401, which
//!   [`is_removed`] recognises so the front-ends say "this Manager was
//!   removed" and offer to forget it.
//!
//! Every call reads the version first ([`EngineClient::ensure_compatible`]),
//! so a newer or older API major refuses before a code is sent. Tokens,
//! handles and codes never reach a log: the types here redact them in
//! `Debug`.

use std::fmt;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::engine_client::{
    save_token, Auth, EngineClient, EngineError, EngineUrl, HttpTransport, Method,
};
use crate::sites::Site;
use crate::ssh_key::SecretStore;

/// A pairing name: 1–64 visible characters, none of the control, bidi or
/// zero-width ones the engine refuses (design "Pairing" 6).
pub const NAME_MAX: usize = 64;

/// The engine's own rule ([`aincient_engine::codes::valid_name`]), checked
/// here first so a bad name never costs a request.
pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=NAME_MAX).contains(&n)
        && !name.chars().any(|c| {
            c.is_control()
                || matches!(c as u32,
                    0x00AD | 0x061C | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E
                    | 0x2060..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
        })
}

/// `name` made acceptable: invisible characters dropped, trimmed, cut to
/// [`NAME_MAX`]; `None` when nothing is left.
pub fn clean_name(name: &str) -> Option<String> {
    let kept: String = name
        .chars()
        .filter(|c| valid_name(&c.to_string()))
        .take(NAME_MAX)
        .collect();
    let kept = kept.trim().to_string();
    valid_name(&kept).then_some(kept)
}

/// What this Manager calls itself when it pairs, unless the owner types
/// another: the computer's name ("Shibin's MacBook Pro"), else its host
/// name, else "Atelier Manager".
pub fn default_name() -> String {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let found = if cfg!(target_os = "macos") {
        run("scutil", &["--get", "ComputerName"])
    } else {
        None
    }
    .or_else(|| std::env::var("COMPUTERNAME").ok())
    .or_else(|| run("hostname", &[]));
    found
        .as_deref()
        .and_then(clean_name)
        .unwrap_or_else(|| "Atelier Manager".to_string())
}

/// A new pairing: its id and its token, returned once.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Paired {
    pub id: String,
    pub token: String,
}

impl fmt::Debug for Paired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Paired")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// An open request to join: the code to show, the handle to poll with.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct JoinRequest {
    /// `XXXX-XXXX`: shown on this Manager, typed into the primary.
    pub code: String,
    /// 256 bits; whoever holds it collects the token once approved.
    pub handle: String,
    /// Seconds the request lives (10 minutes).
    pub expires_in: u64,
}

impl fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinRequest")
            .field("code", &"<shown on screen only>")
            .field("handle", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// One poll of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collected {
    /// 202: not approved yet.
    Waiting,
    /// Approved: the pairing exists now and this is its token, once.
    Paired(Paired),
}

/// A pairing, as the engine lists it (never its token or hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub last_used: u64,
    #[serde(default)]
    pub via: String,
    #[serde(default)]
    pub primary: bool,
}

/// `GET pairings`: every pairing, senior first, and which one is this
/// Manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Managers {
    #[serde(rename = "self")]
    pub me: String,
    pub pairings: Vec<PairingView>,
}

impl Managers {
    /// This Manager's own pairing.
    pub fn mine(&self) -> Option<&PairingView> {
        self.pairings.iter().find(|p| p.id == self.me)
    }

    /// Whether this Manager is the site's primary (the only one that
    /// approves).
    pub fn is_primary(&self) -> bool {
        self.mine().is_some_and(|p| p.primary)
    }
}

/// A Manager waiting to be approved, as the primary sees it: name and
/// address, never its code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestView {
    pub name: String,
    pub ip: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub approved: bool,
}

fn checked_name(name: &str) -> Result<&str, EngineError> {
    let name = name.trim();
    if valid_name(name) {
        Ok(name)
    } else {
        Err(EngineError::BadAddress(
            "the Manager's name: 1 to 64 visible characters".into(),
        ))
    }
}

/// `POST pair {code, name}`: the install code (E-02). The pairing made is
/// the site's primary. A wrong, used or missing code is one 401.
pub fn pair<T: HttpTransport>(
    c: &EngineClient<T>,
    code: &str,
    name: &str,
) -> Result<Paired, EngineError> {
    let name = checked_name(name)?;
    c.ensure_compatible()?;
    c.call(
        Method::Post,
        "pair",
        Auth::None,
        Some(&json!({ "code": code.trim(), "name": name })),
    )
}

/// `POST pair/request {name}`: ask to join. Lets nobody in by itself.
pub fn request_join<T: HttpTransport>(
    c: &EngineClient<T>,
    name: &str,
) -> Result<JoinRequest, EngineError> {
    let name = checked_name(name)?;
    c.ensure_compatible()?;
    let r: JoinRequest = c.call(
        Method::Post,
        "pair/request",
        Auth::None,
        Some(&json!({ "name": name })),
    )?;
    if r.handle.len() != 64 || !r.handle.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EngineError::Malformed("the request's handle".into()));
    }
    Ok(r)
}

/// `POST pair/collect {handle}`, once. A 404 means the request is gone: it
/// expired, or the site locked down.
pub fn collect<T: HttpTransport>(
    c: &EngineClient<T>,
    handle: &str,
) -> Result<Collected, EngineError> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        token: Option<String>,
    }
    c.ensure_compatible()?;
    let r: Reply = c.call(
        Method::Post,
        "pair/collect",
        Auth::None,
        Some(&json!({ "handle": handle })),
    )?;
    match (r.id, r.token) {
        (Some(id), Some(token)) if !token.is_empty() => Ok(Collected::Paired(Paired { id, token })),
        _ => Ok(Collected::Waiting),
    }
}

/// How [`wait_for_approval`] waits: real time in the front-ends, a fake
/// clock in tests.
pub trait Waiter {
    /// Wait `d`; `false` when the user cancelled (Ctrl-C, the Cancel button).
    fn wait(&mut self, d: Duration) -> bool;
    /// Time since the request was made.
    fn elapsed(&self) -> Duration;
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    Paired(Paired),
    /// The request's 10 minutes passed, or the engine dropped it (404:
    /// expired or locked down).
    Expired,
    /// The user stopped waiting. The request dies on its own.
    Cancelled,
}

/// The pause before poll `n` (0-based): 2 s, then growing to 5 s. An owner
/// typing a code takes seconds, and 5 s keeps a waiting Manager far under
/// any edge limit.
pub fn poll_delay(n: u32) -> Duration {
    Duration::from_millis((2000 + 1000 * u64::from(n)).min(5000))
}

/// Poll until the owner approves on the primary, the request expires, or
/// the user cancels. Any other error (the site went away, 429) ends it.
pub fn wait_for_approval<T: HttpTransport>(
    c: &EngineClient<T>,
    req: &JoinRequest,
    waiter: &mut dyn Waiter,
) -> Result<Waited, EngineError> {
    let ttl = Duration::from_secs(req.expires_in);
    let mut n = 0;
    loop {
        if waiter.elapsed() >= ttl {
            return Ok(Waited::Expired);
        }
        let pause = poll_delay(n).min(ttl.saturating_sub(waiter.elapsed()));
        if !waiter.wait(pause) {
            return Ok(Waited::Cancelled);
        }
        n += 1;
        match collect(c, &req.handle) {
            Ok(Collected::Paired(p)) => return Ok(Waited::Paired(p)),
            Ok(Collected::Waiting) => {}
            Err(EngineError::NotFound(_)) => return Ok(Waited::Expired),
            Err(e) => return Err(e),
        }
    }
}

/// `GET pairings`.
pub fn managers<T: HttpTransport>(c: &EngineClient<T>) -> Result<Managers, EngineError> {
    c.ensure_compatible()?;
    c.call(Method::Get, "pairings", Auth::Token, None)
}

/// `GET pairings/requests` (primary only, else 403).
pub fn requests<T: HttpTransport>(c: &EngineClient<T>) -> Result<Vec<RequestView>, EngineError> {
    #[derive(Deserialize)]
    struct Reply {
        requests: Vec<RequestView>,
    }
    c.ensure_compatible()?;
    let r: Reply = c.call(Method::Get, "pairings/requests", Auth::Token, None)?;
    Ok(r.requests)
}

/// `POST pairings/approve {code}` (primary only): the code the new Manager
/// shows, as the owner typed it. Returns the approved Manager's name. 404
/// when no pending request has that code.
pub fn approve<T: HttpTransport>(c: &EngineClient<T>, typed: &str) -> Result<String, EngineError> {
    #[derive(Deserialize)]
    struct Reply {
        approved: String,
    }
    let typed = typed.trim();
    if typed.is_empty() {
        return Err(EngineError::BadAddress("type the code first".into()));
    }
    c.ensure_compatible()?;
    let r: Reply = c.call(
        Method::Post,
        "pairings/approve",
        Auth::Token,
        Some(&json!({ "code": typed })),
    )?;
    Ok(r.approved)
}

/// A pairing id as the engine makes them (12 of `[0-9a-z]`): checked here so
/// nothing else is ever put into a path.
pub fn valid_pairing_id(id: &str) -> bool {
    id.len() == 12
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// `DELETE pairings/<id>`: a junior pairing (403 for a senior one).
pub fn revoke<T: HttpTransport>(c: &EngineClient<T>, id: &str) -> Result<(), EngineError> {
    if !valid_pairing_id(id) {
        return Err(EngineError::BadAddress(format!(
            "`{id}` is not a pairing id (12 lowercase letters and digits, from `engine managers`)"
        )));
    }
    c.ensure_compatible()?;
    let _: serde_json::Value =
        c.call(Method::Delete, &format!("pairings/{id}"), Auth::Token, None)?;
    Ok(())
}

/// `DELETE pairings/self`: this Manager leaves the site.
pub fn forget_self<T: HttpTransport>(c: &EngineClient<T>) -> Result<(), EngineError> {
    c.ensure_compatible()?;
    let _: serde_json::Value = c.call(Method::Delete, "pairings/self", Auth::Token, None)?;
    Ok(())
}

/// Whether `e` says this Manager's pairing is gone (revoked, idled out, a
/// lockdown by a senior one): its token answers 401.
pub fn is_removed(e: &EngineError) -> bool {
    matches!(e, EngineError::Unauthorized(_))
}

/// What a front-end says when the token stopped working.
pub fn removed_note(site: &Site) -> String {
    let at = site
        .engine_url()
        .map(crate::sites::url_host)
        .unwrap_or_default();
    format!(
        "this Manager was removed from {} ({at}): its pairing was revoked, expired or ended by \
         a lockdown. Forget it here, then join again if you should still have access",
        site.label
    )
}

/// The engine base of an engine-paired site.
pub fn engine_url(site: &Site) -> Result<EngineUrl> {
    let Some(url) = site.engine_url() else {
        bail!("site `{}` is not paired with an engine", site.slug);
    };
    Ok(EngineUrl::from_domain(url)?)
}

/// A client for an engine-paired `site` with its token from `store`. No
/// token is [`EngineError::NotPaired`] at the first token call.
pub fn client_with<T: HttpTransport>(
    site: &Site,
    store: &dyn SecretStore,
    transport: T,
) -> Result<EngineClient<T>> {
    let base = engine_url(site)?;
    let c = EngineClient::new(base, transport);
    Ok(match crate::engine_client::load_token(store, &site.slug)? {
        Some(t) => c.with_token(t),
        None => c,
    })
}

/// Keep a new pairing's token for `slug` (keychain only, never on disk).
pub fn keep(store: &dyn SecretStore, slug: &str, paired: &Paired) -> Result<()> {
    save_token(store, slug, &paired.token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_client::{load_token, Request, Response};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Fake {
        replies: RefCell<VecDeque<(u16, String)>>,
        sent: RefCell<Vec<Request>>,
    }

    impl Fake {
        fn with(replies: &[(u16, &str)]) -> Self {
            let f = Fake::default();
            for (s, b) in replies {
                f.replies.borrow_mut().push_back((*s, b.to_string()));
            }
            f
        }
        fn sent(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .map(|r| {
                    format!(
                        "{} {}{}",
                        r.method.as_str(),
                        r.url
                            .trim_start_matches("https://cms.example.com/_atelier/engine/v1/"),
                        r.body
                            .as_ref()
                            .map(|b| format!(" {}", String::from_utf8_lossy(b)))
                            .unwrap_or_default()
                    )
                })
                .collect()
        }
    }

    impl HttpTransport for Fake {
        fn send(&self, req: &Request) -> Result<Response, EngineError> {
            self.sent.borrow_mut().push(req.clone());
            let (status, body) = self
                .replies
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| EngineError::Transport("no reply scripted".into()))?;
            Ok(Response {
                status,
                body: body.into_bytes(),
            })
        }
    }

    const V1: (u16, &str) = (
        200,
        r#"{"api":"1.0.0","engine":"0.12.1","experimental":true}"#,
    );
    const HANDLE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn anon(f: &Fake) -> EngineClient<&Fake> {
        EngineClient::new(EngineUrl::from_domain("cms.example.com").unwrap(), f)
    }
    fn authed(f: &Fake) -> EngineClient<&Fake> {
        anon(f).with_token("aet_tok")
    }

    #[test]
    fn the_install_code_pairs_and_the_token_goes_to_the_keychain_only() {
        let f = Fake::with(&[
            V1,
            (
                201,
                r#"{"id":"abc123def456","token":"aet_new","api":"1.0.0"}"#,
            ),
        ]);
        let p = pair(&anon(&f), " ABCD-EFGH-JKMN-PQRS ", "Shibin's Mac").unwrap();
        assert_eq!(p.id, "abc123def456");
        assert!(!format!("{p:?}").contains("aet_new"), "Debug redacts");
        assert_eq!(
            f.sent(),
            [
                "GET version",
                r#"POST pair {"code":"ABCD-EFGH-JKMN-PQRS","name":"Shibin's Mac"}"#
            ]
        );
        assert_eq!(f.sent.borrow()[1].bearer, None, "no token to pair");
        let store = crate::ssh_key::tests::Memory::default();
        keep(&store, "vps", &p).unwrap();
        assert_eq!(
            load_token(&store, "vps").unwrap().as_deref(),
            Some("aet_new")
        );
    }

    #[test]
    fn a_wrong_or_used_install_code_is_refused() {
        for status in [401, 403] {
            let f = Fake::with(&[V1, (status, r#"{"error":"wrong install code"}"#)]);
            let e = pair(&anon(&f), "AAAA-BBBB-CCCC-DDDD", "m").unwrap_err();
            assert!(
                matches!(e, EngineError::Unauthorized(_) | EngineError::Forbidden(_)),
                "{e:?}"
            );
            assert!(e.to_string().contains("wrong install code"), "{e}");
        }
        // Locked down: pairing is closed (403).
        let f = Fake::with(&[
            V1,
            (
                403,
                r#"{"error":"this site is locked down: pairing is closed"}"#,
            ),
        ]);
        let e = pair(&anon(&f), "x", "m").unwrap_err();
        assert!(e.to_string().contains("locked down"), "{e}");
    }

    #[test]
    fn a_bad_name_costs_no_request() {
        let f = Fake::default();
        for bad in ["", "   ", &"x".repeat(65), "a\u{202E}b", "a\nb"] {
            assert!(pair(&anon(&f), "c", bad).is_err(), "{bad:?}");
            assert!(request_join(&anon(&f), bad).is_err(), "{bad:?}");
        }
        assert!(f.sent().is_empty());
        assert_eq!(
            clean_name("  Shibin\u{200B}'s Mac \n").as_deref(),
            Some("Shibin's Mac")
        );
        assert_eq!(clean_name("\u{200B}"), None);
        assert_eq!(clean_name(&"y".repeat(80)).unwrap().len(), 64);
        assert!(valid_name(&default_name()));
    }

    #[test]
    fn a_newer_engine_refuses_before_any_code_is_sent() {
        let f = Fake::with(&[(200, r#"{"api":"2.0.0"}"#)]);
        let e = pair(&anon(&f), "AAAA-BBBB-CCCC-DDDD", "m").unwrap_err();
        assert!(matches!(e, EngineError::ManagerTooOld { .. }));
        assert_eq!(f.sent(), ["GET version"]);
    }

    #[test]
    fn plain_http_and_redirects_are_refused() {
        assert!(matches!(
            EngineUrl::from_domain("http://cms.example.com"),
            Err(EngineError::Insecure { .. })
        ));
        let f = Fake::with(&[V1, (302, "")]);
        let e = pair(&anon(&f), "c", "m").unwrap_err();
        assert_eq!(e, EngineError::Redirect { status: 302 });
        assert_eq!(f.sent().len(), 2, "nothing followed");
    }

    /// A clock that moves only when waited on; cancels after `cancel_after`
    /// waits when set.
    struct Clock {
        now: Cell<Duration>,
        waits: Cell<u32>,
        cancel_after: Option<u32>,
    }
    impl Clock {
        fn new(cancel_after: Option<u32>) -> Self {
            Clock {
                now: Cell::new(Duration::ZERO),
                waits: Cell::new(0),
                cancel_after,
            }
        }
    }
    impl Waiter for Clock {
        fn wait(&mut self, d: Duration) -> bool {
            if self.cancel_after == Some(self.waits.get()) {
                return false;
            }
            self.waits.set(self.waits.get() + 1);
            self.now.set(self.now.get() + d);
            true
        }
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
    }

    fn join_req(expires_in: u64) -> JoinRequest {
        JoinRequest {
            code: "ABCD-EFGH".into(),
            handle: HANDLE.into(),
            expires_in,
        }
    }

    #[test]
    fn join_asks_then_waits_through_202s_until_approved() {
        let f = Fake::with(&[
            V1,
            (
                201,
                &format!(r#"{{"code":"ABCD-EFGH","handle":"{HANDLE}","expires_in":600}}"#),
            ),
            (202, r#"{"approved":false}"#),
            (202, r#"{"approved":false}"#),
            (
                201,
                r#"{"id":"b00000000001","token":"aet_b","api":"1.0.0"}"#,
            ),
        ]);
        let c = anon(&f);
        let req = request_join(&c, "Laptop B").unwrap();
        assert_eq!(req.code, "ABCD-EFGH");
        assert!(
            !format!("{req:?}").contains(HANDLE),
            "Debug redacts the handle"
        );
        let mut clock = Clock::new(None);
        match wait_for_approval(&c, &req, &mut clock).unwrap() {
            Waited::Paired(p) => assert_eq!(p.token, "aet_b"),
            other => panic!("{other:?}"),
        }
        assert_eq!(clock.waits.get(), 3);
        assert_eq!(clock.now.get(), Duration::from_secs(2 + 3 + 4), "backoff");
        let sent = f.sent();
        assert_eq!(sent[1], r#"POST pair/request {"name":"Laptop B"}"#);
        assert!(sent[2..].iter().all(|s| s.starts_with("POST pair/collect")));
        assert!(f.sent.borrow().iter().all(|r| r.bearer.is_none()));
    }

    #[test]
    fn an_unapproved_request_ends_at_its_expiry() {
        let mut replies = vec![V1];
        replies.extend(std::iter::repeat_n((202, r#"{"approved":false}"#), 400));
        let f = Fake::with(&replies);
        let mut clock = Clock::new(None);
        let got = wait_for_approval(&anon(&f), &join_req(600), &mut clock).unwrap();
        assert_eq!(got, Waited::Expired);
        assert_eq!(
            clock.now.get(),
            Duration::from_secs(600),
            "never past the expiry"
        );
        let polls = f.sent().len() - 1;
        assert!((118..=125).contains(&polls), "about every 5 s: {polls}");
    }

    #[test]
    fn a_dropped_request_is_expired_and_cancel_stops_at_once() {
        let f = Fake::with(&[
            V1,
            (
                404,
                r#"{"error":"no such request: it expired, or the site locked down"}"#,
            ),
        ]);
        let got = wait_for_approval(&anon(&f), &join_req(600), &mut Clock::new(None)).unwrap();
        assert_eq!(got, Waited::Expired);

        let f = Fake::default();
        let got = wait_for_approval(&anon(&f), &join_req(600), &mut Clock::new(Some(0))).unwrap();
        assert_eq!(got, Waited::Cancelled);
        assert!(f.sent().is_empty(), "cancelled before the first poll");

        let f = Fake::with(&[V1, (429, r#"{"error":"slow down"}"#)]);
        let e = wait_for_approval(&anon(&f), &join_req(600), &mut Clock::new(None)).unwrap_err();
        assert!(matches!(e, EngineError::TooManyRequests(_)));
    }

    #[test]
    fn the_primary_lists_requests_without_codes_and_approves_a_typed_one() {
        let f = Fake::with(&[
            V1,
            (
                200,
                r#"{"requests":[{"name":"Laptop B","ip":"203.0.113.9","created":5,"approved":false}]}"#,
            ),
            (200, r#"{"approved":"Laptop B"}"#),
        ]);
        let c = authed(&f);
        let r = requests(&c).unwrap();
        assert_eq!(r[0].name, "Laptop B");
        assert_eq!(approve(&c, "  abcd-efgh ").unwrap(), "Laptop B");
        assert_eq!(f.sent()[2], r#"POST pairings/approve {"code":"abcd-efgh"}"#);
        assert!(f.sent.borrow()[1..]
            .iter()
            .all(|r| r.bearer.as_deref() == Some("aet_tok")));
        // An empty code is never sent.
        assert!(approve(&c, "  ").is_err());
        assert_eq!(f.sent().len(), 3);
    }

    #[test]
    fn a_non_primary_is_refused_and_a_wrong_code_is_404() {
        let f = Fake::with(&[
            V1,
            (403, r#"{"error":"only the primary pairing can do that"}"#),
        ]);
        let e = approve(&authed(&f), "ABCD-EFGH").unwrap_err();
        assert!(matches!(e, EngineError::Forbidden(m) if m.contains("primary")));
        let f = Fake::with(&[
            V1,
            (403, r#"{"error":"only the primary pairing can do that"}"#),
        ]);
        assert!(matches!(
            requests(&authed(&f)),
            Err(EngineError::Forbidden(_))
        ));
        let f = Fake::with(&[V1, (404, r#"{"error":"no pending request has that code"}"#)]);
        assert!(matches!(
            approve(&authed(&f), "ZZZZ-ZZZZ"),
            Err(EngineError::NotFound(_))
        ));
    }

    #[test]
    fn managers_revoke_and_forget() {
        let f = Fake::with(&[
            V1,
            (
                200,
                r#"{"self":"a00000000001","pairings":[
                {"id":"a00000000001","name":"A","created":1,"last_used":9,"via":"install code","primary":true},
                {"id":"b00000000002","name":"B","created":2,"last_used":3,"via":"approved by a00000000001","primary":false}]}"#,
            ),
            (200, r#"{"revoked":"b00000000002"}"#),
            (200, r#"{"revoked":"a00000000001"}"#),
        ]);
        let c = authed(&f);
        let m = managers(&c).unwrap();
        assert!(m.is_primary());
        assert_eq!(m.pairings.len(), 2);
        revoke(&c, "b00000000002").unwrap();
        forget_self(&c).unwrap();
        assert_eq!(
            f.sent()[1..],
            [
                "GET pairings",
                "DELETE pairings/b00000000002",
                "DELETE pairings/self"
            ]
        );
        // Nothing but an id shape reaches a path.
        for bad in [
            "../self",
            "self",
            "B00000000002",
            "a0000000000",
            "a00000000001/x",
        ] {
            assert!(
                matches!(revoke(&c, bad), Err(EngineError::BadAddress(_))),
                "{bad}"
            );
        }
        assert_eq!(f.sent().len(), 4);
    }

    #[test]
    fn a_revoked_token_reads_as_removed_and_lockdown_as_409() {
        let f = Fake::with(&[V1, (401, r#"{"error":"pair this Manager first"}"#)]);
        let e = managers(&authed(&f)).unwrap_err();
        assert!(is_removed(&e));
        let site = Site::new_engine(
            std::path::Path::new("/h"),
            "vps",
            Some("VPS"),
            "https://cms.example.com",
        );
        assert!(removed_note(&site).contains("this Manager was removed from VPS (cms.example.com)"));

        let f = Fake::with(&[V1, (409, r#"{"error":"locked down: unlock first"}"#)]);
        let e = authed(&f)
            .act(crate::engine_client::Verb::Freeze)
            .unwrap_err();
        assert!(matches!(e, EngineError::Conflict(_)) && !is_removed(&e));
        // A senior pairing can't be revoked.
        let f = Fake::with(&[
            V1,
            (
                403,
                r#"{"error":"a pairing can't revoke one senior to itself"}"#,
            ),
        ]);
        assert!(matches!(
            revoke(&authed(&f), "a00000000001"),
            Err(EngineError::Forbidden(_))
        ));
    }

    #[test]
    fn the_client_for_a_site_carries_its_own_token() {
        let store = crate::ssh_key::tests::Memory::default();
        let site = Site::new_engine(
            std::path::Path::new("/h"),
            "vps",
            None,
            "https://cms.example.com",
        );
        let f = Fake::with(&[V1]);
        let c = client_with(&site, &store, &f).unwrap();
        assert_eq!(c.status().unwrap_err(), EngineError::NotPaired);
        save_token(&store, "vps", "aet_v").unwrap();
        let f = Fake::with(&[
            V1,
            (
                200,
                r#"{"api":"1.0.0","app":"running","db":"running","lockdown":false}"#,
            ),
        ]);
        let c = client_with(&site, &store, &f).unwrap();
        assert_eq!(c.status().unwrap().app, "running");
        assert_eq!(f.sent.borrow()[1].bearer.as_deref(), Some("aet_v"));
        let ssh = Site::new_remote(
            std::path::Path::new("/h"),
            "p",
            None,
            "h",
            None,
            crate::KeyMode::Ssh,
        );
        assert!(client_with(&ssh, &store, &f).is_err());
    }
}
