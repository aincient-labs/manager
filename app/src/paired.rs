//! Sites paired with their engine, without SSH (M4, E-02), in the window.
//! EXPERIMENTAL (DECISIONS 0483): every command refuses unless the user
//! turned on Experimental features.
//!
//! The logic is [`aincient_core::pairing`]; this keeps one join in flight
//! for the waiting screen and maps the engine's answers to what the window
//! shows: a [`Status`] for the home screen, a [`remote::SiteState`] for the
//! switcher, the Managers panel. A refused token reads "this Manager was
//! removed from …", which the window recognises and offers to forget.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aincient_core::engine_admin::EngineInfo;
use aincient_core::engine_client::{EngineClient, EngineError, EngineStatus, Https, Verb};
use aincient_core::pairing::{self, Collected, JoinRequest, Managers, RequestView};
use aincient_core::{remote, sites, ssh_key, ChangeGuard, Channel, Site, Status};
use serde::Serialize;

/// What the window shows for a refused token; it looks for this phrase.
pub const REMOVED: &str = "this Manager was removed from";

/// An engine error in words for the window.
pub fn explain(site: &Site, e: EngineError) -> String {
    if pairing::is_removed(&e) {
        return format!("{}.", capitalise(&pairing::removed_note(site)));
    }
    if matches!(e, EngineError::NotPaired) {
        return format!(
            "{REMOVED} {}: there is no pairing token for it in the keychain. Forget it here, \
             then join again.",
            site.label
        );
    }
    capitalise(&e.to_string())
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// A client for `site` with its keychain token.
pub fn client(site: &Site) -> Result<EngineClient<Https>, String> {
    pairing::client_with(site, &ssh_key::Keychain, Https::new()).map_err(|e| format!("{e:#}"))
}

/// The home screen's status from the engine's.
pub fn status(site: &Site) -> Result<Status, String> {
    let st = client(site)?.status().map_err(|e| explain(site, e))?;
    Ok(to_status(site, &st))
}

pub fn to_status(site: &Site, st: &EngineStatus) -> Status {
    let url = site.engine_url().unwrap_or_default().to_string();
    let running = st.app == "running";
    Status {
        installed: true,
        running,
        reachable: running,
        console_url: format!("{url}/atelier"),
        site_url: format!("{url}/"),
        image: String::new(),
        channel: Channel::Stable,
        image_digest: None,
        version: st.version.clone(),
        site_id: None,
        engine: Some(EngineInfo {
            enabled: true,
            container: None,
            running: true,
            state: (!st.state.is_empty()).then(|| st.state.clone()),
        }),
    }
}

/// The D-07 guard: live while its app runs.
pub fn guard(site: &Site) -> ChangeGuard {
    let live = client(site)
        .ok()
        .and_then(|c| c.status().ok())
        .map(|st| st.app == "running" && !st.lockdown)
        .unwrap_or(true);
    ChangeGuard::for_site(site, live)
}

/// The switcher's state.
pub fn probe(site: &Site) -> remote::SiteState {
    match client(site).and_then(|c| c.status().map_err(|e| explain(site, e))) {
        Ok(st) if st.app == "running" => remote::SiteState::Running { version: None },
        Ok(st) if st.lockdown || st.state == "locked" => remote::SiteState::Locked,
        Ok(st) if st.state == "resting" => remote::SiteState::Resting,
        Ok(st) if st.state == "waking" => remote::SiteState::Waking,
        Ok(_) => remote::SiteState::Stopped,
        Err(reason) => remote::SiteState::Unreachable { reason },
    }
}

/// One lifecycle verb; returns what to say.
pub fn act(site: &Site, verb: Verb) -> Result<String, String> {
    let c = client(site)?;
    let acted = c.act(verb).map_err(|e| explain(site, e))?;
    Ok(match (verb, acted.job) {
        (Verb::Unlock, _) => "Unlocked: the site is resting.".to_string(),
        (_, Some(job)) => format!("{job} started on the engine."),
        (_, None) => "Done.".to_string(),
    })
}

/// The engine part of the Experimental card for a paired site.
#[derive(Serialize, Default)]
pub struct PairedEngine {
    pub lockdown: bool,
    pub api: String,
    /// `resting`, `waking`, `awake`, `locked` or `stopped` (M5).
    pub state: String,
    pub wake_code_set: bool,
}

pub fn engine(site: &Site) -> Result<PairedEngine, String> {
    let st = client(site)?.status().map_err(|e| explain(site, e))?;
    Ok(PairedEngine {
        lockdown: st.lockdown,
        api: st.api,
        state: st.state,
        wake_code_set: st.wake_code_set,
    })
}

/// Rotate (returns the new code, shown once) or revoke the wake code (m5-3).
pub fn wake_code(site: &Site, revoke: bool) -> Result<Option<String>, String> {
    let c = client(site)?;
    if revoke {
        c.revoke_wake_code().map_err(|e| explain(site, e))?;
        return Ok(None);
    }
    c.rotate_wake_code().map(Some).map_err(|e| explain(site, e))
}

/// The Managers panel: every pairing, and for the primary the waiting
/// requests (name and address; never a code).
#[derive(Serialize)]
pub struct ManagersView {
    #[serde(flatten)]
    pub managers: Managers,
    pub primary: bool,
    pub requests: Vec<RequestView>,
}

pub fn managers(site: &Site) -> Result<ManagersView, String> {
    let c = client(site)?;
    let managers = pairing::managers(&c).map_err(|e| explain(site, e))?;
    let primary = managers.is_primary();
    let requests = if primary {
        pairing::requests(&c).map_err(|e| explain(site, e))?
    } else {
        vec![]
    };
    Ok(ManagersView {
        managers,
        primary,
        requests,
    })
}

pub fn approve(site: &Site, typed: &str) -> Result<String, String> {
    match pairing::approve(&client(site)?, typed) {
        Ok(name) => Ok(name),
        Err(EngineError::NotFound(_)) => Err(
            "No waiting Manager shows that code. Check it on the new Manager's screen (it \
             expires after 10 minutes). Nothing was changed."
                .into(),
        ),
        Err(e) => Err(explain(site, e)),
    }
}

pub fn revoke(site: &Site, id: &str) -> Result<(), String> {
    pairing::revoke(&client(site)?, id).map_err(|e| explain(site, e))
}

/// Leave the site: revoke this pairing (already gone is fine), delete the
/// token, drop the entry.
pub fn forget(site: &Site) -> Result<(), String> {
    match client(site).map(|c| pairing::forget_self(&c)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) if pairing::is_removed(&e) || e == EngineError::NotPaired => {}
        Ok(Err(e)) => return Err(explain(site, e)),
        Err(e) => return Err(e),
    }
    aincient_core::engine_client::forget_token(&ssh_key::Keychain, &site.slug)
        .map_err(|e| format!("{e:#}"))?;
    let uh = sites::user_home().map_err(|e| format!("{e:#}"))?;
    sites::remove_site(&uh, &site.slug).map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Check a new site's entry and that its slug is free.
fn entry(slug: &str, label: &str, url: &str) -> Result<Site, String> {
    let uh = sites::user_home().map_err(|e| format!("{e:#}"))?;
    let label = Some(label.trim()).filter(|l| !l.is_empty());
    let site =
        sites::engine_entry(&uh, slug, label, url).map_err(|e| capitalise(&format!("{e:#}")))?;
    let taken = sites::list_sites(&uh)
        .map_err(|e| format!("{e:#}"))?
        .iter()
        .any(|r| r.slug == slug);
    if taken {
        return Err(format!("There's already a site called \"{slug}\"."));
    }
    Ok(site)
}

fn checked_name(name: &str) -> Result<String, String> {
    pairing::clean_name(name)
        .filter(|c| c == name.trim())
        .ok_or_else(|| "The Manager's name: 1 to 64 visible characters.".to_string())
}

/// Add the new site and keep its token — or take the pairing back when the
/// keychain refuses it ([`pairing::keep_or_release`]).
fn register(
    site: Site,
    c: EngineClient<Https>,
    paired: &pairing::Paired,
    primary: bool,
) -> Result<(), String> {
    let uh = sites::user_home().map_err(|e| format!("{e:#}"))?;
    let slug = site.slug.clone();
    sites::add_remote_site(&uh, site, |s| {
        pairing::keep_or_release(&ssh_key::Keychain, c, &s.slug, paired, primary)
    })
    .map_err(|e| format!("{e:#}"))?;
    sites::use_site(&uh, &slug)
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

/// "Add a site → On a server → With an install code" (m4-2).
pub fn add_with_install_code(
    slug: &str,
    label: &str,
    url: &str,
    code: &str,
    name: &str,
) -> Result<(), String> {
    let site = entry(slug, label, url)?;
    let name = checked_name(name)?;
    if code.trim().is_empty() {
        return Err("Type the install code the site's installer printed.".into());
    }
    pairing::check_store(&ssh_key::Keychain, slug).map_err(|e| capitalise(&format!("{e:#}")))?;
    let c = EngineClient::new(
        pairing::engine_url(&site).map_err(|e| format!("{e:#}"))?,
        Https::new(),
    );
    let paired = pairing::pair(&c, code, &name).map_err(|e| match e {
        EngineError::Unauthorized(_) | EngineError::Forbidden(_) => format!(
            "The site refused that install code ({e}). It works once: a used, mistyped or \
             replaced code is refused. A new one is made over SSH."
        ),
        other => capitalise(&other.to_string()),
    })?;
    register(site, c, &paired, true)
}

/// The one join this window waits on.
struct Join {
    site: Site,
    req: JoinRequest,
    started: Instant,
}

/// Shared with the blocking worker that polls.
#[derive(Default, Clone)]
pub struct Joining(Arc<Mutex<Option<Join>>>);

/// What the waiting screen shows.
#[derive(Serialize)]
pub struct JoinView {
    pub code: String,
    pub expires_in: u64,
    pub url: String,
}

/// "Add a site → On a server → Join": ask, keep the handle here, return
/// the code to show.
pub fn join_start(
    joining: &Joining,
    slug: &str,
    label: &str,
    url: &str,
    name: &str,
) -> Result<JoinView, String> {
    let site = entry(slug, label, url)?;
    let name = checked_name(name)?;
    let base = pairing::engine_url(&site).map_err(|e| format!("{e:#}"))?;
    pairing::check_store(&ssh_key::Keychain, slug).map_err(|e| capitalise(&format!("{e:#}")))?;
    let c = EngineClient::new(base.clone(), Https::new());
    let req = pairing::request_join(&c, &name).map_err(|e| match e {
        EngineError::Forbidden(m) => format!("The site isn't taking new Managers: {m}"),
        other => capitalise(&other.to_string()),
    })?;
    let view = JoinView {
        code: req.code.clone(),
        expires_in: req.expires_in,
        url: base.base(),
    };
    *joining.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(Join {
        site,
        req,
        started: Instant::now(),
    });
    Ok(view)
}

/// One poll: `waiting`, `paired` (registered and selected; returns the
/// slug) or `expired`.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Polled {
    Waiting { left: u64 },
    Paired { slug: String },
    Expired,
}

pub fn join_poll(joining: &Joining) -> Result<Polled, String> {
    let mut slot = joining.0.lock().unwrap_or_else(|p| p.into_inner());
    let Some(j) = slot.as_ref() else {
        return Ok(Polled::Expired);
    };
    let ttl = j.req.expires_in;
    let elapsed = j.started.elapsed().as_secs();
    if elapsed >= ttl {
        slot.take();
        return Ok(Polled::Expired);
    }
    let c = EngineClient::new(
        pairing::engine_url(&j.site).map_err(|e| format!("{e:#}"))?,
        Https::new(),
    );
    match pairing::collect(&c, &j.req.handle) {
        Ok(Collected::Waiting) => Ok(Polled::Waiting {
            left: ttl - elapsed,
        }),
        Ok(Collected::Paired(p)) => {
            let j = slot.take().expect("checked above");
            let slug = j.site.slug.clone();
            register(j.site, c, &p, false)?;
            Ok(Polled::Paired { slug })
        }
        Err(EngineError::NotFound(_)) => {
            slot.take();
            Ok(Polled::Expired)
        }
        Err(e) => Err(capitalise(&e.to_string())),
    }
}

pub fn join_cancel(joining: &Joining) {
    joining.0.lock().unwrap_or_else(|p| p.into_inner()).take();
}
