//! `atelier` — the command-line front door to an Atelier CMS appliance.
//!
//! A thin presentation layer over [`aincient_core`]: it parses arguments, confirms
//! destructive actions, and formats output. All behaviour lives in the core crate,
//! shared verbatim with the Tauri manager GUI.

use std::io::Write;
use std::path::PathBuf;

use aincient_core::{doctor, ops, remote, sites, ssh_key, Channel, InstallOptions, Stack};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod style;

#[derive(Parser)]
#[command(
    name = "atelier",
    version,
    about = "Install and manage your Atelier CMS appliance.",
    long_about = "Install and manage your Atelier CMS appliance.\n\nAtelier runs as a \
                  Docker container; this command lays down and drives that stack \
                  (default ~/.atelier, override with ATELIER_HOME). Several sites can \
                  live side by side: see `atelier sites`, and pick one per command \
                  with --site."
)]
struct Cli {
    /// Operate on this registered site (see `atelier sites list`). Outranks
    /// ATELIER_HOME and the active site.
    #[arg(long, global = true, value_name = "SLUG")]
    site: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose the appliance — and repair it with --fix.
    ///
    /// Checks the host (Docker, ports, disk), the stack (compose files,
    /// containers) and the site inside it (Drupal boots, updates applied, files
    /// writable). Read-only unless you pass --fix. For a remote site the host
    /// is the server: the server's own doctor runs, after the SSH login and
    /// its atelier CLI are checked from here.
    Doctor {
        /// Apply the safe repairs: rebuild caches, run pending database
        /// updates, repair file ownership, restart containers, and re-run the
        /// appliance's self-heal. Never deletes data.
        #[arg(long)]
        fix: bool,
        /// Emit machine-readable JSON — paste this into a bug report.
        #[arg(long)]
        json: bool,
    },
    /// Manage the appliance: install, update, run, and inspect it.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Your sites: list, add, choose the active one, rename, remove.
    ///
    /// Each site is its own appliance — its own containers, database, files,
    /// port and backups. Commands operate on the active site unless you pass
    /// --site <SLUG> (or set ATELIER_HOME).
    Sites {
        #[command(subcommand)]
        command: SitesCommand,
    },
    /// Publish the site you built — export it to static HTML (deploy anywhere).
    Site {
        #[command(subcommand)]
        command: SiteCommand,
    },
    /// Move your data in and out as portable snapshots (database + files).
    Data {
        #[command(subcommand)]
        command: DataCommand,
    },
    /// Configure AI providers and the model bound to each Atelier role.
    Ai {
        #[command(subcommand)]
        command: AiCommand,
    },
    /// Build a component pack — scaffold it, develop it live, validate it.
    Pack {
        #[command(subcommand)]
        command: PackCommand,
    },
    /// Serve the pack developer's MCP tools over stdio (for Claude Code,
    /// Cursor, …). Needs a running dev stack (`atelier pack dev`).
    Mcp,
}

/// The multi-site registry (`~/.atelier/sites.toml`).
#[derive(Subcommand)]
enum SitesCommand {
    /// List your sites: name, port, whether each is running, and its version.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a new site under ~/.atelier/sites/<SLUG> on a free port, and
    /// install it — or, with --host, attach to a site already running on a
    /// server you reach over SSH.
    Add {
        /// Short machine name, used with --site (lowercase letters, digits, -).
        slug: String,
        /// Display name (defaults to the slug).
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Console port (default: the first free one from 41221 up).
        #[arg(long, value_name = "PORT", conflicts_with = "host")]
        port: Option<u16>,
        /// Attach to a running site on this server: an SSH alias from
        /// ~/.ssh/config, `host` or `user@host`. Nothing is installed there;
        /// the server's own `atelier` CLI does the work.
        #[arg(long, value_name = "SSH-TARGET")]
        host: Option<String>,
        /// The site's slug on the server (default: SLUG).
        #[arg(long, value_name = "SLUG", requires = "host")]
        remote_site: Option<String>,
        /// `guided`: generate a key for this server, keep it in your OS
        /// keychain, and show the line to add on the server. Default: your own
        /// SSH setup (agent, ~/.ssh/config).
        #[arg(long, value_name = "MODE", value_parser = ["guided"], requires = "host")]
        key: Option<String>,
        /// A Linux `atelier` binary to copy to the server if it has none (or an
        /// older one). Default: this version's release download. Also
        /// ATELIER_REMOTE_BINARY.
        #[arg(long, value_name = "PATH", requires = "host")]
        engine_binary: Option<PathBuf>,
        /// Where the site is public, e.g. `cms.example.com` (https assumed) or
        /// `http://host:port`. `app open` opens the console there; without
        /// one it goes through an SSH tunnel.
        #[arg(long, value_name = "DOMAIN", requires = "host")]
        domain: Option<String>,
    },
    /// Make a site the active one — what commands use without --site.
    Use {
        /// The site's slug.
        slug: String,
    },
    /// Change a site's display name. Its slug, folder and data stay as they are.
    Rename {
        /// The site's slug.
        slug: String,
        /// The new display name.
        label: String,
    },
    /// Stop a site and remove it from the list. Deletes nothing: its folder,
    /// database and files stay, and `sites add <SLUG>` brings it back.
    Remove {
        /// The site's slug.
        slug: String,
    },
}

/// The pack developer loop (see the Atelier bring-your-own-components docs).
#[derive(Subcommand)]
enum PackCommand {
    /// Scaffold a new pack — an ordinary Drupal module with the atelier
    /// metadata, dev overlay, Dockerfile and CI already in place.
    New {
        /// The module machine name (lowercase letters, digits, _).
        name: String,
        /// Also scaffold a hello-world console studio and its typed mount
        /// contract (EXPERIMENTAL; needs Atelier 0.16+).
        #[arg(long)]
        studio: bool,
    },
    /// Bring up the appliance in DEV MODE with this pack mounted: Twig/PHP
    /// edits visible on refresh, a Tailwind watcher, the component gallery,
    /// and a schema watcher that rebuilds + re-validates on save.
    /// Run from the pack's root.
    Dev {
        /// Bring the stack up and return instead of staying to watch.
        #[arg(long)]
        no_watch: bool,
        /// Reuse the machine's REAL appliance (~/.atelier) instead of the
        /// isolated per-pack dev stack — the pack is mounted into your live
        /// site and it is switched into dev mode.
        #[arg(long)]
        attach: bool,
    },
    /// Stop the pack dev stack. Run from the pack's root.
    Down {
        /// Also delete the isolated dev site's volumes (the throwaway demo
        /// database + files) — resets it for the next `atelier pack dev`.
        #[arg(long)]
        purge: bool,
    },
    /// Run the appliance's admission gate + CSS lint + atelier.pack.yml check
    /// against this pack (what a boot runs; what client CI should gate on).
    Validate {
        /// Pack module machine name; defaults to the pack in this directory.
        module: Option<String>,
    },
}

