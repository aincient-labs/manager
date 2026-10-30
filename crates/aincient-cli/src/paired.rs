//! Sites paired with their engine, without SSH (M4, E-02). EXPERIMENTAL
//! (DECISIONS 0483): every command here needs `--experimental`.
//!
//! - `sites add <SLUG> --url … [--install-code …]`: the first Manager, with
//!   the install code only SSH makes; it becomes the site's primary.
//! - `sites join <SLUG> --url …`: every later Manager. Shows an 8-symbol code
//!   to type into the primary, then waits for the approval.
//! - `--site <SLUG> engine requests|approve|managers|revoke|forget`: the
//!   pairing list, the primary's approvals, and leaving.
//! - On an engine-paired site, `app status`, `app start` (wake), `site
//!   freeze`, `site live`, `site lockdown` and `site unlock` go to the
//!   engine's API; everything else is refused (no SSH, no host CLI).
//!
//! The logic is [`aincient_core::pairing`]; this prints and asks.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use aincient_core::engine_client::{EngineClient, EngineError, EngineStatus, Https, Verb};
use aincient_core::pairing::{self, Waited, Waiter};
use aincient_core::sites::{self, Site};
use aincient_core::{remote, ssh_key, ChangeGuard};
use anyhow::{anyhow, bail, Context, Result};

use crate::{confirm, confirm_change, style, AppCommand, Command, SiteCommand};

/// Refuse an engine-paired feature without `--experimental`.
pub fn require_experimental(experimental: bool, what: &str) -> Result<()> {
    if experimental {
        return Ok(());
    }
    bail!(
        "`{what}` uses the EXPERIMENTAL engine (DECISIONS 0483): it holds the server's Docker \
         socket, which is root on that server, and pairs this Manager with it over HTTPS. \
         Re-run with --experimental if that is what you want."
    )
}

/// An engine error in words; a refused token says this Manager was removed.
fn explain(site: &Site, e: EngineError) -> anyhow::Error {
    if pairing::is_removed(&e) {
        return anyhow!(
            "{}.\nForget it here: atelier --site {} engine forget --experimental",
            pairing::removed_note(site),
            site.slug
        );
    }
    if matches!(e, EngineError::NotPaired) {
        return anyhow!(
            "this Manager has no pairing token for `{}` in the keychain. Join again: atelier \
             sites remove {slug}, then atelier sites join {slug} --url {} --experimental",
            site.slug,
            site.engine_url().unwrap_or_default(),
            slug = site.slug
        );
    }
    anyhow!(e)
}

/// The Manager's name: given, or this computer's (checked either way).
fn manager_name(given: Option<String>) -> Result<String> {
    match given {
        Some(n) => pairing::clean_name(&n)
            .filter(|c| c == n.trim())
            .ok_or_else(|| anyhow!("--name: 1 to 64 visible characters")),
        None => Ok(pairing::default_name()),
    }
}

/// Read one line from the terminal (or a pipe), after `prompt`.
fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// `sites add <SLUG> --url … --install-code …` (m4-2).
pub fn sites_add(
    uh: &std::path::Path,
    slug: &str,
    label: Option<&str>,
    url: &str,
    install_code: Option<String>,
    name: Option<String>,
) -> Result<()> {
    let site = sites::engine_entry(uh, slug, label, url)?;
    let name = manager_name(name)?;
    let code = match install_code {
        Some(c) => c,
        None if std::io::stdin().is_terminal() => {
            read_line("The install code the site's installer printed (XXXX-XXXX-XXXX-XXXX): ")?
        }
        None => bail!(
            "no --install-code, and this isn't an interactive terminal to ask for one. Nothing \
             was added."
        ),
    };
    let base = pairing::engine_url(&site)?;
    println!("Pairing with {} as \"{name}\"…", base.base());
    let added = sites::add_remote_site(uh, site, |site| {
        let client = EngineClient::new(base.clone(), Https::new());
        let paired = pairing::pair(&client, &code, &name).map_err(|e| match e {
            EngineError::Unauthorized(m) | EngineError::Forbidden(m) => anyhow!(
                "the site refused the install code ({m}). It works once: a used, mistyped or \
                 replaced code is refused. Make a new one over SSH: docker exec \
                 <project>-engine-1 atelier engine admin install-code"
            ),
            other => anyhow!(other),
        })?;
        pairing::keep(&ssh_key::Keychain, &site.slug, &paired)
            .context("paired, but the token couldn't be kept in the keychain")?;
        println!(
            "{} as the site's primary Manager (pairing {}). Its token is in your OS keychain.",
            style::success("Paired"),
            paired.id
        );
        Ok(())
    })
    .context("nothing was added")?;
    finish_added(&added.site, added.activated);
    Ok(())
}

