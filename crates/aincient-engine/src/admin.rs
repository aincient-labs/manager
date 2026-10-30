//! What an owner with SSH does without a token, inside the engine container:
//! `docker exec <project>-engine-1 atelier engine admin …`. No token, since
//! whoever can `docker exec` is already root-equivalent. The installer runs
//! `admin install-code` (m4-6). Every change is audited as `ssh`.
//!
//! The Manager drives a site over SSH through these too (m3-7): `status`,
//! `lockdown`, `unlock` and `wake`. Each takes no argument that reaches
//! Docker; the site calls are [`Site`]'s fixed ones.

use crate::codes::{self, now};
use crate::inbox::Inbox;
use crate::site::{service_state, Site};
use crate::state::{self, Revoked, Revoker, Store};
use crate::{API_VERSION, ENGINE_VERSION};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::io::{Read, Write};

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Make a one-time install code and print it once: it pairs the primary
    /// Manager. Replaces any unused one; a new primary leaves the old one as
    /// an ordinary pairing.
    InstallCode,
    /// Managers waiting to be approved (their codes are on their own screens).
    Requests,
    /// Let a waiting Manager in by the code it shows.
    Approve { code: String },
    /// Make a new wake code and print it once (`--set` reads your own from stdin).
    WakeCode {
        #[arg(long)]
        set: bool,
    },
    /// List pairings.
    Pairings,
    /// Revoke a pairing by id.
    Revoke { id: String },
    /// Make a pairing the primary (the owner's), e.g. after re-pairing.
    MakePrimary { id: String },
    /// The engine's state as JSON: its API version (the Manager checks the
    /// major first, m3-3), the site's services, lockdown, the wake pause.
    Status,
    /// Lock down (D-09): set the flag, revoke every pairing but the primary,
    /// drop every pending request, then stop `app` and `db`. Runs nothing of
    /// Drupal's and is never refused; while the flag is set the engine keeps
    /// the site stopped.
    Lockdown,
    /// Leave lockdown (doesn't wake the site).
    Unlock,
    /// Start `db` then `app` (refused while locked down). Doesn't wait for
    /// Drupal to answer.
    Wake,
    /// Resume wake by code after it paused.
    ResumeWake,
    /// The newest audit entries.
    Audit {
        #[arg(short, default_value_t = 50)]
        n: usize,
    },
}