/// The appliance (Docker) lifecycle — the 90% commands.
#[derive(Subcommand)]
enum AppCommand {
    /// Show the appliance status (installed, running, reachable).
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install the appliance (or upgrade in place if already installed).
    Install {
        /// Which images to follow: `stable` (released versions, the default) or
        /// `edge` (every build off main — unreleased, may break).
        #[arg(long, value_name = "CHANNEL", conflicts_with = "image")]
        channel: Option<String>,
        /// Image tag to run — pins this exact image, following no channel.
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        /// Host port for the console.
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
    },
    /// Show which images this install follows, or switch it to another channel.
    ///
    /// `stable` tracks released versions (`:latest`); `edge` tracks every build
    /// off main (`:edge`) — unreleased and may break. Switching rewrites the
    /// stack's image setting; the new image arrives on the next update, or
    /// immediately with --now.
    Channel {
        /// The channel to switch to. Omit to show the current one.
        #[arg(value_name = "stable|edge")]
        channel: Option<String>,
        /// Pull and converge onto the new channel right away.
        #[arg(long)]
        now: bool,
        /// Skip the confirmation when the switch may step backwards.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Pull a newer image and converge in place (snapshot + auto-rollback).
    ///
    /// A very old site may not be able to reach the newest release in one move —
    /// a release that drops code can only migrate state that is already past it.
    /// When that happens the route through the required intermediate versions is
    /// worked out, shown, and (once confirmed) walked in order.
    Update {
        /// Stop at this version instead of the newest one your channel offers
        /// (e.g. `0.3.0`). Leaves the install pinned to it.
        #[arg(long, value_name = "VERSION", conflicts_with = "all")]
        to: Option<String>,
        /// Update every installed site in `atelier sites list`, one after the
        /// other, each along its own upgrade route.
        #[arg(long)]
        all: bool,
        /// Don't ask before walking a multi-step upgrade.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Check whether a newer image is available in the registry.
    #[command(visible_alias = "check")]
    CheckUpdate {
        #[arg(long)]
        json: bool,
    },
    /// Wipe all data and install from scratch (destructive).
    Reinstall {
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Start the appliance.
    Start,
    /// Stop the appliance (keeps data).
    Stop,
    /// Remove the containers. With --wipe, also delete all data (destructive).
    Down {
        /// Also remove volumes — wipes the database, files, and admin password.
        #[arg(long)]
        wipe: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Tail the appliance logs.
    Logs {
        /// Follow log output.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Limit to one service (e.g. `app` or `db`).
        service: Option<String>,
    },
    /// Open the console in your browser, signed in (one-time login link).
    /// For a remote site: at its recorded domain, else through an SSH tunnel
    /// that stays open until you press Ctrl-C.
    Open,
    /// Open your public site (the front page visitors see) in your browser.
    View,
    /// Show the initial admin password, or set a new one with --set.
    Password {
        /// Set a new admin password instead of showing the current one.
        #[arg(long, value_name = "NEW")]
        set: Option<String>,
    },
}

/// Publishing the site you built — the static export and (later) deploy.
#[derive(Subcommand)]
enum SiteCommand {
    /// Export the public site to static HTML — the deploy-anywhere artifact.
    Export {
        /// Host directory to write the static site into (default:
        /// ./aincient-export). Must be absent, empty, or a previous export —
        /// anything else is refused, nothing is overwritten.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Scheme + host to render absolute links against
        /// (e.g. https://example.com).
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Also package a .zip beside the exported site.
        #[arg(long)]
        zip: bool,
        /// Add config/sync to the zip (a portable "own your data" bundle).
        #[arg(long)]
        include_config: bool,
        /// Add users.json (accounts without password hashes) to the zip.
        #[arg(long)]
        include_users: bool,
        /// Skip the post-export link check.
        #[arg(long)]
        skip_link_check: bool,
    },
    /// Freeze the published site into a snapshot and serve it to visitors.
    /// Logged-in users keep seeing the live site.
    Freeze {
        /// A name for this snapshot (becomes part of its id).
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Mark the snapshot as kept: `prune` never removes it.
        #[arg(long)]
        keep: bool,
        /// Take the snapshot but keep serving whatever is served now.
        #[arg(long)]
        no_serve: bool,
        /// Serve it even if the export reported broken links or failures.
        #[arg(long)]
        force: bool,
        /// Scheme + host to render absolute links against.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
    },
    /// List snapshots and which one visitors see.
    Snapshots {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Serve a snapshot (or "live") to visitors. Instant; nothing restarts.
    Use {
        /// A snapshot id from `site snapshots`, or `live`.
        id: String,
    },
    /// Back to live: Drupal renders every visit. Snapshots are kept.
    Live,
    /// Delete old snapshots — keeps the newest N, every kept one, and the served one.
    Prune {
        /// How many recent unkept snapshots to retain.
        #[arg(long, default_value_t = 10, value_name = "N")]
        keep: u32,
    },
}

/// Your data in and out — portable db + files snapshots. `export`/`import` are
/// aliases for `backup`/`restore`, whichever mental model you prefer.
#[derive(Subcommand)]
enum DataCommand {
    /// Back up the database and uploaded files to ~/.atelier/backups as a
    /// portable .tar.gz snapshot.
    #[command(visible_alias = "export")]
    Backup {
        /// A label folded into the filename.
        #[arg(long)]
        label: Option<String>,
        /// Print the archive's path, size, sha256 and site as JSON.
        #[arg(long)]
        json: bool,
        /// Remote sites: keep the backup on the server only. By default it is
        /// also copied to this machine (~/.atelier/remote/<SLUG>/backups) and
        /// verified.
        #[arg(long)]
        no_copy: bool,
    },
    /// Restore from a backup file (destructive). A .tar.gz snapshot restores the
    /// database and files; a legacy .sql/.sql.gz dump restores the database only.
    #[command(visible_alias = "import")]
    Restore {
        /// Path to a `.tar.gz` snapshot (or a legacy `.sql`/`.sql.gz` dump).
        /// For a remote site: a file on this machine is uploaded and restored
        /// there; any other path is one on the server.
        file: PathBuf,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Restore a backup taken from a different site. Without it, such a
        /// backup is refused.
        #[arg(long)]
        other_site: bool,
        /// Remote sites: FILE is a path on the server, even if a file by that
        /// name exists here.
        #[arg(long)]
        on_server: bool,
    },
    /// List snapshots taken on this host.
    #[command(visible_alias = "backups")]
    List,
}

/// AI provider + model-role configuration.
#[derive(Subcommand)]
enum AiCommand {
    /// Inspect or change the AI model bound to each Atelier role.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

#[derive(Subcommand)]
enum ModelCommand {
    /// List each role and the provider/model it's bound to.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Bind a role (reasoning|task|fast) to a provider and model.
    Set {
        /// The role to bind: reasoning, task, or fast.
        role: String,
        /// A provider plugin id (e.g. anthropic, openai, ollama).
        #[arg(long, value_name = "ID")]
        provider: String,
        /// A model id offered by that provider.
        #[arg(long, value_name = "MODEL")]
        model: String,
    },
}

fn main() {
    // `NO_COLOR` is authoritative (no-color.org): force colour off so it wins even
    // over `CLICOLOR_FORCE`, which `supports-color` would otherwise let take priority.
    if std::env::var_os("NO_COLOR").is_some() {
        owo_colors::set_override(false);
    }
    if let Err(err) = run() {
        eprintln!("{} {err:#}", style::error("error:"));
        // A remote command's own exit code is the answer a script wants (m2-7).
        let code = err
            .downcast_ref::<remote::RemoteExit>()
            .map_or(1, |exit| exit.code);
        std::process::exit(code);
    }
}

/// A success banner: a restrained hairline rule (when colour is on) over a mint headline and the
/// cinnabar console URL. Used at the end of install/reinstall/update.
fn done_banner(headline: &str, url: &str) {
    println!();
    if let Some(rule) = style::rule() {
        println!("{rule}");
    }
    println!("{} Console: {}", style::success(headline), style::url(url));
}

/// Like [`done_banner`], but for when the stack started yet the console hasn't
/// answered within the wait window — it's most likely still booting, not broken.
fn pending_banner(headline: &str, url: &str) {
    println!();
    println!("{} Console: {}", style::warn(headline), style::url(url));
    println!(
        "{}",
        style::warn(
            "It's taking longer than usual to come up. Give it another minute, then reload \
             — or watch it boot with `atelier app logs -f app`."
        )
    );
}

/// A success banner for `atelier site export`: the mint headline, the output
/// path, and a nudge toward the deploy-anywhere payoff.
fn done_export_banner(path: &std::path::Path) {
    println!();
    if let Some(rule) = style::rule() {
        println!("{rule}");
    }
    println!(
        "{} Static site at {}",
        style::success("Exported."),
        style::url(&path.display().to_string())
    );
    println!("Deploy it anywhere — Netlify, Cloudflare Pages, GitHub Pages, or any static host.");
}

/// Renders core lifecycle progress on the terminal: a headline as each stage
/// begins, then heartbeat dots while the console finishes booting. Docker's own
/// output streams underneath untouched (we don't capture it — `captures_output`
/// stays false), so `pull`/`up` keep their familiar live progress.
#[derive(Default)]
struct CliReporter {
    last: Option<ops::Stage>,
    /// True when the current line was left open (no trailing newline) for dots.
    open_line: bool,
}

impl ops::Reporter for CliReporter {
    fn stage(&mut self, stage: ops::Stage, message: &str, _fraction: Option<f32>) {
        // A repeated Booting stage is a poll tick — show a heartbeat dot.
        if self.last == Some(stage) && stage == ops::Stage::Booting {
            print!(".");
            std::io::stdout().flush().ok();
            return;
        }
        if self.open_line {
            println!();
            self.open_line = false;
        }
        self.last = Some(stage);
        match stage {
            ops::Stage::Ready => println!("{}", style::success(message)),
            ops::Stage::Booting => {
                print!("{} ", style::heading(message));
                std::io::stdout().flush().ok();
                self.open_line = true;
            }
            _ => println!("{}", style::heading(message)),
        }
    }

    /// Ask on the terminal, defaulting to no.
    ///
    /// A non-interactive run (a script, a pipe, CI) answers NO rather than
    /// blocking forever on a stdin that will never carry an answer — and no is the
    /// safe answer, since it leaves the install exactly as it was. `--yes` is
    /// deliberately NOT consulted here: it exists to skip the confirmations of the
    /// operation you asked for, and this question is about a *different* change the
    /// manager noticed it could make to your install.
    fn confirm(&mut self, question: &str) -> bool {
        use std::io::IsTerminal;
        if self.open_line {
            println!();
            self.open_line = false;
        }
        if !std::io::stdin().is_terminal() {
            println!(
                "{}",
                style::warn(&format!(
                    "{question}\nNot asking — this isn't an interactive terminal. \
                     Nothing changed; run `atelier app channel stable` when you want it."
                ))
            );
            return false;
        }
        confirm(&format!("\n{question}"), false).unwrap_or(false)
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // The registry commands name their site positionally and must keep working
    // whatever the active site is — so they never resolve a stack.
    if let Command::Sites { command } = cli.command {
        if cli.site.is_some() {
            anyhow::bail!(
                "`atelier sites` takes the site as an argument — drop --site \
                 (e.g. `atelier sites use blog`)"
            );
        }
        return run_sites(command);
    }
    if let Command::App {
        command: AppCommand::Update { all: true, yes, .. },
    } = cli.command
    {
        if cli.site.is_some() {
            anyhow::bail!("--all updates every site — drop --site, or drop --all");
        }
        return update_all(yes);
    }

    let resolution = sites::locate(cli.site.as_deref())?;
    if let Some(site) = remote_site(&resolution) {
        return run_remote(cli.command, site);
    }
    // A local site: exactly the path it took before remote sites existed.
    let stack = resolution.into_stack();

    match cli.command {
        Command::Sites { .. } => unreachable!("handled above"),
        Command::Doctor { fix, json } => doctor(&stack, fix, json),
        Command::App { command } => run_app(command, &stack),
        Command::Site { command } => run_site(command, &stack),
        Command::Data { command } => run_data(command, &stack),
        Command::Ai { command } => run_ai(command, &stack),
        Command::Pack { command } => run_pack(command, &stack),
        Command::Mcp => {
            // When launched from a pack, serve against whichever stack `pack
            // dev` actually started (the isolated one if it exists); anywhere
            // else, the real appliance as before.
            let stack = std::env::current_dir()
                .ok()
                .and_then(|d| aincient_core::Pack::locate_upward(&d).ok())
                .map(|p| aincient_core::pack::resolve_stack(&p, &stack))
                .unwrap_or(stack);
            aincient_core::mcp::serve(&stack)
        }
    }
}

/// `atelier sites …` — the multi-site registry. The logic lives in
/// [`aincient_core::sites`]; this only prints.
fn run_sites(command: SitesCommand) -> Result<()> {
    let uh = sites::user_home()?;
    match command {
        SitesCommand::List { json } => sites_list(&uh, json),
        SitesCommand::Add {
            slug,
            label,
            host: Some(host),
            remote_site,
            key,
            engine_binary,
            domain,
            ..
        } => sites_add_remote(
            &uh,
            &slug,
            label.as_deref(),
            &host,
            remote_site.as_deref(),
            key.is_some(),
            engine_binary,
            domain,
        ),
        SitesCommand::Add {
            slug, label, port, ..
        } => {
            let added =
                sites::add_site(&uh, &slug, label.as_deref(), port, sites::host_port_is_free)?;
            let stack = added.site.stack();
            if added.reregistered {
                println!(
                    "{} {} was set up here before — registered again with its existing data.",
                    style::success("Found"),
                    stack.home.display()
                );
            }
            println!(
                "{} {} ({}) — port {}, folder {}",
                style::success("Added site"),
                added.site.slug,
                added.site.label,
                added.port,
                stack.home.display()
            );
            let installed = ops::install(
                &stack,
                &InstallOptions::default(),
                &mut CliReporter::default(),
            )
            .with_context(|| {
                format!(
                    "the site is added but not installed — finish with \
                         `atelier --site {slug} app install`"
                )
            })?;
            if installed {
                done_banner("Installed.", &stack.console_url());
            } else {
                pending_banner(
                    "Installed — still finishing first boot.",
                    &stack.console_url(),
                );
            }
            show_login(&stack);
            if added.activated {
                println!("It is the active site: commands without --site use it.");
            } else {
                println!(
                    "Use it with `atelier --site {slug} …`, or make it the default with \
                     `atelier sites use {slug}`."
                );
            }
            Ok(())
        }
        SitesCommand::Use { slug } => {
            let site = sites::use_site(&uh, &slug)?;
            if let Some(remote) = &site.remote {
                println!(
                    "{} {} ({}) — on {}",
                    style::success("Active site:"),
                    site.slug,
                    site.label,
                    remote.host
                );
                warn_atelier_home();
                return Ok(());
            }
            println!(
                "{} {} ({}) — console {}",
                style::success("Active site:"),
                site.slug,
                site.label,
                style::url(&site.stack().console_url())
            );
            warn_atelier_home();
            Ok(())
        }
        SitesCommand::Rename { slug, label } => {
            let site = sites::rename_site(&uh, &slug, &label)?;
            println!(
                "{} {} is now called \"{}\". Its folder and data are unchanged.",
                style::success("Renamed."),
                site.slug,
                site.label
            );
            Ok(())
        }
        SitesCommand::Remove { slug } => {
            let removed = sites::remove_site(&uh, &slug)?;
            if let Some(remote) = &removed.site.remote {
                println!(
                    "{} {} is no longer in your sites. Nothing on {} was touched.",
                    style::success("Removed."),
                    removed.site.slug,
                    remote.host
                );
                if remote.key == sites::KeyMode::Guided {
                    println!(
                        "Its SSH key for {} stays in your keychain (service `{}`) — re-adding \
                         the site reuses it.",
                        remote.host,
                        aincient_core::ssh_key::KEYCHAIN_SERVICE
                    );
                }
                return Ok(());
            }
            let stack = removed.site.stack();
            // Stopping keeps containers and volumes (`compose stop`); it frees
            // the port so it can't collide with a site added later. Only a
            // running site is stopped, so a host without Docker stays quiet.
            if stack.exists() && ops::status(&stack).running {
                if let Err(e) = ops::stop(&stack, &mut ops::Silent) {
                    println!(
                        "{} {e:#}",
                        style::warn("Removed from the list, but couldn't stop it:")
                    );
                }
            }
            println!(
                "{} {} is no longer in your sites. Nothing was deleted: its folder ({}) and \
                 its database and files (Docker project {}) are kept.",
                style::success("Removed."),
                removed.site.slug,
                stack.home.display(),
                removed.site.project
            );
            println!("Bring it back with `atelier sites add {slug}`.");
            if removed.was_active {
                println!(
                    "No site is active now — commands use ~/.atelier until you run \
                     `atelier sites use <SLUG>` or pass --site."
                );
            }
            Ok(())
        }
    }
}

/// `atelier sites list` — the registry with each site's live state.
fn sites_list(uh: &std::path::Path, json: bool) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Row {
        #[serde(flatten)]
        site: sites::SiteRow,
        running: bool,
        version: Option<String>,
    }
    // Remote sites are not contacted here: listing stays fast and offline-safe.
    // `atelier --site <SLUG> app status` asks the server.
    let rows: Vec<Row> = sites::list_sites(uh)?
        .into_iter()
        .map(|site| {
            let st = site.installed.then(|| ops::status(&site.stack()));
            Row {
                running: st.as_ref().is_some_and(|s| s.running),
                version: st.and_then(|s| s.version),
                site,
            }
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "{}",
            style::warn("No sites yet. Create one with `atelier sites add <SLUG>`.")
        );
        return Ok(());
    }
    println!(
        "  {:<14} {:<22} {:<6} {:<14} VERSION",
        "SITE", "NAME", "PORT", "STATE"
    );
    for r in &rows {
        if let Some(host) = &r.site.host {
            println!(
                "{} {:<14} {:<22} {:<6} {:<14} on {host}",
                if r.site.active { "*" } else { " " },
                r.site.slug,
                r.site.label,
                "",
                "remote",
            );
            continue;
        }
        let state = if !r.site.installed {
            "not installed"
        } else if r.running {
            "running"
        } else {
            "stopped"
        };
        println!(
            "{} {:<14} {:<22} {:<6} {:<14} {}",
            if r.site.active { "*" } else { " " },
            r.site.slug,
            r.site.label,
            r.site.port.map(|p| p.to_string()).unwrap_or_default(),
            state,
            r.version
                .clone()
                .or_else(|| r.site.image.clone())
                .unwrap_or_default()
        );
    }
    let running = rows.iter().filter(|r| r.running).count();
    println!("\n  * = active site (what commands use without --site)");
    println!(
        "  {running} running — each running site is its own app, database and web server \
         containers, with its own Postgres."
    );
    warn_atelier_home();
    Ok(())
}

/// Say so when `ATELIER_HOME` outranks the active site.
fn warn_atelier_home() {
    if let Some(home) = std::env::var_os("ATELIER_HOME") {
        println!(
            "{}",
            style::warn(&format!(
                "ATELIER_HOME is set ({}), so commands without --site use that folder instead \
                 of the active site.",
                std::path::Path::new(&home).display()
            ))
        );
    }
}

/// `atelier app update --all` — every installed site, one after the other, each
/// along its own route. A failure on one site doesn't stop the rest; the run
/// fails at the end if any did.
fn update_all(yes: bool) -> Result<()> {
    let uh = sites::user_home()?;
    let all = sites::list_sites(&uh)?;
    let remote: Vec<_> = all
        .iter()
        .filter(|r| r.host.is_some())
        .map(|r| r.slug.clone())
        .collect();
    if !remote.is_empty() {
        println!(
            "{}",
            style::warn(&format!(
                "--all updates the sites on this machine; remote sites are updated one at a \
                 time (`atelier --site <SLUG> app update`): {}",
                remote.join(", ")
            ))
        );
    }
    let rows: Vec<_> = all.into_iter().filter(|r| r.installed).collect();
    if rows.is_empty() {
        println!("{}", style::warn("No installed sites to update."));
        return Ok(());
    }
    let mut failed = Vec::new();
    for row in &rows {
        println!(
            "\n{}",
            style::heading(&format!("== {} ({}) ==", row.slug, row.label))
        );
        if let Err(e) = update_cmd(&row.stack(), None, yes) {
            println!("{} {e:#}", style::error("error:"));
            failed.push(row.slug.clone());
        }
    }
    if failed.is_empty() {
        println!("\n{}", style::success("Every site is done."));
        Ok(())
    } else {
        anyhow::bail!("the update failed for: {}", failed.join(", "))
    }
}

/// `atelier pack …` — the component-pack developer loop.
fn run_pack(command: PackCommand, stack: &Stack) -> Result<()> {
    use aincient_core::{pack, Pack};
    match command {
        PackCommand::New { name, studio } => {
            let dest = pack::scaffold(&std::env::current_dir()?, &name, studio)?;
            println!(
                "{} {}",
                style::success("Pack scaffolded at"),
                dest.display()
            );
            println!("Next: cd {name} && atelier pack dev — then open the gallery and edit components/showcase/.");
            if studio {
                println!(
                    "{} the console studio (studio/) is experimental and needs Atelier 0.16+; the contract is studio/atelier-studio.d.ts.",
                    style::warn("Studio:")
                );
            }
            Ok(())
        }
        PackCommand::Dev { no_watch, attach } => {
            let p = Pack::locate(&std::env::current_dir()?)?;
            let stack = if attach {
                println!(
                    "{} your REAL appliance (~/.atelier) is being switched into dev mode — its containers are recreated with this pack mounted.",
                    style::warn("--attach:")
                );
                pack::dev_up(stack, &p)?;
                stack.clone()
            } else {
                let first_boot = !pack::dev_stack(&p).exists();
                let dev = pack::ensure_dev_stack(&p, stack)?;
                println!(
                    "Isolated dev stack: compose project {} on port {} — your real appliance is untouched.",
                    dev.project_name(),
                    dev.http_port()
                );
                pack::dev_up(&dev, &p)?;
                if first_boot {
                    println!("First boot — a throwaway demo site is being installed.");
                }
                pack::wait_ready(&dev, std::time::Duration::from_secs(600), &mut |line| {
                    println!("{line}")
                });
                dev
            };
            println!(
                "{} pack {} is mounted; dev mode is ON.",
                style::success("Dev stack is up —"),
                p.module
            );
            println!(
                "Gallery: http://localhost:{}/atelier/packs/{}/gallery",
                stack.http_port(),
                p.module
            );
            match pack::sync_preset(&stack, &p) {
                Ok(()) => println!(
                    "Token preset synced into build/atelier/ (commit it — CI builds against it)."
                ),
                Err(e) => println!(
                    "{} {e:#}",
                    style::warn("Preset sync failed (CSS build may miss utilities):")
                ),
            }
            if no_watch {
                return Ok(());
            }
            pack::watch(&stack, &p, |line| println!("{line}"))
        }
        PackCommand::Down { purge } => {
            let p = Pack::locate(&std::env::current_dir()?)?;
            let resolved = pack::resolve_stack(&p, stack);
            if purge {
                // `down -v` deletes the compose project's volumes. Against the
                // real appliance those volumes ARE the site — refuse rather
                // than resolve there.
                if resolved.home != pack::dev_home(&p) {
                    anyhow::bail!(
                        "--purge resets the ISOLATED dev site, and this pack has none — \
                         refusing to delete your real appliance's data"
                    );
                }
                pack::dev_down_purge(&resolved, &p)?;
                println!(
                    "{} volumes deleted — the next `atelier pack dev` boots a fresh demo site.",
                    style::success("Dev stack stopped;")
                );
            } else {
                pack::dev_down(&resolved, &p)?;
                println!("{}", style::success("Dev stack stopped."));
            }
            Ok(())
        }
        PackCommand::Validate { module } => {
            // From a pack root, validate against whichever stack `pack dev`
            // started; an explicit module outside a pack keeps hitting the
            // real appliance.
            match (Pack::locate(&std::env::current_dir()?), module) {
                (Ok(p), module) => {
                    let resolved = pack::resolve_stack(&p, stack);
                    let module = module.unwrap_or(p.module);
                    pack::validate(&resolved, &module)
                }
                (Err(_), Some(module)) => pack::validate(stack, &module),
                (Err(e), None) => Err(e),
            }
        }
    }
}

/// `atelier app …` — the appliance lifecycle.
fn run_app(command: AppCommand, stack: &Stack) -> Result<()> {
    match command {
        AppCommand::Status { json } => status(stack, json),
        AppCommand::Install {
            channel,
            image,
            port,
        } => {
            // A registered site with no `.env` yet gets a port no other site
            // claims; everything else keeps today's behaviour (DECISIONS 0393).
            let user_home = sites::user_home().ok();
            let registry = user_home
                .as_deref()
                .and_then(|uh| sites::Registry::load(uh).ok().flatten());
            let opts = InstallOptions {
                image: match channel {
                    Some(name) => parse_channel(&name)?.image(),
                    None => image,
                },
                http_port: sites::port_for_install(
                    stack,
                    port,
                    registry.as_ref(),
                    user_home.as_deref(),
                    sites::host_port_is_free,
                ),
            };
            if ops::install(stack, &opts, &mut CliReporter::default())? {
                done_banner("Installed.", &stack.console_url());
            } else {
                pending_banner(
                    "Installed — still finishing first boot.",
                    &stack.console_url(),
                );
            }
            show_login(stack);
            Ok(())
        }
        AppCommand::Update { to, yes, .. } => update_cmd(stack, to, yes),
        AppCommand::Channel { channel, now, yes } => channel_cmd(stack, channel, now, yes),
        AppCommand::CheckUpdate { json } => check_update(stack, json),
        AppCommand::Reinstall { yes } => {
            if !confirm_change(
                &Target::Local,
                "Reinstall will DELETE all data (database, files, admin password) and install \
                 fresh. Continue?",
                yes,
            )? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            let opts = InstallOptions::default();
            if ops::reinstall(stack, &opts, &mut CliReporter::default())? {
                done_banner("Reinstalled.", &stack.console_url());
            } else {
                pending_banner(
                    "Reinstalled — still finishing first boot.",
                    &stack.console_url(),
                );
            }
            show_login(stack);
            Ok(())
        }
        AppCommand::Start => {
            if ops::start(stack, &mut CliReporter::default())? {
                println!(
                    "{} Console: {}",
                    style::success("Started."),
                    style::url(&stack.console_url())
                );
            } else {
                pending_banner("Started — still finishing boot.", &stack.console_url());
            }
            Ok(())
        }
        AppCommand::Stop => {
            ops::stop(stack, &mut ops::Silent)?;
            println!("{}", style::success("Stopped."));
            Ok(())
        }
        AppCommand::Down { wipe, yes } => {
            if wipe
                && !confirm_change(
                    &Target::Local,
                    "This will DELETE all data (database, files, admin password). Continue?",
                    yes,
                )?
            {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            ops::down(stack, wipe)?;
            println!(
                "{}",
                style::success(if wipe {
                    "Removed and wiped."
                } else {
                    "Removed (data kept)."
                })
            );
            Ok(())
        }
        AppCommand::Logs { follow, service } => {
            let mut cmd = ops::logs_command(stack, follow, service.as_deref());
            cmd.status().context("failed to run docker compose logs")?;
            Ok(())
        }
        AppCommand::Open => {
            ops::open_console_authed(stack)?;
            Ok(())
        }
        AppCommand::View => {
            ops::open_url(&stack.site_url())?;
            Ok(())
        }
        AppCommand::Password { set } => match set {
            Some(pw) => {
                ops::set_admin_password(stack, &pw, &mut ops::Silent)?;
                println!("{}", style::success("Admin password updated."));
                Ok(())
            }
            None => {
                match ops::admin_password(stack) {
                    Some(pw) => println!("admin / {pw}"),
                    None => println!(
                        "No saved initial password (it was likely changed, or you pinned one at \
                         install). Set a new one with: atelier app password --set <new>"
                    ),
                }
                Ok(())
            }
        },
    }
}

/// `atelier site …` — publish the site you built.
fn run_site(command: SiteCommand, stack: &Stack) -> Result<()> {
    match command {
        SiteCommand::Export {
            out,
            base_url,
            zip,
            include_config,
            include_users,
            skip_link_check,
        } => {
            let opts = ops::ExportOptions {
                out,
                base_url,
                zip,
                include_config,
                include_users,
                skip_link_check,
            };
            let path = ops::export_static(stack, &opts, &mut ops::Silent)?;
            done_export_banner(&path);
            Ok(())
        }
        SiteCommand::Freeze {
            label,
            keep,
            no_serve,
            force,
            base_url,
        } => {
            let opts = ops::FreezeOptions {
                label,
                keep,
                no_serve,
                force,
                base_url,
            };
            ops::freeze(stack, &opts, &mut ops::Silent)?;
            Ok(())
        }
        SiteCommand::Snapshots { json } => {
            let list = ops::snapshots(stack)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            print_snapshots(&list);
            Ok(())
        }
        SiteCommand::Use { id } => {
            ops::use_snapshot(stack, &id, &mut ops::Silent)?;
            if id == "live" {
                println!("{}", style::success("Live: Drupal renders every visit."));
            } else {
                println!(
                    "{} Visitors now see snapshot {id}. Logged-in users keep seeing the live site.",
                    style::success("Frozen.")
                );
            }
            Ok(())
        }
        SiteCommand::Live => {
            ops::use_snapshot(stack, "live", &mut ops::Silent)?;
            println!("{}", style::success("Live: Drupal renders every visit."));
            Ok(())
        }
        SiteCommand::Prune { keep } => {
            let deleted = ops::prune_snapshots(stack, keep, &mut ops::Silent)?;
            if deleted.is_empty() {
                println!("Nothing to prune.");
            } else {
                println!("{} {}", style::success("Deleted"), deleted.join(", "));
            }
            Ok(())
        }
    }
}

/// The `site snapshots` table: Live first, then newest first, the served row marked.
fn print_snapshots(list: &ops::SnapshotList) {
    let mark = |serving: bool| if serving { "●" } else { " " };
    println!(
        "{} {:<34} {:<24} {:<26} {:>5}  ",
        mark(list.serving == "live"),
        "live",
        "Live (Drupal renders every visit)",
        "",
        ""
    );
    for s in &list.snapshots {
        println!(
            "{} {:<34} {:<24} {:<26} {:>5}  {}",
            mark(list.serving == s.id),
            s.id,
            s.label.clone().unwrap_or_default(),
            s.frozen_at.clone().unwrap_or_default(),
            s.pages,
            if s.keep { "kept" } else { "" }
        );
    }
}

/// `atelier data …` — portable db + files snapshots.
fn run_data(command: DataCommand, stack: &Stack) -> Result<()> {
    match command {
        DataCommand::Backup { label, json, .. } => {
            let path = ops::backup(stack, label.as_deref(), &mut ops::Silent)?;
            if json {
                // What a Manager driving this CLI over SSH reads (m2-5).
                let info = ops::describe_backup(&path)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
                return Ok(());
            }
            println!("{} {}", style::success("Backup written to"), path.display());
            Ok(())
        }
        DataCommand::Restore {
            file,
            yes,
            other_site,
            ..
        } => {
            if !file.is_file() {
                anyhow::bail!("backup file not found: {}", file.display());
            }
            // Whose backup this is (m2-6, D-04): another site's is refused
            // before anything is asked.
            if let Some(note) = ops::restore_identity(stack, &file, other_site)?.message() {
                println!("{}", style::warn(note));
            }
            // Provenance BEFORE the confirmation: which image this data came from
            // is exactly what the operator needs to decide, and after the prompt
            // it would be advice about something already done.
            if let Some(skew) = ops::restore_skew(stack, &file) {
                println!("{}", style::warn(&skew));
            }
            if !confirm_change(
                &Target::Local,
                &restore_question(&file.display().to_string()),
                yes,
            )? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            ops::restore_with(
                stack,
                &file,
                &ops::RestoreOptions { other_site },
                &mut ops::Silent,
            )?;
            println!("{}", style::success("Restore complete."));
            Ok(())
        }
        DataCommand::List => list_backups(stack),
    }
}

/// `atelier ai …` — AI provider + model-role configuration.
fn run_ai(command: AiCommand, stack: &Stack) -> Result<()> {
    match command {
        AiCommand::Model { command } => match command {
            ModelCommand::List { json } => model_list(stack, json),
            ModelCommand::Set {
                role,
                provider,
                model,
            } => {
                ops::model_set(stack, &role, &provider, &model)?;
                println!("{} {role} → {provider}:{model}", style::success("Bound"));
                Ok(())
            }
        },
    }
}

fn model_list(stack: &Stack, json: bool) -> Result<()> {
    let roles = ops::model_list(stack)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&roles)?);
        return Ok(());
    }
    if roles.is_empty() {
        println!(
            "{}",
            style::warn("No model roles yet — connect AI through onboarding first.")
        );
        return Ok(());
    }
    for r in roles {
        let binding = if r.provider.is_empty() || r.model.is_empty() {
            style::warn("(not set)").to_string()
        } else {
            style::url(&format!("{}:{}", r.provider, r.model))
        };
        let star = if r.is_default() { " *" } else { "" };
        println!(
            "  {} {}{star}",
            style::heading(&format!("{:<10}", r.role)),
            binding
        );
    }
    println!("\n  * = default role (what the console inherits)");
    Ok(())
}

/// `atelier doctor [--fix] [--json]` — the diagnose/repair front door.
///
/// Exits non-zero when something is still failing after the run, so it works as
/// a gate in a script; warnings alone keep the exit code at 0.
fn doctor(stack: &Stack, fix: bool, json: bool) -> Result<()> {
    let report = if fix {
        // The repair ladder is chatty (a converge run streams docker output), so
        // it goes through the same reporter the lifecycle ops use.
        doctor::fix(stack, &mut CliReporter::default())
    } else {
        doctor::diagnose(stack)
    };

    finish_doctor(&report, fix, json, "atelier doctor --fix")
}

/// Print a doctor report (local or remote) and exit 1 when it is unhealthy.
fn finish_doctor(report: &doctor::Report, fix: bool, json: bool, fix_cmd: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&JsonReport::of(report))?);
    } else {
        print_report(report, fix, fix_cmd);
    }

    if !report.healthy() {
        std::process::exit(1);
    }
    Ok(())
}

/// The `--json` envelope: the report plus the verdict a reader would otherwise
/// have to recompute, and the manager version that produced it.
///
/// This exists so a pasted report answers "is it broken, and which build said
/// so?" without the reader tallying severities by hand — the whole point of
/// having a machine-readable mode in a bug report.
#[derive(serde::Serialize)]
struct JsonReport<'a> {
    manager_version: &'static str,
    healthy: bool,
    failures: usize,
    warnings: usize,
    skipped: usize,
    /// What `--fix` would attempt, in the order it would attempt it.
    available_repairs: Vec<doctor::Repair>,
    #[serde(flatten)]
    report: &'a doctor::Report,
}