fn finish_added(site: &Site, activated: bool) {
    println!(
        "{} {} ({}) — engine at {}",
        style::success("Added site"),
        site.slug,
        site.label,
        site.engine_url().unwrap_or_default()
    );
    if activated {
        println!("It is the active site: commands without --site use it.");
    }
    println!(
        "Try: atelier --site {} app status --experimental",
        site.slug
    );
}

/// Real time for [`pairing::wait_for_approval`]; Ctrl-C ends the process,
/// and the request dies on its own after its 10 minutes.
struct Clock {
    started: Instant,
    told: u64,
}

impl Waiter for Clock {
    fn wait(&mut self, d: Duration) -> bool {
        std::thread::sleep(d);
        let minute = self.started.elapsed().as_secs() / 60;
        if minute > self.told {
            self.told = minute;
            println!("  still waiting ({minute} min)…");
        }
        true
    }
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// The code, big and clear, with what to do with it.
pub fn code_panel(code: &str, url: &str) -> String {
    let spaced: String = code
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let width = spaced.chars().count() + 8;
    format!(
        "\n  ┌{line}┐\n  │    {spaced}    │\n  └{line}┘\n\n  Type this into your FIRST Manager (the site's primary) to let this one in:\n    \
         GUI: the site's Managers panel → Approve\n    \
         CLI: atelier --site <its name for {url}> engine approve --experimental\n  \
         It works for 10 minutes. Nothing else lets this Manager in.\n",
        line = "─".repeat(width),
    )
}

/// `sites join <SLUG> --url …` (m4-5).
pub fn sites_join(
    uh: &std::path::Path,
    slug: &str,
    label: Option<&str>,
    url: &str,
    name: Option<String>,
) -> Result<()> {
    let site = sites::engine_entry(uh, slug, label, url)?;
    let name = manager_name(name)?;
    let base = pairing::engine_url(&site)?;
    let added = sites::add_remote_site(uh, site, |site| {
        let client = EngineClient::new(base.clone(), Https::new());
        let req = pairing::request_join(&client, &name).map_err(|e| match e {
            EngineError::Forbidden(m) => anyhow!("the site isn't taking new Managers: {m}"),
            other => anyhow!(other),
        })?;
        let started = Instant::now();
        println!(
            "Asked {} to let \"{name}\" in.{}",
            base.base(),
            code_panel(&req.code, &base.base())
        );
        println!("Waiting for the approval… (Ctrl-C cancels)");
        let mut clock = Clock { started, told: 0 };
        match pairing::wait_for_approval(&client, &req, &mut clock)? {
            Waited::Paired(p) => {
                pairing::keep(&ssh_key::Keychain, &site.slug, &p)
                    .context("approved, but the token couldn't be kept in the keychain")?;
                println!(
                    "{} (pairing {}). Its token is in your OS keychain.",
                    style::success("Approved"),
                    p.id
                );
                Ok(())
            }
            Waited::Expired => bail!(
                "the request expired (10 minutes) or the site locked down before it was \
                 approved. Run `atelier sites join` again for a new code"
            ),
            Waited::Cancelled => bail!("stopped waiting"),
        }
    })
    .context("nothing was added")?;
    finish_added(&added.site, added.activated);
    Ok(())
}

/// What a command does on an engine-paired site. Pure, so the table is
/// tested.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Status { json: bool },
    Open,
    Verb { verb: Verb, yes: bool },
    Requests,
    Approve { yes: bool },
    Managers,
    Revoke { id: String, yes: bool },
    Forget { yes: bool },
}

