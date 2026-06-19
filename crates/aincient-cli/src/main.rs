//! `aincient` — the command-line front door to an AIncient CMS appliance.
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
    name = "aincient",
    version,
    about = "Install and manage your AIncient CMS appliance.",
    long_about = "Install and manage your AIncient CMS appliance.\n\nAIncient runs as a \
                  Docker container; this command lays down and drives that stack \
                  (default ~/.aincient, override with AINCIENT_HOME)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check that Docker and Compose are ready to run the appliance.
    Doctor,
    /// Show the appliance status (installed, running, reachable).
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install the appliance (or upgrade in place if already installed).
    Install {
        /// AI provider API key (otherwise add it later in onboarding).
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
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
    /// Back up the database to ~/.aincient/backups.
    Backup {
        /// A label folded into the filename.
        #[arg(long)]
        label: Option<String>,
    },
    /// List backups taken on this host.
    Backups,
    /// Restore the database from a backup file (destructive).
    Restore {
        /// Path to a `.sql.gz` (or `.sql`) backup.
        file: PathBuf,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Wipe all data and install from scratch (destructive).
    Reinstall {
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
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
    /// Inspect or change the AI model bound to each AIncient role.
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

fn run() -> Result<()> {
    let cli = Cli::parse();
    let stack = Stack::locate()?;

    match cli.command {
        Command::Doctor => doctor(),
        Command::Status { json } => status(&stack, json),
        Command::Install { key, image, port } => {
            let opts = InstallOptions {
                ai_key: key,
                image,
                http_port: port,
            };
            ops::install(&stack, &opts)?;
            done_banner("Installed.", &stack.console_url());
            show_login(&stack);
            Ok(())
        }
        Command::Update => {
            ops::update(&stack)?;
            done_banner("Update complete.", &stack.console_url());
            Ok(())
        }
        Command::CheckUpdate { json } => check_update(&stack, json),
        Command::Backup { label } => {
            let path = ops::backup(&stack, label.as_deref())?;
            println!("{} {}", style::success("Backup written to"), path.display());
            Ok(())
        }
        Command::Backups => list_backups(&stack),
        Command::Restore { file, yes } => {
            if !confirm(
                &format!(
                    "Restore will REPLACE the current database with {}. Continue?",
                    file.display()
                ),
                yes,
            )? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            ops::restore(&stack, &file)?;
            println!("{}", style::success("Restore complete."));
            Ok(())
        }
        Command::Reinstall { key, yes } => {
            if !confirm(
                "Reinstall will DELETE all data (database, files, admin password) and install \
                 fresh. Continue?",
                yes,
            )? {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            let opts = InstallOptions {
                ai_key: key,
                ..Default::default()
            };
            ops::reinstall(&stack, &opts)?;
            done_banner("Reinstalled.", &stack.console_url());
            show_login(&stack);
            Ok(())
        }
        Command::Start => {
            ops::start(&stack)?;
            println!(
                "{} Console: {}",
                style::success("Started."),
                style::url(&stack.console_url())
            );
            Ok(())
        }
        Command::Stop => {
            ops::stop(&stack)?;
            println!("{}", style::success("Stopped."));
            Ok(())
        }
        Command::Down { wipe, yes } => {
            if wipe
                && !confirm(
                    "This will DELETE all data (database, files, admin password). Continue?",
                    yes,
                )?
            {
                println!("{}", style::warn("Aborted."));
                return Ok(());
            }
            ops::down(&stack, wipe)?;
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
        Command::Logs { follow, service } => {
            let mut cmd = ops::logs_command(&stack, follow, service.as_deref());
            cmd.status().context("failed to run docker compose logs")?;
            Ok(())
        }
        Command::Open => {
            ops::open_console(&stack)?;
            Ok(())
        }
        Command::Password { set } => match set {
            Some(pw) => {
                ops::set_admin_password(&stack, &pw)?;
                println!("{}", style::success("Admin password updated."));
                Ok(())
            }
            None => {
                match ops::admin_password(&stack) {
                    Some(pw) => println!("admin / {pw}"),
                    None => println!(
                        "No saved initial password (it was likely changed, or you pinned one at \
                         install). Set a new one with: aincient password --set <new>"
                    ),
                }
                Ok(())
            }
        },
        Command::Model { command } => match command {
            ModelCommand::List { json } => model_list(&stack, json),
            ModelCommand::Set {
                role,
                provider,
                model,
            } => {
                ops::model_set(&stack, &role, &provider, &model)?;
                println!(
                    "{} {role} → {provider}:{model}",
                    style::success("Bound")
                );
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
            println!("{}", style::success("Ready to run AIncient."));
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
            style::warn("Not installed yet — run `aincient install`.")
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
            "{} for {}.\nRun `aincient update`.",
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
            style::warn("No backups yet. Create one with `aincient backup`.")
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