impl<'a> JsonReport<'a> {
    fn of(report: &'a doctor::Report) -> Self {
        JsonReport {
            manager_version: env!("CARGO_PKG_VERSION"),
            healthy: report.healthy(),
            failures: report.count(doctor::Severity::Fail),
            warnings: report.count(doctor::Severity::Warn),
            skipped: report.count(doctor::Severity::Skipped),
            available_repairs: report.available_repairs(),
            report,
        }
    }
}

/// Render a report grouped by tier, with each failure's remedy indented under
/// it — so the fix is never more than one line away from the problem.
fn print_report(report: &doctor::Report, fixed: bool, fix_cmd: &str) {
    for tier in [
        doctor::Tier::Connection,
        doctor::Tier::Host,
        doctor::Tier::Stack,
        doctor::Tier::Site,
    ] {
        let checks: Vec<_> = report.checks.iter().filter(|c| c.tier == tier).collect();
        let Some(first) = checks.first() else {
            continue;
        };
        println!("\n{}", style::heading(&tier_title(tier, &first.on)));
        for check in checks {
            print_check(check);
        }
    }

    if !report.actions.is_empty() {
        println!("\n{}", style::heading("Repairs"));
        for action in &report.actions {
            let mark = if action.succeeded {
                style::success("fixed")
            } else {
                style::warn("failed")
            };
            println!("  {mark}  {}", action.description);
            if !action.succeeded {
                if let Some(detail) = &action.detail {
                    println!("        {}", style::warn(&first_line(detail)));
                }
            }
        }
    }

    println!();
    if let Some(rule) = style::rule() {
        println!("{rule}");
    }

    let failures = report.count(doctor::Severity::Fail);
    let warnings = report.count(doctor::Severity::Warn);
    if failures == 0 {
        let headline = if warnings > 0 {
            format!("No problems found ({warnings} advisory).")
        } else {
            "Everything checks out.".to_string()
        };
        println!("{}", style::success(&headline));
        return;
    }

    println!(
        "{}",
        style::warn(&format!(
            "{failures} problem{} still need{} attention.",
            if failures == 1 { "" } else { "s" },
            if failures == 1 { "s" } else { "" },
        ))
    );
    // Only advertise --fix when it would actually do something, and only when
    // the user hasn't just run it — "run --fix" after --fix already failed is
    // noise, not advice.
    if !fixed && !report.available_repairs().is_empty() {
        println!("Run `{fix_cmd}` to repair what can be repaired automatically.");
    }
}

