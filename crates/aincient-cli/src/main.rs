//! `atelier` — the command-line front door to an Atelier CMS appliance.
//!
//! A thin presentation layer over [`aincient_core`]: it parses arguments, confirms
//! destructive actions, and formats output. All behaviour lives in the core crate,
//! shared verbatim with the Tauri manager GUI.

use std::io::Write;
use std::path::PathBuf;

use aincient_core::{doctor, ops, Channel, InstallOptions, Stack};
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
                  (default ~/.atelier, override with ATELIER_HOME)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose the appliance — and repair it with --fix.
    ///
    /// Checks the host (Docker, ports, disk), the stack (compose files,
    /// containers) and the site inside it (Drupal boots, updates applied, files
    /// writable). Read-only unless you pass --fix.
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
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
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
        /// Host directory to write the static site into
        /// (default: ./aincient-export).
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
    },
    /// Restore from a backup file (destructive). A .tar.gz snapshot restores the
    /// database and files; a legacy .sql/.sql.gz dump restores the database only.
    #[command(visible_alias = "import")]
    Restore {
        /// Path to a `.tar.gz` snapshot (or a legacy `.sql`/`.sql.gz` dump).
        file: PathBuf,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
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
        std::process::exit(1);
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
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let stack = Stack::locate()?;

    match cli.command {
        Command::Doctor { fix, json } => doctor(&stack, fix, json),
        Command::App { command } => run_app(command, &stack),
        Command::Site { command } => run_site(command, &stack),
        Command::Data { command } => run_data(command, &stack),
        Command::Ai { command } => run_ai(command, &stack),
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
            let opts = InstallOptions {
                image: match channel {
                    Some(name) => parse_channel(&name)?.image(),
                    None => image,
                },
                http_port: port,
            };
            if ops::install(stack, &opts, &mut CliReporter::default())? {
                done_banner("Installed.", &stack.console_url());
            } else {
                pending_banner("Installed — still finishing first boot.", &stack.console_url());
            }
            show_login(stack);
            Ok(())
        }
        AppCommand::Update { to, yes } => update_cmd(stack, to, yes),
        AppCommand::Channel { channel, now, yes } => channel_cmd(stack, channel, now, yes),
        AppCommand::CheckUpdate { json } => check_update(stack, json),
        AppCommand::Reinstall { yes } => {
            if !confirm(
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
                pending_banner("Reinstalled — still finishing first boot.", &stack.console_url());
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
                && !confirm(
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
    }
}

/// `atelier data …` — portable db + files snapshots.
fn run_data(command: DataCommand, stack: &Stack) -> Result<()> {
    match command {
        DataCommand::Backup { label } => {
            let path = ops::backup(stack, label.as_deref(), &mut ops::Silent)?;
            println!("{} {}", style::success("Backup written to"), path.display());
            Ok(())
        }
        DataCommand::Restore { file, yes } => {
            // Provenance BEFORE the confirmation: which image this data came from
            // is exactly what the operator needs to decide, and after the prompt
            // it would be advice about something already done.
            if let Some(skew) = ops::restore_skew(stack, &file) {
                println!("{}", style::warn(&skew));
            }
            if !confirm(
                &format!(
                    "Restore will REPLACE the current database (and files, for a .tar.gz \
                     snapshot) with {}. Continue?",
                    file.display()
                ),
                yes,
            )? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            ops::restore(stack, &file, &mut ops::Silent)?;
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
        println!("  {} {}{star}", style::heading(&format!("{:<10}", r.role)), binding);
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

    if json {
        println!("{}", serde_json::to_string_pretty(&JsonReport::of(&report))?);
    } else {
        print_report(&report, fix);
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
fn print_report(report: &doctor::Report, fixed: bool) {
    for (tier, title) in [
        (doctor::Tier::Host, "Your machine"),
        (doctor::Tier::Stack, "The appliance"),
        (doctor::Tier::Site, "Your site"),
    ] {
        let checks: Vec<_> = report.checks.iter().filter(|c| c.tier == tier).collect();
        if checks.is_empty() {
            continue;
        }
        println!("\n{}", style::heading(title));
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
        println!("Run `atelier doctor --fix` to repair what can be repaired automatically.");
    }
}

fn print_check(check: &doctor::Check) {
    // Marks must stay distinguishable with colour off, so each severity gets its
    // own glyph: a checked box passes, an empty one fails (the existing
    // `style::mark` convention), `!` advises, `-` was never reached.
    let (mark, styled_label) = match check.severity {
        doctor::Severity::Ok => (style::mark(true), check.label.to_string()),
        doctor::Severity::Warn => ("[!]".to_string(), style::warn(check.label)),
        doctor::Severity::Fail => (style::mark(false), style::danger(check.label)),
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
    let plan = ops::plan_upgrade(stack, to.as_deref());

    if plan.is_stepped() {
        println!("{}", style::heading("This upgrade takes more than one step"));
        print_route(&plan);
        println!(
            "\nEach step pulls its image, migrates, and is health-checked before the \
             next one starts;\na step that fails rolls its own database back. A full \
             snapshot is taken first."
        );
        if let Some(problem) = &plan.problem {
            println!("\n{}", style::warn(problem));
        }
        if !confirm("\nWalk the whole route now?", yes)? {
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
        pending_banner("Update applied — still finishing boot.", &stack.console_url());
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
        println!(
            "{} {}",
            style::success("Already on"),
            target.describe()
        );
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
        if !confirm("Follow unreleased edge builds anyway?", yes)? {
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
        if !confirm("Switch to released versions anyway?", yes)? {
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
    line("Installed", st.installed);
    line("Running", st.running);
    line("Console reachable", st.reachable);
    println!("  Console:  {}", style::url(&st.console_url));
    println!("  Image:    {}", st.image);
    println!("  Channel:  {}", st.channel.describe());
    if !st.installed {
        println!(
            "\n{}",
            style::warn("Not installed yet — run `atelier install`.")
        );
    }
    Ok(())
}

fn check_update(stack: &Stack, json: bool) -> Result<()> {
    let check = ops::check_update(stack);
    if json {
        println!("{}", serde_json::to_string_pretty(&check)?);
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
        Some(false) if stack.channel() == Channel::Pinned => println!(
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