/// Map a command to its engine route, or refuse it: no SSH means no host
/// CLI, only the verbs the engine has.
pub fn route(command: Command, site: &Site) -> Result<Route> {
    let url = site.engine_url().unwrap_or_default();
    let refuse = |what: &str| {
        anyhow!(
            "`atelier {what}` isn't available here: {}",
            remote::no_ssh(&site.slug, url)
        )
    };
    Ok(match command {
        Command::App { command } => match command {
            AppCommand::Status { json } => Route::Status { json },
            AppCommand::Start => Route::Verb {
                verb: Verb::Wake,
                yes: true,
            },
            AppCommand::Open => Route::Open,
            AppCommand::Stop { .. } => return Err(refuse("app stop")),
            AppCommand::Update { .. } => return Err(refuse("app update")),
            AppCommand::CheckUpdate { .. } => return Err(refuse("app check-update")),
            AppCommand::Install { .. } => return Err(refuse("app install")),
            AppCommand::Reinstall { .. } => return Err(refuse("app reinstall")),
            AppCommand::Down { .. } => return Err(refuse("app down")),
            AppCommand::Channel { .. } => return Err(refuse("app channel")),
            AppCommand::Logs { .. } => return Err(refuse("app logs")),
            AppCommand::View => return Err(refuse("app view")),
            AppCommand::Password { .. } => return Err(refuse("app password")),
        },
        Command::Site { command } => match command {
            SiteCommand::Freeze {
                label,
                keep,
                no_serve,
                force,
                base_url,
                yes,
            } => {
                if label.is_some() || keep || no_serve || force || base_url.is_some() {
                    bail!(
                        "the engine's freeze takes no options (--label, --keep, --no-serve, \
                         --force, --base-url need the server's own `atelier`, over SSH)"
                    );
                }
                Route::Verb {
                    verb: Verb::Freeze,
                    yes,
                }
            }
            SiteCommand::Live { yes } => Route::Verb {
                verb: Verb::Live,
                yes,
            },
            SiteCommand::Lockdown { yes } => Route::Verb {
                verb: Verb::Lockdown,
                yes,
            },
            SiteCommand::Unlock { yes } => Route::Verb {
                verb: Verb::Unlock,
                yes,
            },
            SiteCommand::Export { .. } => return Err(refuse("site export")),
            SiteCommand::Snapshots { .. } => return Err(refuse("site snapshots")),
            SiteCommand::Use { .. } => return Err(refuse("site use")),
            SiteCommand::Prune { .. } => return Err(refuse("site prune")),
        },
        Command::Doctor { .. } => return Err(refuse("doctor")),
        Command::Data { .. } => return Err(refuse("data")),
        Command::Ai { .. } => return Err(refuse("ai")),
        Command::Pack { .. } => return Err(refuse("pack")),
        Command::Mcp => return Err(refuse("mcp")),
        Command::Sites { .. } => unreachable!("handled before resolving a site"),
        #[cfg(unix)]
        Command::Engine { command } => {
            use crate::engine::EngineCommand as E;
            match command {
                E::Requests => Route::Requests,
                E::Approve { yes } => Route::Approve { yes },
                E::Managers => Route::Managers,
                E::Revoke { id, yes } => Route::Revoke { id, yes },
                E::Forget { yes } => Route::Forget { yes },
                E::InstallCode => bail!(
                    "only SSH makes an install code (E-02): on the server, `docker exec \
                     <project>-engine-1 atelier engine admin install-code`, or from a Manager \
                     that reaches it over SSH, `atelier --site <SLUG> engine install-code \
                     --experimental`"
                ),
                E::Enable | E::Disable | E::Remove { .. } => {
                    return Err(refuse("engine enable/disable/remove"))
                }
                E::Serve { .. } | E::Admin { .. } | E::Probe { .. } => {
                    return Err(refuse("engine serve/admin/probe"))
                }
            }
        }
    })
}