/// A report section's heading, naming the machine its checks ran on (m2-9).
fn tier_title(tier: doctor::Tier, on: &doctor::HostTier) -> String {
    use doctor::{HostTier, Tier};
    match (tier, on) {
        (Tier::Connection, _) => "Reaching the server (from this machine)".to_string(),
        (Tier::Host, HostTier::Local) => "Your machine".to_string(),
        (Tier::Stack, HostTier::Local) => "The appliance".to_string(),
        (Tier::Site, HostTier::Local) => "Your site".to_string(),
        (Tier::Host, HostTier::Remote(host)) => format!("The server ({host})"),
        (Tier::Stack, HostTier::Remote(host)) => format!("The appliance on {host}"),
        (Tier::Site, HostTier::Remote(host)) => format!("Your site on {host}"),
    }
}

fn print_check(check: &doctor::Check) {
    // Marks must stay distinguishable with colour off, so each severity gets its
    // own glyph: a checked box passes, an empty one fails (the existing
    // `style::mark` convention), `!` advises, `-` was never reached.
    let (mark, styled_label) = match check.severity {
        doctor::Severity::Ok => (style::mark(true), check.label.to_string()),
        doctor::Severity::Warn => ("[!]".to_string(), style::warn(&check.label)),
        doctor::Severity::Fail => (style::mark(false), style::danger(&check.label)),
        doctor::Severity::Skipped => ("[-]".to_string(), check.label.to_string()),
    };
    match &check.detail {
        Some(d) if check.severity == doctor::Severity::Ok => {
            println!("  {mark} {styled_label} — {d}")
        }
        Some(d) => println!("  {mark} {styled_label} — {}", first_line(d)),
        None => println!("  {mark} {styled_label}"),
    }
    if let Some(remedy) = &check.remedy {
        println!("      {}", style::warn(&format!("→ {remedy}")));
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Accept a channel name, rejecting `pinned` — it's a state you land in by
/// naming an image, not one you can switch to.
fn parse_channel(name: &str) -> Result<Channel> {
    match Channel::parse(name) {
        Some(Channel::Pinned) | None => anyhow::bail!(
            "unknown channel {name:?} — use `stable` (released versions) or `edge` \
             (every build off main). To run one specific image, pass `--image` to \
             `atelier app install` instead."
        ),
        Some(c) => Ok(c),
    }
}

/// `atelier app channel [stable|edge]` — show or switch the image stream.
/// `atelier app update` — plan the route, show it if it has more than one step,
/// then walk it.
///
/// The plan is shown BEFORE anything is pulled. A stepped upgrade runs a migration
/// per hop and can take several minutes, and the operator's mental model of
/// "update" is one pull and one restart — landing them in a five-minute sequence
/// through versions they never chose, without saying so first, is how a routine
/// update becomes a support conversation.
fn update_cmd(stack: &Stack, to: Option<String>, yes: bool) -> Result<()> {
    // A channel update (no explicit target) can be conclusively "nothing new" — and
    // an update that always claims success can never tell the operator so. On a
    // definite answer, short-circuit; fall through when the check is inconclusive.
    // A pinned `--to` always proceeds — the operator named a specific image.
    let plan = match &to {
        Some(target) => ops::plan_upgrade(stack, Some(target)),
        None => {
            let check = ops::check_update(stack);
            if check.update_available == Some(false) {
                let at = check.current_version.unwrap_or_else(|| stack.image());
                println!("{}", style::success(&format!("Already up to date ({at}).")));
                return Ok(());
            }
            check.plan.unwrap_or_else(|| ops::plan_upgrade(stack, None))
        }
    };

    // A site's database only migrates forward: an older release run against a schema
    // a newer one already migrated breaks the site with no visible cause. Refuse a
    // PROVEN backwards move outright — the way back to an older version is a restore,
    // not a downgrade. (`is_downgrade` is false whenever either version is unknown,
    // so a fork/local-build target or an unstamped install still passes through to
    // converge's own refusal.)
    if plan.is_downgrade() {
        let from = plan
            .from
            .map(|v| v.to_string())
            .unwrap_or_else(|| "the installed version".to_string());
        let to_v = plan
            .target_version()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "an older version".to_string());
        anyhow::bail!(
            "Refusing to move down from {from} to {to_v}. A site's database only \
             migrates forward — running an older release against it breaks the site \
             with no way back. To return to {to_v}, restore a backup taken on that \
             version instead: `atelier data restore <file>`."
        );
    }

    // Say what's about to change before touching anything. On a stepped route the
    // printed plan below already shows from-and-hops, so this line is just the
    // single-hop case.
    if !plan.is_stepped() {
        if let (Some(f), Some(t)) = (plan.from, plan.target_version()) {
            if f != t {
                println!("{}", style::heading(&format!("Updating {f} → {t}")));
            }
        }
    }

    if plan.is_stepped() {
        println!(
            "{}",
            style::heading("This upgrade takes more than one step")
        );
        print_route(&plan);
        println!(
            "\nEach step pulls its image, migrates, and is health-checked before the \
             next one starts;\na step that fails rolls its own database back. A full \
             snapshot is taken first."
        );
        if let Some(problem) = &plan.problem {
            println!("\n{}", style::warn(problem));
        }
        if !confirm_change(&Target::Local, "\nWalk the whole route now?", yes)? {
            println!("{}", style::warn("Aborted — nothing was changed."));
            return Ok(());
        }
    } else if let Some(problem) = &plan.problem {
        // Worth saying even on a single hop: it means the route wasn't verified,
        // and the operator should know that before a long run rather than after.
        println!("{}", style::warn(problem));
    }

    let pinned_to = to.is_some();
    if ops::apply_upgrade(stack, &plan, &mut CliReporter::default())? {
        done_banner("Update complete.", &stack.console_url());
    } else {
        pending_banner(
            "Update applied — still finishing boot.",
            &stack.console_url(),
        );
    }
    if pinned_to {
        println!(
            "{}",
            style::warn(&format!(
                "You asked for a specific version, so this install is now pinned to \
                 {} and no further updates will arrive. \
                 `atelier app channel stable` follows releases again.",
                stack.image()
            ))
        );
    }
    Ok(())
}

/// Print a route as a numbered list, one line per hop.
fn print_route(plan: &ops::UpgradePlan) {
    let from = plan
        .from
        .map(|v| v.to_string())
        .unwrap_or_else(|| "an unknown version".to_string());
    println!("\n  from {from}");
    for (i, step) in plan.steps.iter().enumerate() {
        let label = match step.version {
            Some(v) => v.to_string(),
            None => step.image.clone(),
        };
        match &step.reason {
            Some(reason) => println!("  {}. {label}   ({reason})", i + 1),
            None => println!("  {}. {label}", i + 1),
        }
    }
}

fn channel_cmd(stack: &Stack, channel: Option<String>, now: bool, yes: bool) -> Result<()> {
    let current = stack.channel();
    let Some(name) = channel else {
        println!("  Channel:  {}", current.describe());
        println!("  Image:    {}", stack.image());
        match current {
            // Stable is the whole story on a supported install — edge is not
            // advertised as somewhere to go next.
            Channel::Stable => println!("\nThis is the supported channel."),
            Channel::Edge => println!(
                "\nEdge is unreleased work off main, for testing. To follow released \
                 versions only: `atelier app channel stable`."
            ),
            Channel::Pinned => println!(
                "\nThis image never moves, so updates won't arrive. \
                 `atelier app channel stable` follows releases again."
            ),
        }
        return Ok(());
    };

    let target = parse_channel(&name)?;
    if target == current {
        println!("{} {}", style::success("Already on"), target.describe());
        return Ok(());
    }

    // Going TO edge is a deliberate step off the supported path, so it asks. Edge
    // is not offered in the GUI at all; the CLI keeps it (our own test lanes and
    // anyone testing a fix need it) behind a confirmation that says what it costs.
    if target == Channel::Edge {
        println!(
            "{}",
            style::warn(
                "Edge is unreleased work off main, rebuilt on every merge. It is for \
                 testing, not for a site you care about: it can break, it can be ahead \
                 of every release, and moving back to stable later hands an older \
                 codebase a database a newer one already migrated — which only \
                 migrates forward. Take a backup first: `atelier data backup`."
            )
        );
        if !confirm_change(&Target::Local, "Follow unreleased edge builds anyway?", yes)? {
            println!("{}", style::warn("Aborted."));
            return Ok(());
        }
    }

    // Edge is built from every merge, so it can be ahead of the newest release:
    // moving to stable can hand an older codebase a database a newer one already
    // migrated, and Drupal only migrates forward. Say so before doing it.
    if current == Channel::Edge && target == Channel::Stable {
        println!(
            "{}",
            style::warn(
                "Edge builds can be ahead of the newest release, and a site's database \
                 only migrates forward. The appliance snapshots and rolls back if the \
                 switch fails, but take your own backup first if the site matters: \
                 `atelier data backup`."
            )
        );
        if !confirm_change(&Target::Local, "Switch to released versions anyway?", yes)? {
            println!("{}", style::warn("Aborted."));
            return Ok(());
        }
    }

    let (image, ready) = ops::switch_channel(stack, target, now, &mut CliReporter::default())?;
    match ready {
        Some(true) => done_banner(
            &format!("Now on {}.", target.describe()),
            &stack.console_url(),
        ),
        Some(false) => pending_banner(
            &format!("Switched to {} — still finishing boot.", target.name()),
            &stack.console_url(),
        ),
        None => println!(
            "{} {image}\nRun `atelier app update` to pull it (or re-run with --now).",
            style::success(&format!("Channel set to {}:", target.name())),
        ),
    }
    Ok(())
}

fn status(stack: &Stack, json: bool) -> Result<()> {
    let st = ops::status(stack);
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    // Name the site once there is a list to name it from — a single-site
    // install that never touched `atelier sites` reads exactly as before.
    let registry_exists = sites::user_home()
        .map(|uh| sites::registry_path(&uh).is_file())
        .unwrap_or(false);
    if let (true, Some(slug), Some(label)) = (registry_exists, &stack.slug, &stack.label) {
        println!("  Site:     {label} ({slug})");
    }
    print_status(&st);
    Ok(())
}

/// The human rendering of a [`ops::Status`], local or remote.
fn print_status(st: &ops::Status) {
    line("Installed", st.installed);
    line("Running", st.running);
    line("Console reachable", st.reachable);
    println!("  Console:  {}", style::url(&st.console_url));
    if let Some(version) = &st.version {
        println!("  Version:  {version}");
    }
    println!("  Image:    {}", st.image);
    println!("  Channel:  {}", st.channel.describe());
    if !st.installed {
        println!(
            "\n{}",
            style::warn("Not installed yet — run `atelier install`.")
        );
    }
}

// --- remote sites (M2) -------------------------------------------------------

/// The registered remote site a command resolved to, if any. `None` — a local
/// site, a hand-rolled `ATELIER_HOME`, an unregistered `~/.atelier` — keeps
/// the local code path untouched.
fn remote_site(resolution: &sites::Resolution) -> Option<&sites::Site> {
    resolution.site.as_ref().filter(|s| s.is_remote())
}

/// How a command runs on a remote site (m2-7, D-04).
#[derive(Debug)]
enum Route {
    /// `app status [--json]`.
    Status { json: bool },
    /// `app check-update [--json]`.
    CheckUpdate { json: bool },
    /// `app update`: plan + confirm here, then the server walks it.
    Update { to: Option<String>, yes: bool },
    /// `data restore`: confirm here, then the server restores its own file,
    /// or one uploaded from here.
    Restore {
        file: String,
        yes: bool,
        other_site: bool,
        on_server: bool,
    },
    /// `data backup`: on the server, then (unless `copy` is off) home.
    Backup {
        label: Option<String>,
        copy: bool,
        json: bool,
    },
    /// `data list`: the server's backups and the copies on this machine.
    BackupList,
    /// `app open`: the recorded domain, else an SSH tunnel.
    Open,
    /// `site snapshots [--json]`.
    Snapshots { json: bool },
    /// `doctor [--fix] [--json]`: Manager-side checks + the server's report.
    Doctor { fix: bool, json: bool },
    /// Everything else: the same subcommand on the server, streamed.
    Run(remote::Action),
}

/// Map a parsed command to its remote route, or refuse it with a message that
/// names the host. Pure, so the whole table is unit-tested.
fn remote_route(command: Command, host: &str, slug: &str) -> Result<Route> {
    use remote::Action;
    let on_server = |what: &str| {
        anyhow::anyhow!(
            "`atelier {what}` isn't available for remote sites yet — site `{slug}` is on {host}. \
             Run it on the server: `ssh {host} atelier --site <SLUG> {what}`."
        )
    };
    let local_only = |what: &str, why: &str| {
        anyhow::anyhow!("`atelier {what}` {why}, and site `{slug}` is on {host}.")
    };
    Ok(match command {
        Command::Sites { .. } => unreachable!("handled before resolving a site"),
        Command::Doctor { fix, json } => Route::Doctor { fix, json },
        Command::App { command } => match command {
            AppCommand::Status { json } => Route::Status { json },
            AppCommand::CheckUpdate { json } => Route::CheckUpdate { json },
            AppCommand::Update { to, yes, .. } => Route::Update { to, yes },
            AppCommand::Start => Route::Run(Action::Start),
            AppCommand::Stop => Route::Run(Action::Stop),
            AppCommand::Install { .. } => {
                return Err(local_only(
                    "app install",
                    "installs on this machine — the Manager only attaches to a site already \
                     running on a server",
                ))
            }
            AppCommand::Open => Route::Open,
            AppCommand::View => {
                return Err(local_only(
                    "app view",
                    "opens the site in a browser here, at the server's own local address",
                ))
            }
            AppCommand::Reinstall { .. } => return Err(on_server("app reinstall")),
            AppCommand::Down { .. } => return Err(on_server("app down")),
            AppCommand::Channel { .. } => return Err(on_server("app channel")),
            AppCommand::Logs { .. } => return Err(on_server("app logs")),
            AppCommand::Password { .. } => return Err(on_server("app password")),
        },
        Command::Site { command } => match command {
            SiteCommand::Export {
                out,
                base_url,
                zip,
                include_config,
                include_users,
                skip_link_check,
            } => Route::Run(Action::Export(ops::ExportOptions {
                out,
                base_url,
                zip,
                include_config,
                include_users,
                skip_link_check,
            })),
            SiteCommand::Freeze {
                label,
                keep,
                no_serve,
                force,
                base_url,
            } => Route::Run(Action::Freeze(ops::FreezeOptions {
                label,
                keep,
                no_serve,
                force,
                base_url,
            })),
            SiteCommand::Snapshots { json } => Route::Snapshots { json },
            SiteCommand::Use { id } => Route::Run(Action::UseSnapshot { id }),
            SiteCommand::Live => Route::Run(Action::Live),
            SiteCommand::Prune { keep } => Route::Run(Action::Prune { keep }),
        },
        Command::Data { command } => match command {
            DataCommand::Backup {
                label,
                json,
                no_copy,
            } => Route::Backup {
                label,
                copy: !no_copy,
                json,
            },
            DataCommand::List => Route::BackupList,
            DataCommand::Restore {
                file,
                yes,
                other_site,
                on_server,
            } => Route::Restore {
                file: file
                    .to_str()
                    .context("the backup path isn't valid UTF-8")?
                    .to_string(),
                yes,
                other_site,
                on_server,
            },
        },
        Command::Ai { .. } => return Err(on_server("ai model")),
        Command::Pack { .. } => {
            return Err(local_only(
                "pack",
                "develops a pack against a dev stack on this machine",
            ))
        }
        Command::Mcp => {
            return Err(local_only(
                "mcp",
                "serves tools for a dev stack on this machine",
            ))
        }
    })
}

/// Print a remote command's output as it arrives: stdout to stdout, stderr to
/// stderr, so redirection behaves as it does for the local command.
fn relay(pipe: aincient_core::docker::Pipe, line: &str) {
    match pipe {
        aincient_core::docker::Pipe::Stdout => println!("{line}"),
        aincient_core::docker::Pipe::Stderr => eprintln!("{line}"),
    }
}

/// A command on a remote site: run by the server's own `atelier` over SSH
/// (D-01), non-interactively, after any confirmation was asked here.
fn run_remote(command: Command, site: &sites::Site) -> Result<()> {
    let remote = site.remote.as_ref().expect("caller checked");
    let host = remote.host.as_str();
    let route = remote_route(command, host, &site.slug)?;
    // Doctor opens its own session: a failed login is a finding, not an error.
    if let Route::Doctor { fix, json } = route {
        if !json {
            println!("Checking {} on {host}…", site.slug);
        }
        let report = remote::doctor(site, fix, &mut relay)?;
        let fix_cmd = format!("atelier --site {} doctor --fix", site.slug);
        return finish_doctor(&report, fix, json, &fix_cmd);
    }
    let target = Target::Remote {
        host,
        slug: &site.slug,
    };
    let session = remote_session(site, remote::EngineOptions::from_env())?;
    match route {
        Route::Doctor { .. } => unreachable!("handled above"),
        Route::Status { json } => {
            // The URLs the user can open from HERE (m2-4): the recorded
            // domain, or none — never the server's own localhost.
            let (st, route) = remote::status_for_here(remote, session.status()?);
            let via = match route {
                remote::ConsoleRoute::Domain { .. } => "domain",
                remote::ConsoleRoute::Tunnel { .. } => "tunnel",
            };
            if json {
                let mut value = serde_json::to_value(&st)?;
                value["console_via"] = via.into();
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            println!(
                "  Site:     {} ({}) — `{}` on {}",
                site.label, site.slug, remote.slug, host
            );
            print_status(&st);
            if via == "tunnel" {
                println!(
                    "  Console:  via an SSH tunnel — `atelier --site {} app open`",
                    site.slug
                );
            }
            Ok(())
        }
        Route::Open => remote_open(&session, site),
        Route::Backup { label, copy, json } => {
            let home = copy.then(|| remote::local_backups_dir(site));
            let got = session.backup(label, home.as_deref(), &mut relay)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "server": got.server,
                        "local": got.local,
                    }))?
                );
                return Ok(());
            }
            println!(
                "{} {} on {host} ({:.1} MB)",
                style::success("Backup written to"),
                got.server.path,
                got.server.size as f64 / 1_048_576.0
            );
            match got.local {
                Some(local) => println!(
                    "{} {} (size and sha256 match the server's)",
                    style::success("Copied to"),
                    local.display()
                ),
                None => println!("Kept on {host} only (--no-copy)."),
            }
            Ok(())
        }
        Route::BackupList => {
            println!("{}", style::heading(&format!("On {host}:")));
            session.run_action(&remote::Action::BackupList, &mut relay)?;
            let dir = remote::local_backups_dir(site);
            println!(
                "\n{}",
                style::heading(&format!("On this machine ({}):", dir.display()))
            );
            let here = ops::list_backups(&site.stack());
            if here.is_empty() {
                println!(
                    "  none yet — `atelier --site {} data backup` copies one here",
                    site.slug
                );
            }
            for b in here {
                println!(
                    "  {}  ({:.1} MB)",
                    b.name,
                    b.size_bytes as f64 / 1_048_576.0
                );
            }
            Ok(())
        }
        Route::CheckUpdate { json } => {
            let check = session.check_update()?;
            print_check_update(&check, Channel::of_image(&check.image), json)
        }
        Route::Snapshots { json } => {
            let list = session.snapshots()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                print_snapshots(&list);
            }
            Ok(())
        }
        Route::Update { to, yes } => remote_update(&session, &target, to, yes),
        Route::Restore {
            file,
            yes,
            other_site,
            on_server,
        } => {
            let local = PathBuf::from(&file);
            if !on_server && local.is_file() {
                // A backup on this machine (m2-6): refused before a byte moves
                // if it is another site's; then uploaded, verified, restored.
                if let Some(note) = session
                    .check_restore_identity(&local, other_site)?
                    .message()
                {
                    println!("{}", style::warn(note));
                }
                println!(
                    "{file} is on this machine — it is uploaded to {host} and restored there."
                );
                if !confirm_change(&target, &restore_question(&file), yes)? {
                    println!("{}", style::warn("Aborted."));
                    return Ok(());
                }
                session.restore_upload(&local, other_site, &mut relay)?;
                println!("{}", style::success("Restore complete."));
                return Ok(());
            }
            println!(
                "{}",
                style::warn(&format!(
                    "{file} is a path on {host} — the server restores its own copy."
                ))
            );
            if !confirm_change(&target, &restore_question(&file), yes)? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            session.run_action(&remote::Action::Restore { file, other_site }, &mut relay)
        }
        Route::Run(action) => {
            session.run_action(&action, &mut relay)?;
            if let remote::Action::Export(_) = action {
                println!("The export is on {host}.");
            }
            Ok(())
        }
    }
}

