//! `atelier` — the command-line front door to an Atelier CMS appliance.
//!
//! A thin presentation layer over [`aincient_core`]: it parses arguments, confirms
//! destructive actions, and formats output. All behaviour lives in the core crate,
//! shared verbatim with the Tauri manager GUI.

use std::io::Write;
use std::path::PathBuf;

use aincient_core::{ops, InstallOptions, Stack};
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
    /// Check that Docker and Compose are ready to run the appliance.
    Doctor,
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
        /// Image tag to run.
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        /// Host port for the console.
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
    },
    /// Pull a newer image and converge in place (snapshot + auto-rollback).
    Update,
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
    /// Open the console in your browser.
    Open,
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

/// A success banner: a spectrum rule (when colour is on) over a mint headline and the
/// violet console URL. Used at the end of install/reinstall/update.
fn done_banner(headline: &str, url: &str) {
    println!();
    if let Some(rule) = style::spectrum_rule() {
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
    if let Some(rule) = style::spectrum_rule() {
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
        Command::Doctor => doctor(),
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
        AppCommand::Install { image, port } => {
            let opts = InstallOptions {
                image,
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
        AppCommand::Update => {
            if ops::update(stack, &mut CliReporter::default())? {
                done_banner("Update complete.", &stack.console_url());
            } else {
                pending_banner("Update applied — still finishing boot.", &stack.console_url());
            }
            Ok(())
        }
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
            ops::open_console(stack)?;
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

fn doctor() -> Result<()> {
    let pf = aincient_core::preflight();
    line("Docker installed", pf.docker_installed);
    line("Docker running", pf.docker_running);
    line("Compose plugin", pf.compose_available);
    match pf.problem() {
        Some(msg) => {
            println!("\n{}", style::warn(&msg));
            std::process::exit(1);
        }
        None => {
            println!();
            if let Some(rule) = style::spectrum_rule() {
                println!("{rule}");
            }
            println!("{}", style::success("Ready to run Atelier."));
            Ok(())
        }
    }
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
    match check.update_available {
        Some(true) => println!(
            "{} for {}.\nRun `atelier app update`.",
            style::heading("An update is available"),
            check.image
        ),
        Some(false) => println!(
            "{} {}.",
            style::success("You're on the latest"),
            check.image
        ),
        None => println!(
            "{}",
            style::warn(&format!(
                "Couldn't reach the registry to compare {} (are you logged in?).",
                check.image
            ))
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