/// The confirmation for a lifecycle verb: `(question, detail)`.
fn verb_words(verb: Verb) -> (&'static str, &'static str) {
    match verb {
        Verb::Freeze => (
            "Freeze {site}? Visitors get the snapshot instead of the live site.",
            "",
        ),
        Verb::Live => ("Switch {site} back to live?", ""),
        Verb::Lockdown => (
            "Lock down {site}?",
            "The site stops at once: app and db are stopped, wake and pairing close, every \
             pending request is dropped and every Manager junior to this one is revoked. Only \
             this Manager, a senior one, or SSH can unlock it.",
        ),
        Verb::Unlock => (
            "Unlock {site}?",
            "Wake and pairing open again. The site stays stopped until it is started.",
        ),
        Verb::Wake => ("Start {site}?", ""),
        Verb::Rest => ("Put {site} to rest?", ""),
    }
}

/// The guard for `site`, live when its app runs (D-07).
fn guard(site: &Site, st: &EngineStatus) -> ChangeGuard {
    ChangeGuard::for_site(site, st.app == "running" && !st.lockdown)
}

fn print_status(site: &Site, st: &EngineStatus) {
    println!(
        "  Site:     {} ({}) — engine at {}",
        site.label,
        site.slug,
        site.engine_url().unwrap_or_default()
    );
    println!("  App:      {}", st.app);
    println!("  Database: {}", st.db);
    println!(
        "  Lockdown: {}",
        if st.lockdown {
            "ON (locked down)"
        } else {
            "off"
        }
    );
    if st.wake_paused {
        println!("  Wake:     paused (too many wrong wake codes)");
    }
    if let Some(j) = &st.job {
        let state = match (&j.finished, &j.error) {
            (None, _) => "running".to_string(),
            (Some(_), Some(e)) => format!("failed: {e}"),
            (Some(_), None) => "done".to_string(),
        };
        println!("  Last job: {} ({state})", j.verb);
    }
    println!("  API:      {}", st.api);
}