/// `app update` on a remote site. Without `--to`, the server's own
/// `check-update` gives the route first, so "already up to date" costs no
/// update run and a stepped route is shown and confirmed HERE. `--yes` goes
/// to the server only when that confirmation happened (or the user gave it):
/// otherwise a server that finds a stepped route declines it rather than wait.
fn remote_update(
    session: &remote::Session<remote::Ssh>,
    target: &Target,
    to: Option<String>,
    yes: bool,
) -> Result<()> {
    let mut confirmed = yes;
    if to.is_none() {
        let check = session.check_update()?;
        if check.update_available == Some(false) {
            let at = check.current_version.as_deref().unwrap_or(&check.image);
            println!("{}", style::success(&format!("Already up to date ({at}).")));
            return Ok(());
        }
        if let Some(plan) = check.plan.as_ref().filter(|p| p.is_stepped()) {
            println!(
                "{}",
                style::heading("This upgrade takes more than one step")
            );
            print_route(plan);
            if let Some(problem) = &plan.problem {
                println!("\n{}", style::warn(problem));
            }
            if !confirm_change(target, "\nWalk the whole route now?", yes)? {
                println!("{}", style::warn("Aborted — nothing was changed."));
                return Ok(());
            }
            confirmed = true;
        }
    }
    session.run_action(&remote::Action::Update { to, yes: confirmed }, &mut relay)
}

