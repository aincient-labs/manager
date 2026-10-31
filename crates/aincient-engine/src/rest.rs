//! Resting mode (plan M5): the transitions the API's jobs and the owner's
//! `admin` commands share, so both paths keep the same order.
//!
//! - **rest** (D-11, D-12, m5-1): freeze first (a fresh snapshot), then mark
//!   the site resting for edge, then stop `app` and `db`. A failed freeze
//!   stops nothing and leaves the mark off: the site stays awake and the
//!   error says why.
//! - **wake** (D-10, m5-3): refused while locked down; clear the mark first
//!   (edge then proxies, showing "starting" while Drupal boots), then start
//!   `db` and `app`. A wake that fails goes back to resting.
//! - **lockdown** (D-09) is resting with wake disabled: [`crate::state`]'s
//!   `lock_down` sets both flags; unlock clears only `lockdown`, so the site
//!   is resting again with wake open.
//!
//! The mark is two files in the engine's `engine-signal` volume, which edge
//! mounts read-only and `app` not at all ([`Inbox::signal_edge`]): nothing a
//! request or Drupal does can switch the resting copy on or off.

use crate::codes::now;
use crate::inbox::Inbox;
use crate::site::{service_state, Cut, Drush, Site};
use crate::state::{Flags, Store};
use anyhow::{bail, Context, Result};

/// Make edge's markers match the flags. Unreadable state is shown as locked,
/// as the engine treats it.
pub fn sync_edge(store: &Store, inbox: &Inbox) {
    let (resting, locked) = store
        .lock()
        .and_then(|l| l.flags())
        .map_or((true, true), |f| (f.resting, f.lockdown));
    if let Err(e) = inbox.signal_edge(resting, locked) {
        eprintln!("engine: couldn't update edge's resting markers: {e}");
    }
}

/// Rest: freeze, mark, stop. A site whose `app` isn't running (stopped by
/// hand) has nothing to freeze: it is marked and stopped as it is, and edge
/// serves the last snapshot.
pub fn rest(site: &dyn Site, store: &Store, inbox: &Inbox, cut: Cut) -> Result<()> {
    if store.lock()?.flags()?.lockdown {
        bail!("locked down: unlock first");
    }
    let states = site.states().unwrap_or(serde_json::Value::Null);
    if service_state(&states, "app") == "running" {
        site.drush(Drush::Freeze, cut)
            .context("the freeze failed, so the site stays awake")?;
    }
    store.lock()?.update_flags(|f| {
        f.resting = true;
        f.woken_at = None;
    })?;
    sync_edge(store, inbox);
    site.stop_app_and_db(cut)
}

/// The first half of a wake: refused while locked down, then the mark comes
/// off and the idle timer starts counting.
pub fn begin_wake(store: &Store, inbox: &Inbox) -> Result<()> {
    let at = now();
    let ok = store.lock()?.update_flags(|f| {
        if f.lockdown {
            return false;
        }
        f.resting = false;
        f.woken_at = Some(at);
        true
    })?;
    if !ok {
        bail!("locked down: unlock first");
    }
    sync_edge(store, inbox);
    Ok(())
}

/// A wake that didn't come up: back to resting, `app` and `db` stopped
/// (best effort; the engine's periodic check notices a running `app`).
pub fn wake_failed(site: &dyn Site, store: &Store, inbox: &Inbox) {
    let _ = store.lock().and_then(|l| {
        l.update_flags(|f| {
            f.resting = true;
            f.woken_at = None;
        })
    });
    sync_edge(store, inbox);
    if let Err(e) = site.stop_app_and_db(&|| false) {
        eprintln!("engine: stopping after a failed wake: {e:#}");
    }
}

/// The site's state in one word, for `status` (m5, CLI and GUI): `locked`,
/// `waking`, `resting`, `awake` or `stopped` (not resting, `app` not
/// running: stopped by hand).
pub fn state_word(f: &Flags, waking: bool, app: &str) -> &'static str {
    if f.lockdown {
        "locked"
    } else if waking {
        "waking"
    } else if f.resting {
        "resting"
    } else if app == "running" {
        "awake"
    } else {
        "stopped"
    }
}