/// Run one admin command against `store`; output goes to `out`, hints to
/// stderr. `stdin` is read only by `wake-code --set`, before the state lock
/// is taken (the engine waits on that lock).
pub fn run(
    cmd: AdminCommand,
    store: &Store,
    inbox: &Inbox,
    site: &dyn Site,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<()> {
    let wake = match &cmd {
        AdminCommand::WakeCode { set: true } => {
            let mut s = String::new();
            stdin.take(4096).read_to_string(&mut s)?;
            let hash = codes::hash_wake_code(&s, false)?;
            Some((None, hash))
        }
        AdminCommand::WakeCode { set: false } => {
            let code = codes::mint_wake_code();
            let hash = codes::hash_wake_code(&code, true)?;
            Some((Some(code), hash))
        }
        _ => None,
    };
    if let AdminCommand::Audit { n } = cmd {
        for e in store.audit.tail(n) {
            writeln!(out, "{e}")?;
        }
        return Ok(());
    }
    if let AdminCommand::Status = cmd {
        // Unreadable state reads as locked down, as the engine treats it.
        let flags = store.lock().and_then(|l| l.flags());
        let states = site.states().unwrap_or(serde_json::Value::Null);
        let v = serde_json::json!({
            "api": API_VERSION,
            "engine": ENGINE_VERSION,
            "app": service_state(&states, "app"),
            "db": service_state(&states, "db"),
            "lockdown": flags.as_ref().map_or(true, |f| f.lockdown),
            "wake_paused": flags.as_ref().is_ok_and(|f| f.wake_paused(now())),
            "wake_code_set": flags.as_ref().is_ok_and(|f| f.wake_hash.is_some()),
        });
        writeln!(out, "{v}")?;
        return Ok(());
    }
    let l = store.lock()?;
    let audit = |verb: &str, outcome: &str| store.audit("ssh", "-", verb, outcome);
    let publish = |l: &state::Locked| -> Result<()> {
        let views = state::views(&l.pairings()?);
        inbox.publish(
            "pairings.json",
            serde_json::json!(views).to_string().as_bytes(),
        );
        Ok(())
    };
    match cmd {
        AdminCommand::InstallCode => {
            let code = codes::mint_install_code();
            l.set_install_code(&code)?;
            audit("install-code", "made");
            writeln!(out, "{code}")?;
            eprintln!("Shown once. Enter it in the Manager to pair it as this site's owner.");
        }
        AdminCommand::Requests => {
            for r in state::request_views(&l.requests()?) {
                writeln!(
                    out,
                    "{:<32}  from {:<15}  asked {}{}",
                    r.name,
                    r.ip,
                    r.created,
                    if r.approved { "  (approved)" } else { "" }
                )?;
            }
        }
        AdminCommand::Approve { code } => {
            if code.len() > codes::CODE_FIELD_MAX {
                bail!("no pending request has that code");
            }
            let Some(r) = l.approve(&code, "ssh")? else {
                bail!("no pending request has that code");
            };
            audit(
                "pairings/approve",
                &format!("ok, name {:?}, asked from {}", r.name, r.ip),
            );
            writeln!(
                out,
                "Approved {}. It is paired once it next checks in.",
                r.name
            )?;
        }
        AdminCommand::WakeCode { set } => {
            let (code, hash) = wake.expect("hashed above");
            l.update_flags(|f| {
                f.wake_hash = Some(hash);
                f.wake_generated = !set;
                f.wake_resumed();
            })?;
            audit("wake-code", if set { "set" } else { "rotated" });
            if let Some(code) = code {
                writeln!(out, "{code}")?;
                eprintln!("Shown once. The engine keeps only its hash.");
            }
        }
        AdminCommand::Pairings => {
            for p in state::views(&l.pairings()?) {
                writeln!(
                    out,
                    "{}  {:<32}  created {}  last used {}{}",
                    p.id,
                    p.name,
                    p.created,
                    p.last_used,
                    if p.primary { "  (primary)" } else { "" }
                )?;
            }
        }
        AdminCommand::Revoke { id } => {
            if !codes::is_pairing_id(&id) || l.revoke(Revoker::Ssh, &id)? != Revoked::Yes {
                bail!("no pairing {id}");
            }
            audit(&format!("revoke {id}"), "ok");
            publish(&l)?;
            writeln!(out, "Revoked {id}.")?;
        }
        AdminCommand::MakePrimary { id } => {
            if !codes::is_pairing_id(&id) || !l.make_primary(&id)? {
                bail!("no pairing {id}");
            }
            audit(&format!("make-primary {id}"), "ok");
            publish(&l)?;
            writeln!(out, "{id} is now the primary pairing.")?;
        }
        AdminCommand::Lockdown => {
            // SSH stands in for the primary: whatever is junior to it may be
            // the attacker's. One change under the lock, as the API's.
            let gone = l.lock_down("ssh")?;
            publish(&l)?;
            drop(l);
            audit(
                "lockdown",
                &format!("flag set; revoked junior pairings: {gone:?}"),
            );
            // Stop first, nothing of Drupal's. If this fails the flag stays
            // set and the engine stops the site within its next check.
            site.stop_app_and_db(&|| false).context(
                "the lockdown flag is set, but stopping app and db failed; the engine \
                 retries every 30 s",
            )?;
            audit("lockdown", "app and db stopped");
            writeln!(
                out,
                "Locked down: app and db stopped, wake and pairing closed, {} pairing(s) \
                 revoked. Unlock with the Manager or `atelier engine admin unlock`.",
                gone.len()
            )?;
        }
        AdminCommand::Unlock => {
            l.update_flags(|f| f.lockdown = false)?;
            audit("unlock", "ok");
            writeln!(out, "Unlocked. The site stays resting until it is woken.")?;
        }
        AdminCommand::Wake => {
            if l.flags()?.lockdown {
                audit("wake", "refused: locked down");
                bail!("locked down: unlock first");
            }
            drop(l);
            site.start_db_and_app(&|| false)?;
            audit("wake", "db and app started");
            writeln!(out, "Woken: db and app started.")?;
        }
        AdminCommand::ResumeWake => {
            l.update_flags(|f| f.wake_resumed())?;
            audit("wake/resume", "ok");
            writeln!(out, "Wake by code resumed.")?;
        }
        AdminCommand::Audit { .. } | AdminCommand::Status => unreachable!("answered above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    /// A site that records what was asked of it.
    #[derive(Default)]
    struct Recorder {
        up: std::sync::atomic::AtomicBool,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Site for Recorder {
        fn states(&self) -> Result<serde_json::Value> {
            let s = if self.up.load(std::sync::atomic::Ordering::SeqCst) {
                "running"
            } else {
                "exited"
            };
            Ok(serde_json::json!([{"Service": "app", "State": s}, {"Service": "db", "State": s}]))
        }
        fn stop_app_and_db(&self, _: crate::site::Cut) -> Result<()> {
            self.calls.lock().unwrap().push("stop");
            self.up.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn start_db_and_app(&self, _: crate::site::Cut) -> Result<()> {
            self.calls.lock().unwrap().push("start");
            self.up.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn drush(&self, _: crate::site::Drush, _: crate::site::Cut) -> Result<()> {
            panic!("admin runs nothing of Drupal's")
        }
        fn answers(&self) -> bool {
            true
        }
    }

    fn admin(store: &Store, inbox: &Inbox, cmd: AdminCommand, stdin: &str) -> Result<String> {
        admin_on(store, inbox, &Recorder::default(), cmd, stdin)
    }

    fn admin_on(
        store: &Store,
        inbox: &Inbox,
        site: &dyn Site,
        cmd: AdminCommand,
        stdin: &str,
    ) -> Result<String> {
        let mut out = Vec::new();
        run(cmd, store, inbox, site, &mut stdin.as_bytes(), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn status_reports_the_api_version_first_and_the_sites_services() {
        let d = TempDir::new();
        let store = Store::at(d.0.join("state")).unwrap();
        let inbox = Inbox::at(d.0.join("in"), d.0.join("out"));
        let site = Recorder::default();
        site.up.store(true, std::sync::atomic::Ordering::SeqCst);
        let out = admin_on(&store, &inbox, &site, AdminCommand::Status, "").unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["api"], API_VERSION);
        assert_eq!(v["app"], "running");
        assert_eq!(v["db"], "running");
        assert_eq!(v["lockdown"], false);
        assert!(
            site.calls.lock().unwrap().is_empty(),
            "status changes nothing"
        );
    }

    #[test]
    fn ssh_lockdown_keeps_the_primary_revokes_the_rest_and_stops_the_site() {
        let d = TempDir::new();
        let store = Store::at(d.0.join("state")).unwrap();
        let inbox = Inbox::at(d.0.join("in"), d.0.join("out"));
        std::fs::create_dir_all(&inbox.out).unwrap();
        let (owner, other) = {
            let l = store.lock().unwrap();
            (
                crate::state::tests::primary(&l),
                crate::state::tests::add(&l, "b"),
            )
        };
        let site = Recorder::default();
        site.up.store(true, std::sync::atomic::Ordering::SeqCst);
        admin_on(&store, &inbox, &site, AdminCommand::Lockdown, "").unwrap();
        assert_eq!(*site.calls.lock().unwrap(), ["stop"]);
        let l = store.lock().unwrap();
        assert!(l.flags().unwrap().lockdown);
        let ids: Vec<String> = l.pairings().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, [owner.id.clone()]);
        assert!(!ids.contains(&other.id));
        drop(l);
        // Wake is refused while locked down; unlock lifts it, wake starts.
        let e = admin_on(&store, &inbox, &site, AdminCommand::Wake, "").unwrap_err();
        assert!(e.to_string().contains("locked down"));
        assert_eq!(*site.calls.lock().unwrap(), ["stop"]);
        let st = admin_on(&store, &inbox, &site, AdminCommand::Status, "").unwrap();
        assert!(st.contains("\"lockdown\":true"), "{st}");
        admin_on(&store, &inbox, &site, AdminCommand::Unlock, "").unwrap();
        admin_on(&store, &inbox, &site, AdminCommand::Wake, "").unwrap();
        assert_eq!(*site.calls.lock().unwrap(), ["stop", "start"]);
        let log = admin(&store, &inbox, AdminCommand::Audit { n: 20 }, "").unwrap();
        assert!(log.contains("lockdown") && log.contains("wake"), "{log}");
    }

    #[test]
    fn ssh_makes_codes_shown_once_and_stored_as_hashes() {
        let d = TempDir::new();
        let store = Store::at(d.0.join("state")).unwrap();
        let inbox = Inbox::at(d.0.join("in"), d.0.join("out"));
        let code = admin(&store, &inbox, AdminCommand::InstallCode, "").unwrap();
        let code = code.trim();
        assert_eq!(code.len(), 19);
        let wake = admin(&store, &inbox, AdminCommand::WakeCode { set: false }, "").unwrap();
        let on_disk = std::fs::read_to_string(d.0.join("state/engine.json")).unwrap();
        assert!(!on_disk.contains(&codes::normalize_code(code)));
        assert!(!on_disk.contains(&codes::normalize_code(wake.trim())));
        let f = store.lock().unwrap().flags().unwrap();
        assert!(f.wake_generated);
        let own = admin(
            &store,
            &inbox,
            AdminCommand::WakeCode { set: true },
            "My Own Wake Code\n",
        )
        .unwrap();
        assert_eq!(own, "", "an owner's own code isn't echoed");
        assert!(!store.lock().unwrap().flags().unwrap().wake_generated);
        assert!(admin(
            &store,
            &inbox,
            AdminCommand::WakeCode { set: true },
            "short"
        )
        .is_err());
        let log = admin(&store, &inbox, AdminCommand::Audit { n: 10 }, "").unwrap();
        assert!(log.contains("install-code") && log.contains("\"set\""));
    }

    #[test]
    fn ssh_revoke_and_make_primary_publish_the_list_at_once() {
        let d = TempDir::new();
        let store = Store::at(d.0.join("state")).unwrap();
        let inbox = Inbox::at(d.0.join("in"), d.0.join("out"));
        std::fs::create_dir_all(&inbox.out).unwrap();
        let (a, b) = {
            let l = store.lock().unwrap();
            (
                crate::state::tests::primary(&l),
                crate::state::tests::add(&l, "b"),
            )
        };
        admin(
            &store,
            &inbox,
            AdminCommand::MakePrimary { id: b.id.clone() },
            "",
        )
        .unwrap();
        admin(
            &store,
            &inbox,
            AdminCommand::Revoke { id: a.id.clone() },
            "",
        )
        .unwrap();
        let list = std::fs::read_to_string(inbox.out.join("pairings.json")).unwrap();
        assert!(list.contains(&b.id) && !list.contains(&a.id));
        assert!(!list.contains("token"), "no hashes in the console's list");
        assert!(admin(
            &store,
            &inbox,
            AdminCommand::Revoke { id: "../x".into() },
            ""
        )
        .is_err());
        assert!(admin(
            &store,
            &inbox,
            AdminCommand::Approve {
                code: "ABCD-EFGH".into()
            },
            ""
        )
        .is_err());
    }
}