/// `app open` on a remote site (m2-4, D-02): the recorded domain, or an SSH
/// tunnel held open until Ctrl-C (which ends ssh along with us).
fn remote_open(session: &remote::Session<remote::Ssh>, site: &sites::Site) -> Result<()> {
    let access = remote::console_access_in(session, site)?;
    match access.tunnel {
        None => {
            println!("Opening {}", style::url(&access.url));
            ops::open_url(&access.url)
        }
        Some(mut tunnel) => {
            println!(
                "{} {} → {} on {}",
                style::success("Tunnel open:"),
                style::url(&access.url),
                site.slug,
                session.host()
            );
            println!("Sign in there. Keep this running while you use it; Ctrl-C closes it.");
            if let Err(e) = ops::open_url(&access.url) {
                println!("{} {e:#}", style::warn("Couldn't open a browser:"));
            }
            tunnel.wait()
        }
    }
}

/// Connect to a remote site's engine. When the server's `atelier` is older
/// than this one, offer (in a terminal) to copy ours to `~/.local/bin`.
fn remote_session(
    site: &sites::Site,
    opts: remote::EngineOptions,
) -> Result<remote::Session<remote::Ssh>> {
    use std::io::IsTerminal;
    match remote::open(site, &opts) {
        Err(e) if e.downcast_ref::<remote::EngineTooOld>().is_some() => {
            println!("{}", style::warn(&e.to_string()));
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "not copying it — this isn't an interactive terminal. Re-run in a terminal \
                     to accept the copy."
                );
            }
            if !confirm("Copy it now?", false)? {
                anyhow::bail!("nothing changed on the server");
            }
            remote::open(
                site,
                &remote::EngineOptions {
                    replace_older: true,
                    ..opts
                },
            )
        }
        other => other,
    }
}

