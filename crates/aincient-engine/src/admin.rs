//! What an owner with SSH does without a token, inside the engine container:
//! `docker exec <project>-engine-1 atelier engine admin …`. No token, since
//! whoever can `docker exec` is already root-equivalent. The installer runs
//! `admin install-code` (m4-6). Every change is audited as `ssh`.

use crate::codes;
use crate::inbox::Inbox;
use crate::state::{self, Revoked, Revoker, Store};
use anyhow::{bail, Result};
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
    /// Leave lockdown (doesn't wake the site).
    Unlock,
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
        AdminCommand::Unlock => {
            l.update_flags(|f| f.lockdown = false)?;
            audit("unlock", "ok");
            writeln!(out, "Unlocked. The site stays resting until it is woken.")?;
        }
        AdminCommand::ResumeWake => {
            l.update_flags(|f| f.wake_resumed())?;
            audit("wake/resume", "ok");
            writeln!(out, "Wake by code resumed.")?;
        }
        AdminCommand::Audit { .. } => unreachable!("answered above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    fn admin(store: &Store, inbox: &Inbox, cmd: AdminCommand, stdin: &str) -> Result<String> {
        let mut out = Vec::new();
        run(cmd, store, inbox, &mut stdin.as_bytes(), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
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