/// After a verb's 202: poll `status` until its job ends (or `limit`).
fn follow_job(
    c: &EngineClient<Https>,
    site: &Site,
    verb: &str,
    since: u64,
    limit: Duration,
) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < limit {
        std::thread::sleep(Duration::from_secs(2));
        let st = c.status().map_err(|e| explain(site, e))?;
        if let Some(j) = st
            .job
            .as_ref()
            .filter(|j| j.verb == verb && j.started >= since)
        {
            if j.finished.is_some() {
                return match &j.error {
                    Some(e) => bail!("{verb} failed on the server: {e}"),
                    None => {
                        println!(
                            "{} {verb}: app {}, db {}{}",
                            style::success("Done"),
                            st.app,
                            st.db,
                            if st.lockdown { ", locked down" } else { "" }
                        );
                        Ok(())
                    }
                };
            }
        }
    }
    println!(
        "{}",
        style::warn(&format!(
            "{verb} is still running on the server; `atelier --site {} app status \
             --experimental` shows when it ends",
            site.slug
        ))
    );
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn age(at: u64) -> String {
    if at == 0 {
        return "never".into();
    }
    let s = now().saturating_sub(at);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

/// A command on an engine-paired site.
pub fn run(command: Command, site: &Site, experimental: bool) -> Result<()> {
    require_experimental(experimental, "an engine-paired site")?;
    let route = route(command, site)?;
    let c = pairing::client_with(site, &ssh_key::Keychain, Https::new())?;
    let ex = |e| explain(site, e);
    match route {
        Route::Status { json } => {
            let st = c.status().map_err(ex)?;
            if json {
                let mut v = serde_json::to_value(&st)?;
                v["engine_url"] = site.engine_url().unwrap_or_default().into();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_status(site, &st);
            }
            Ok(())
        }
        Route::Open => {
            let url = format!("{}/atelier", site.engine_url().unwrap_or_default());
            println!("Opening {}", style::url(&url));
            aincient_core::open_url(&url)
        }
        Route::Verb { verb, yes } => {
            let st = c.status().map_err(ex)?;
            if verb == Verb::Unlock && !st.lockdown {
                println!("{} isn't locked down; nothing to do.", site.label);
                return Ok(());
            }
            let g = guard(site, &st);
            let (question, detail) = verb_words(verb);
            if !confirm_change(&g, detail, question, yes)? {
                println!("{}", style::warn("Aborted — nothing was changed."));
                return Ok(());
            }
            let since = now().saturating_sub(5);
            let acted = c.act(verb).map_err(ex)?;
            if verb == Verb::Unlock {
                println!("{} {} is unlocked.", style::success("Done."), site.label);
                return Ok(());
            }
            let job = acted.job.unwrap_or_else(|| verb.path().to_string());
            if let Some(after) = &acted.after {
                println!("{job} queued after {after}.");
            }
            println!("{job} started on the engine…");
            follow_job(&c, site, &job, since, Duration::from_secs(300))
        }
        Route::Requests => {
            let list = pairing::requests(&c).map_err(|e| match e {
                EngineError::Forbidden(m) => {
                    anyhow!("{m}: only the site's primary Manager sees and approves requests")
                }
                other => explain(site, other),
            })?;
            if list.is_empty() {
                println!("No Managers are waiting to join {}.", site.label);
                return Ok(());
            }
            println!("  {:<32} {:<18} ASKED", "NAME", "FROM");
            for r in &list {
                println!(
                    "  {:<32} {:<18} {}{}",
                    r.name,
                    r.ip,
                    age(r.created),
                    if r.approved { "  (approved)" } else { "" }
                );
            }
            println!(
                "\n  Their codes are on their own screens, never here. Approve one with \
                 `atelier --site {} engine approve --experimental` and type its code.",
                site.slug
            );
            Ok(())
        }
        Route::Approve { yes } => approve(&c, site, yes),
        Route::Managers => {
            let m = pairing::managers(&c).map_err(ex)?;
            println!(
                "  {:<13} {:<28} {:<10} {:<14} VIA",
                "ID", "NAME", "RANK", "LAST SEEN"
            );
            for (i, p) in m.pairings.iter().enumerate() {
                let rank = if p.primary {
                    "primary".to_string()
                } else {
                    format!("#{}", i + 1)
                };
                println!(
                    "  {:<13} {:<28} {:<10} {:<14} {}{}",
                    p.id,
                    p.name,
                    rank,
                    age(p.last_used),
                    p.via,
                    if p.id == m.me {
                        "   ← this Manager"
                    } else {
                        ""
                    }
                );
            }
            println!(
                "\n  Senior first. A Manager revokes itself (`engine forget`) or a junior one \
                 (`engine revoke <ID>`), never a senior one."
            );
            Ok(())
        }
        Route::Revoke { id, yes } => {
            let m = pairing::managers(&c).map_err(ex)?;
            let Some(target) = m.pairings.iter().find(|p| p.id == id) else {
                bail!("no pairing `{id}` on {}; see `engine managers`", site.label);
            };
            if id == m.me {
                bail!("that is this Manager; use `engine forget` to leave the site");
            }
            let g = ChangeGuard::for_site(site, false);
            let q = format!("Revoke \"{}\" from {{site}}?", target.name);
            if !confirm_change(&g, "It stops working on its next call.", &q, yes)? {
                println!("{}", style::warn("Aborted — nothing was changed."));
                return Ok(());
            }
            pairing::revoke(&c, &id).map_err(|e| match e {
                EngineError::Forbidden(m) => anyhow!("{m}"),
                other => explain(site, other),
            })?;
            println!(
                "{} \"{}\" can no longer manage {}.",
                style::success("Revoked."),
                target.name,
                site.label
            );
            Ok(())
        }
        Route::Forget { yes } => forget(&c, site, yes),
    }
}

/// On the primary: show who waits, take the typed code, confirm naming the
/// host (D-07), approve.
fn approve(c: &EngineClient<Https>, site: &Site, yes: bool) -> Result<()> {
    let st = c.status().map_err(|e| explain(site, e))?;
    let list = pairing::requests(c).map_err(|e| match e {
        EngineError::Forbidden(m) => {
            anyhow!("{m}: only the site's primary Manager approves new ones")
        }
        other => explain(site, other),
    })?;
    let waiting: Vec<_> = list.iter().filter(|r| !r.approved).collect();
    if waiting.is_empty() {
        println!("No Managers are waiting to join {}.", site.label);
        return Ok(());
    }
    println!("Waiting to join {}:", site.label);
    for r in &waiting {
        println!("  \"{}\" from {} ({})", r.name, r.ip, age(r.created));
    }
    println!(
        "\nType ONLY the code shown on the new Manager's own screen — never one from a website, \
         a message or an email. Whoever's code you type can manage this site."
    );
    let typed = read_line("Code: ")?;
    if typed.is_empty() {
        println!("{}", style::warn("No code typed — nothing was changed."));
        return Ok(());
    }
    let g = guard(site, &st);
    if !confirm_change(
        &g,
        "",
        "Let the Manager showing that code manage {site}?",
        yes,
    )? {
        println!("{}", style::warn("Aborted — nothing was changed."));
        return Ok(());
    }
    match pairing::approve(c, &typed) {
        Ok(name) => {
            println!(
                "{} \"{name}\" finishes pairing on its own within seconds.",
                style::success("Approved.")
            );
            Ok(())
        }
        Err(EngineError::NotFound(_)) => bail!(
            "no waiting Manager shows that code (check it, or it expired). Nothing was changed."
        ),
        Err(e) => Err(explain(site, e)),
    }
}

/// Leave the site: revoke this pairing (already gone is fine), delete the
/// token, drop the entry.
fn forget(c: &EngineClient<Https>, site: &Site, yes: bool) -> Result<()> {
    let g = ChangeGuard::for_site(site, false);
    if !confirm_change(
        &g,
        "This Manager's pairing is revoked on the site, its token deleted from your keychain, \
         and the site dropped from your list. Joining again needs the primary's approval.",
        "Forget {site} on this Manager?",
        yes,
    )? {
        println!("{}", style::warn("Aborted — nothing was changed."));
        return Ok(());
    }
    match pairing::forget_self(c) {
        Ok(()) => println!("{} this Manager's pairing.", style::success("Revoked")),
        Err(e) if pairing::is_removed(&e) || e == EngineError::NotPaired => {
            println!("The site had already removed this Manager.")
        }
        Err(e) => {
            if !confirm(
                &format!("Couldn't revoke on the site ({e}). Forget it here anyway?"),
                false,
            )? {
                bail!("nothing was changed");
            }
        }
    }
    aincient_core::engine_client::forget_token(&ssh_key::Keychain, &site.slug)?;
    let uh = sites::user_home()?;
    sites::remove_site(&uh, &site.slug)?;
    println!(
        "{} {} is no longer in your sites, and its token is gone from your keychain.",
        style::success("Forgotten."),
        site.slug
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;

    fn site() -> Site {
        Site::new_engine(
            std::path::Path::new("/h"),
            "vps",
            Some("VPS"),
            "https://cms.example.com",
        )
    }

    fn parsed(args: &[&str]) -> Cli {
        let mut argv = vec!["atelier", "--site", "vps"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap()
    }

    fn r(args: &[&str]) -> Result<Route> {
        route(parsed(args).command, &site())
    }

    #[test]
    fn the_engines_verbs_route_to_its_api() {
        assert_eq!(
            r(&["app", "status", "--json"]).unwrap(),
            Route::Status { json: true }
        );
        assert_eq!(
            r(&["app", "start"]).unwrap(),
            Route::Verb {
                verb: Verb::Wake,
                yes: true
            }
        );
        assert_eq!(
            r(&["site", "freeze", "-y"]).unwrap(),
            Route::Verb {
                verb: Verb::Freeze,
                yes: true
            }
        );
        assert_eq!(
            r(&["site", "live"]).unwrap(),
            Route::Verb {
                verb: Verb::Live,
                yes: false
            }
        );
        assert_eq!(
            r(&["site", "lockdown"]).unwrap(),
            Route::Verb {
                verb: Verb::Lockdown,
                yes: false
            }
        );
        assert_eq!(
            r(&["site", "unlock", "-y"]).unwrap(),
            Route::Verb {
                verb: Verb::Unlock,
                yes: true
            }
        );
        assert_eq!(r(&["app", "open"]).unwrap(), Route::Open);
    }

    #[test]
    fn everything_else_is_refused_naming_what_works() {
        for args in [
            &["app", "stop"][..],
            &["app", "update"],
            &["app", "logs"],
            &["data", "backup"],
            &["data", "restore", "x.tar.gz"],
            &["doctor"],
            &["site", "export"],
            &["site", "snapshots"],
            &["ai", "model", "list"],
        ] {
            let e = r(args).unwrap_err().to_string();
            assert!(
                e.contains("without SSH") && e.contains("lockdown"),
                "{args:?}: {e}"
            );
        }
        let e = r(&["site", "freeze", "--label", "x"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("takes no options"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn pairing_commands_route_and_install_code_needs_ssh() {
        assert_eq!(r(&["engine", "requests"]).unwrap(), Route::Requests);
        assert_eq!(
            r(&["engine", "approve"]).unwrap(),
            Route::Approve { yes: false }
        );
        assert_eq!(r(&["engine", "managers"]).unwrap(), Route::Managers);
        assert_eq!(
            r(&["engine", "revoke", "b00000000002", "-y"]).unwrap(),
            Route::Revoke {
                id: "b00000000002".into(),
                yes: true
            }
        );
        assert_eq!(
            r(&["engine", "forget"]).unwrap(),
            Route::Forget { yes: false }
        );
        let e = r(&["engine", "install-code"]).unwrap_err().to_string();
        assert!(e.contains("only SSH makes an install code"), "{e}");
        assert!(r(&["engine", "remove"]).is_err());
    }

    #[test]
    fn engine_sites_need_the_experimental_flag() {
        let cli = parsed(&["app", "status"]);
        let e = run(cli.command, &site(), cli.experimental)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--experimental") && e.contains("root"), "{e}");
        for args in [
            &["atelier", "sites", "add", "vps", "--url", "https://x.com"][..],
            &["atelier", "sites", "join", "vps", "--url", "https://x.com"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(!cli.experimental);
            let crate::Command::Sites { command } = cli.command else {
                panic!()
            };
            let e = crate::run_sites(command, false).unwrap_err().to_string();
            assert!(e.contains("--experimental"), "{e}");
        }
    }

    #[test]
    fn add_and_join_arguments() {
        let ok = |a: &[&str]| Cli::try_parse_from(a).is_ok();
        assert!(ok(&[
            "atelier",
            "sites",
            "add",
            "vps",
            "--url",
            "https://x.com",
            "--install-code",
            "AAAA-BBBB-CCCC-DDDD",
            "--name",
            "Mac",
            "--experimental"
        ]));
        // --url and --host are two different sites; codes and names need a URL.
        assert!(!ok(&[
            "atelier",
            "sites",
            "add",
            "vps",
            "--url",
            "https://x.com",
            "--host",
            "h"
        ]));
        assert!(!ok(&[
            "atelier",
            "sites",
            "add",
            "vps",
            "--url",
            "https://x.com",
            "--port",
            "41300"
        ]));
        assert!(!ok(&[
            "atelier",
            "sites",
            "add",
            "vps",
            "--install-code",
            "X"
        ]));
        assert!(!ok(&["atelier", "sites", "add", "vps", "--name", "X"]));
        assert!(
            !ok(&["atelier", "sites", "join", "vps"]),
            "join needs --url"
        );
        assert!(ok(&[
            "atelier",
            "sites",
            "join",
            "vps",
            "--url",
            "https://x.com",
            "--experimental"
        ]));
        assert!(manager_name(Some("  ".into())).is_err());
        assert!(manager_name(Some("a\u{202E}b".into())).is_err());
        assert_eq!(manager_name(Some(" Laptop B ".into())).unwrap(), "Laptop B");
    }

    #[test]
    fn plain_http_is_refused_before_any_code_is_sent() {
        let uh = std::env::temp_dir().join(format!("atelier-paired-{}", std::process::id()));
        let e = sites_add(
            &uh,
            "vps",
            None,
            "http://cms.example.com",
            Some("AAAA-BBBB-CCCC-DDDD".into()),
            Some("m".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("plain HTTP"), "{e}");
        let e = sites_join(&uh, "vps", None, "http://cms.example.com", Some("m".into()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("plain HTTP"), "{e}");
        assert!(!uh.exists(), "nothing written");
    }

    #[test]
    fn the_code_panel_shows_the_code_and_what_to_do() {
        let p = code_panel("ABCD-EFGH", "https://x.com");
        assert!(p.contains("A B C D - E F G H"), "{p}");
        assert!(
            p.contains("FIRST Manager") && p.contains("engine approve"),
            "{p}"
        );
        assert!(p.contains("10 minutes"));
    }
}