/// `atelier sites add <SLUG> --host <TARGET>`: attach to a running site on a
/// server (U-03). The registry is written only after the server answered and
/// reported the site running.
fn sites_add_remote(
    uh: &std::path::Path,
    slug: &str,
    label: Option<&str>,
    host: &str,
    remote_site: Option<&str>,
    guided: bool,
    engine_binary: Option<PathBuf>,
    domain: Option<String>,
) -> Result<()> {
    use std::io::IsTerminal;
    if !sites::is_valid_host(host) {
        anyhow::bail!("`{host}` is not a usable SSH target (an alias, host or user@host)");
    }
    if let Some(d) = domain.as_deref().filter(|d| !sites::is_valid_domain(d)) {
        anyhow::bail!("`{d}` is not a usable domain — a host name, or https://host[:port]");
    }
    let key = if guided {
        sites::KeyMode::Guided
    } else {
        sites::KeyMode::Ssh
    };
    let mut site = sites::Site::new_remote(uh, slug, label, host, remote_site, key);
    if let Some(remote) = site.remote.as_mut() {
        remote.domain = domain;
    }
    if guided {
        let key = ssh_key::ensure_guided_key(&ssh_key::Keychain, host)?;
        println!(
            "{} the Manager's key for {host} {} in your OS keychain.",
            style::success(if key.created { "Created" } else { "Found" }),
            if key.created { "and stored it" } else { "is" },
        );
        println!("\nAdd this line to ~/.ssh/authorized_keys on {host}:\n");
        println!("  {}\n", key.public_line);
        println!("For example, logged in on the server, run:\n");
        println!("  {}\n", ssh_key::authorize_command(&key.public_line));
        if std::io::stdin().is_terminal() {
            print!("Press Enter once it is added (Ctrl-C to stop)… ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
        } else if key.created {
            anyhow::bail!(
                "not an interactive terminal — add the key on the server, then run this \
                 command again (the key stays in your keychain and is reused)"
            );
        }
    }
    let opts = remote::EngineOptions {
        binary: engine_binary.or_else(|| remote::EngineOptions::from_env().binary),
        ..Default::default()
    };
    println!("Checking {host}…");
    let added = sites::add_remote_site(uh, site, |site| {
        let session = remote_session(site, opts)?;
        let st = session.attach_check()?;
        println!(
            "{} {host} runs site `{}`{} (engine {}).",
            style::success("Reached"),
            site.remote.as_ref().map(|r| r.slug.as_str()).unwrap_or(""),
            st.version
                .as_deref()
                .map(|v| format!(", version {v}"))
                .unwrap_or_default(),
            session.engine()
        );
        Ok(())
    })
    .context("nothing was added")?;
    println!(
        "{} {} ({}) — on {host}",
        style::success("Added site"),
        added.site.slug,
        added.site.label,
    );
    if added.activated {
        println!("It is the active site: commands without --site use it.");
    } else {
        println!("Use it with `atelier --site {slug} app status`.");
    }
    Ok(())
}

fn check_update(stack: &Stack, json: bool) -> Result<()> {
    print_check_update(&ops::check_update(stack), stack.channel(), json)
}

/// Render an update check, local or remote.
fn print_check_update(check: &ops::UpdateCheck, channel: Channel, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(check)?);
        return Ok(());
    }
    // Name the versions when the image says what it is (DECISIONS 0308) — on a
    // rolling tag like `:edge` the digest alone tells the user nothing.
    let from = check.current_version.as_deref().unwrap_or("unknown");
    match check.update_available {
        Some(true) => {
            println!(
                "{} for {}.\n{} → {}",
                style::heading("An update is available"),
                check.image,
                from,
                check.latest_version.as_deref().unwrap_or("a newer build"),
            );
            // A stepped route is the thing to know before starting, not during:
            // it is longer than expected and it passes through versions nobody
            // asked for. `update` asks again before it walks it.
            match check.plan.as_ref().filter(|p| p.is_stepped()) {
                Some(plan) => {
                    println!(
                        "\n{}",
                        style::warn(
                            "That release can't migrate a site this old directly, so the \
                             update goes through:"
                        )
                    );
                    print_route(plan);
                    println!("\nRun `atelier app update` — it walks the whole route.");
                }
                None => println!("Run `atelier app update`."),
            }
        }
        // On a pinned image "up to date" is true but useless — the tag can't move,
        // so no update will ever arrive through it. Say which it is.
        Some(false) if channel == Channel::Pinned => println!(
            "{} {} ({}) — a pinned image, so updates never arrive here. \
             `atelier app channel stable` follows releases.",
            style::success("You're on"),
            check.image,
            from,
        ),
        Some(false) => println!(
            "{} {} ({}).",
            style::success("You're on the latest"),
            check.image,
            from,
        ),
        // Not "are you logged in?" — the image is public; the real cause is one of
        // four, and `problem` names which.
        None => println!(
            "{}",
            style::warn(
                check
                    .problem
                    .as_deref()
                    .unwrap_or("Couldn't check for updates.")
            )
        ),
    }
    Ok(())
}

fn list_backups(stack: &Stack) -> Result<()> {
    let backups = ops::list_backups(stack);
    if backups.is_empty() {
        println!(
            "{}",
            style::warn("No backups yet. Create one with `atelier data backup`.")
        );
        return Ok(());
    }
    for b in backups {
        println!(
            "  {}  ({:.1} MB)",
            b.name,
            b.size_bytes as f64 / 1_048_576.0
        );
    }
    Ok(())
}

fn show_login(stack: &Stack) {
    if let Some(pw) = ops::admin_password(stack) {
        println!("Login:    admin / {pw}  (change this after first login)");
    }
}

/// Which machine a change is about to happen on.
enum Target<'a> {
    /// A site on this machine.
    Local,
    /// A site on a server: `host` is the ssh target, `slug` its name here.
    #[allow(dead_code)] // read by the D-07 confirms (m2-8)
    Remote { host: &'a str, slug: &'a str },
}

/// THE confirmation before a site is changed — every destructive or
/// route-walking action, local or remote, asks through here, always BEFORE the
/// work starts (for a remote site: before the ssh call, so the server never
/// waits on a prompt). Today both targets ask the same y/N; the next slice
/// (m2-8, D-07) makes the remote one name the host and ask for the typed slug
/// on a live site — here, and nowhere else.
fn confirm_change(target: &Target, question: &str, assume_yes: bool) -> Result<bool> {
    match target {
        Target::Local | Target::Remote { .. } => confirm(question, assume_yes),
    }
}

/// The restore question, shared by the local and remote paths.
fn restore_question(file: &str) -> String {
    format!(
        "Restore will REPLACE the current database (and files, for a .tar.gz snapshot) with \
         {file}. Continue?"
    )
}

fn confirm(prompt: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    print!("{prompt} [y/N]: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn line(label: &str, ok: bool) {
    println!("  {} {label}", style::mark(ok));
}

#[cfg(test)]
mod tests {
    use super::*;
    use aincient_core::doctor::{HostTier, Tier};

    const HOST: &str = "deploy@example.com";

    fn parse(args: &[&str]) -> Command {
        let mut argv = vec!["atelier", "--site", "prod"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap().command
    }

    fn route(args: &[&str]) -> Result<Route> {
        remote_route(parse(args), HOST, "prod")
    }

    /// The server argv a plain streamed route sends.
    fn argv(args: &[&str]) -> String {
        match route(args).unwrap() {
            Route::Run(action) => action.argv().join(" "),
            other => panic!("{args:?} is not a plain run: {other:?}"),
        }
    }

    #[test]
    fn every_site_action_routes_to_the_same_subcommand_on_the_server() {
        assert_eq!(argv(&["app", "start"]), "app start");
        assert_eq!(argv(&["app", "stop"]), "app stop");
        assert_eq!(
            argv(&["site", "export", "--out", "/srv/x", "--zip"]),
            "site export --out /srv/x --zip"
        );
        assert_eq!(
            argv(&["site", "freeze", "--label", "v1", "--no-serve"]),
            "site freeze --label v1 --no-serve"
        );
        assert_eq!(argv(&["site", "live"]), "site live");
        assert_eq!(argv(&["site", "use", "s-1"]), "site use s-1");
        assert_eq!(argv(&["site", "prune"]), "site prune --keep 10");

        assert!(matches!(
            route(&["app", "status", "--json"]).unwrap(),
            Route::Status { json: true }
        ));
        assert!(matches!(
            route(&["app", "check"]).unwrap(),
            Route::CheckUpdate { json: false }
        ));
        assert!(matches!(
            route(&["site", "snapshots", "--json"]).unwrap(),
            Route::Snapshots { json: true }
        ));
        assert!(matches!(
            route(&["doctor", "--fix"]).unwrap(),
            Route::Doctor {
                fix: true,
                json: false
            }
        ));
        match route(&["app", "update", "--to", "0.21.0"]).unwrap() {
            Route::Update { to, yes } => {
                assert_eq!(to.as_deref(), Some("0.21.0"));
                assert!(!yes);
            }
            other => panic!("{other:?}"),
        }
        match route(&["data", "restore", "/srv/b.tar.gz", "-y"]).unwrap() {
            Route::Restore {
                file,
                yes,
                other_site,
                on_server,
            } => {
                assert_eq!(file, "/srv/b.tar.gz");
                assert!(yes && !other_site && !on_server);
                // What the server then runs: always non-interactive.
                let sent = remote::Action::Restore { file, other_site }
                    .argv()
                    .join(" ");
                assert_eq!(sent, "data restore --yes -- /srv/b.tar.gz");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backups_come_home_unless_no_copy_and_restore_carries_its_flags() {
        for (args, want_copy, want_json) in [
            (&["data", "backup", "--label", "before"][..], true, false),
            (&["data", "export"], true, false),
            (&["data", "backup", "--no-copy", "--json"], false, true),
        ] {
            match route(args).unwrap() {
                Route::Backup { copy, json, .. } => {
                    assert_eq!((copy, json), (want_copy, want_json), "{args:?}")
                }
                other => panic!("{args:?}: {other:?}"),
            }
        }
        // What the Manager then asks the server for: always the JSON answer.
        let sent = remote::Action::Backup {
            label: Some("before".into()),
            json: true,
        };
        assert_eq!(sent.argv().join(" "), "data backup --label before --json");
        assert!(matches!(
            route(&["data", "list"]).unwrap(),
            Route::BackupList
        ));
        match route(&["data", "restore", "b.tar.gz", "--other-site", "--on-server"]).unwrap() {
            Route::Restore {
                other_site,
                on_server,
                ..
            } => assert!(other_site && on_server),
            other => panic!("{other:?}"),
        }
        assert!(matches!(route(&["app", "open"]).unwrap(), Route::Open));
    }

    #[test]
    fn sites_add_takes_a_domain_only_with_a_host() {
        let cli = Cli::try_parse_from([
            "atelier",
            "sites",
            "add",
            "prod",
            "--host",
            "h",
            "--domain",
            "cms.example.com",
        ])
        .unwrap();
        match cli.command {
            Command::Sites {
                command: SitesCommand::Add { domain, .. },
            } => assert_eq!(domain.as_deref(), Some("cms.example.com")),
            _ => panic!("not sites add"),
        }
        assert!(
            Cli::try_parse_from(["atelier", "sites", "add", "blog", "--domain", "x.com"]).is_err()
        );
    }

    #[test]
    fn local_only_and_not_yet_commands_are_refused_naming_the_host() {
        for args in [
            &["app", "install"][..],
            &["app", "view"],
            &["pack", "dev"],
            &["pack", "validate"],
            &["mcp"],
            &["app", "reinstall", "-y"],
            &["app", "down", "--wipe"],
            &["app", "channel", "edge"],
            &["app", "logs"],
            &["app", "password"],
            &["ai", "model", "list"],
        ] {
            let err = route(args).unwrap_err().to_string();
            assert!(
                err.contains(HOST) && err.contains("`prod`"),
                "{args:?}: {err}"
            );
        }
    }

    #[test]
    fn local_sites_keep_the_local_path() {
        let uh = std::path::Path::new("/home/me");
        let local = sites::Site {
            slug: "blog".into(),
            label: "Blog".into(),
            home: uh.join(".atelier/sites/blog"),
            project: "atelier-blog".into(),
            remote: None,
        };
        let resolution = |site: Option<sites::Site>| sites::Resolution {
            home: uh.join(".atelier/sites/blog"),
            site,
            source: sites::Source::Flag,
        };
        assert!(remote_site(&resolution(Some(local))).is_none());
        assert!(
            remote_site(&resolution(None)).is_none(),
            "unregistered home"
        );
        let remote = sites::Site::new_remote(uh, "prod", None, HOST, None, sites::KeyMode::Ssh);
        assert_eq!(
            remote_site(&resolution(Some(remote))).map(|s| s.slug.as_str()),
            Some("prod")
        );
    }

    #[test]
    fn one_confirmation_serves_both_targets() {
        let remote = Target::Remote {
            host: HOST,
            slug: "prod",
        };
        assert!(confirm_change(&Target::Local, "q?", true).unwrap());
        assert!(confirm_change(&remote, "q?", true).unwrap());
        assert!(restore_question("/srv/b.tar.gz").contains("/srv/b.tar.gz"));
    }

    #[test]
    fn doctor_sections_name_the_machine_they_ran_on() {
        let server = HostTier::Remote(HOST.into());
        assert_eq!(tier_title(Tier::Host, &HostTier::Local), "Your machine");
        assert_eq!(tier_title(Tier::Site, &HostTier::Local), "Your site");
        assert_eq!(
            tier_title(Tier::Host, &server),
            "The server (deploy@example.com)"
        );
        assert_eq!(
            tier_title(Tier::Stack, &server),
            "The appliance on deploy@example.com"
        );
        assert!(tier_title(Tier::Connection, &HostTier::Local).contains("this machine"));
    }
}
