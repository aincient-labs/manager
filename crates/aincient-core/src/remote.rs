//! Remote sites over SSH (M2).
//!
//! For a site on another machine the **server's own `atelier` CLI is the
//! engine** (D-01): the Manager runs `ssh <target> atelier --site <slug> …` and
//! reads what it prints — no Docker over SSH, no second implementation of any
//! site action. This module owns that one hop:
//!
//! - [`Transport`] runs a POSIX `sh` script on the server. [`Ssh`] is the real
//!   one (`BatchMode`, `ConnectTimeout`, a multiplexed master in a private
//!   socket dir, deadlines via [`output_within_input`]); tests use a fake.
//! - [`Session::connect`] makes sure the server has a usable `atelier` before
//!   the first action: `command -v atelier`, then `~/.local/bin/atelier`.
//!   A server set up by `install.sh` has none, so the Manager copies a matching
//!   Linux build to `~/.local/bin/atelier` (verified, then moved into place
//!   atomically). One that is older than this Manager (major.minor) is refused
//!   with [`EngineTooOld`] unless the caller agreed to replace it.
//! - [`run_cli`] / [`status`] are the entry points the front-ends use.
//!
//! Attaching only (U-03): nothing here installs an appliance on a server.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::docker::{output_within, output_within_input, stream_lines_within, Pipe, RunError};
use crate::doctor::{Check, HostTier, Report, Severity, Tier};
use crate::ops::{ExportOptions, FreezeOptions, SnapshotList, Status, UpdateCheck};
use crate::sites::{KeyMode, Remote, Site};
use crate::ssh_key::{self, KeyFile, SecretStore};

/// `ssh -o ConnectTimeout` — how long the TCP connect + handshake may take.
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Deadline for the engine probe and the small housekeeping scripts.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(30);
/// Deadline for a read-only CLI call such as `app status`.
pub const CLI_DEADLINE: Duration = Duration::from_secs(120);
/// Deadline for streaming the engine binary to the server.
pub const UPLOAD_DEADLINE: Duration = Duration::from_secs(600);
/// `app check-update`: a registry probe per hop of the route (each bounded
/// by the server's own 10 s probe deadline), so more than a plain read.
pub const CHECK_UPDATE_DEADLINE: Duration = Duration::from_secs(300);
/// `app start` / `app stop`: the server waits up to `START_READY_TIMEOUT`
/// (120 s) for the console, after Compose has done its part.
pub const LIFECYCLE_DEADLINE: Duration = Duration::from_secs(600);
/// `doctor` read-only: Docker probes (15 s each) plus drush calls.
pub const DOCTOR_DEADLINE: Duration = Duration::from_secs(300);
/// `doctor --fix`, `site freeze`, `site export`: a converge run or a full
/// static render. Locally unbounded; over SSH half an hour is the ceiling.
pub const WORK_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// `data backup` / `data restore`: a database dump plus the files tree.
/// Locally unbounded (`run_inherited`); a link dead that long has failed.
pub const DATA_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// `app update`: every hop pulls an image, migrates and waits up to
/// `READY_TIMEOUT` (240 s) — a stepped route can take several of those.
pub const UPDATE_DEADLINE: Duration = Duration::from_secs(90 * 60);
/// How long the multiplexed master outlives its last client.
pub const CONTROL_PERSIST_SECS: u64 = 60;
/// Env override for the Linux engine binary to copy (same as `--engine-binary`).
pub const BINARY_ENV: &str = "ATELIER_REMOTE_BINARY";
/// Where release assets live (`<base>/v<version>/atelier-<triple>.tar.xz`).
pub const RELEASES_BASE: &str = "https://github.com/aincient-labs/manager/releases/download";
/// This Manager's version — what a remote engine has to match (major.minor).
pub const MANAGER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs a POSIX `sh` script on one server.
pub trait Transport {
    /// The ssh target, for messages.
    fn target(&self) -> &str;
    /// Run `script` on the server under `sh`, with `input` (a local file) as
    /// its stdin, bounded by `deadline`. The raw outcome; [`Session`] maps it.
    fn exec(
        &self,
        script: &str,
        input: Option<&Path>,
        deadline: Duration,
    ) -> std::result::Result<Output, RunError>;

    /// Run `script` with stdin closed, handing each output line to `sink` as
    /// it arrives (for long actions the user watches), bounded by `deadline`.
    fn exec_streaming(
        &self,
        script: &str,
        deadline: Duration,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> std::result::Result<ExitStatus, RunError>;
}

/// The real transport: the system `ssh`.
pub struct Ssh {
    target: String,
    control_path: Option<PathBuf>,
    identity: Option<KeyFile>,
    master_tried: std::cell::Cell<bool>,
}

impl Ssh {
    /// `ssh` to `remote.host`, authenticating as its [`KeyMode`] says: the
    /// user's own setup, or the guided key from `store`, materialised to a
    /// private temp file for the lifetime of this value.
    pub fn new(remote: &Remote, store: &dyn SecretStore) -> Result<Self> {
        let identity = match remote.key {
            KeyMode::Ssh => None,
            KeyMode::Guided => Some(ssh_key::key_file(store, &remote.host)?),
        };
        Ok(Self::with_identity(&remote.host, identity))
    }

    /// `ssh` to `target` with an explicit identity file (or none).
    pub fn with_identity(target: &str, identity: Option<KeyFile>) -> Self {
        Ssh {
            target: target.to_string(),
            control_path: control_path(),
            identity,
            master_tried: std::cell::Cell::new(false),
        }
    }

    /// The options every invocation shares.
    fn base_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut opt = |o: String| {
            args.push("-o".into());
            args.push(o.into());
        };
        opt("BatchMode=yes".into());
        opt(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"));
        opt("ServerAliveInterval=15".into());
        opt("ServerAliveCountMax=2".into());
        if let Some(path) = &self.control_path {
            opt(format!("ControlPath={}", path.display()));
        }
        if let Some(key) = &self.identity {
            opt("IdentitiesOnly=yes".into());
            args.push("-i".into());
            args.push(key.path().into());
        }
        args
    }

    /// The `ssh` command that runs `script` on the server. It joins a running
    /// master when there is one (`ControlMaster=no` falls back to a direct
    /// connection when there isn't).
    pub fn command(&self, script: &str) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(self.base_args());
        if self.control_path.is_some() {
            cmd.args(["-o", "ControlMaster=no"]);
        }
        // `--` so the target can never be read as an option; the script goes
        // through `sh -c` so the user's login shell (fish, zsh…) doesn't matter.
        cmd.arg("-T")
            .arg("--")
            .arg(&self.target)
            .arg(format!("sh -c {}", sh_quote(script)));
        cmd
    }

    /// Start a background master once, best effort, so later calls skip the
    /// handshake. Its stdio is /dev/null: a master holding our pipes would
    /// stall [`output_within_input`] until its deadline.
    fn start_master(&self) {
        if self.master_tried.replace(true) {
            return;
        }
        let Some(path) = &self.control_path else {
            return;
        };
        if path.exists() {
            return;
        }
        let mut cmd = Command::new("ssh");
        cmd.args(self.base_args())
            .args(["-o", "ControlMaster=yes"])
            .args(["-o", &format!("ControlPersist={CONTROL_PERSIST_SECS}")])
            .args(["-N", "-f", "--", &self.target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return;
        };
        let until = Instant::now() + Duration::from_secs(CONNECT_TIMEOUT_SECS + 5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

impl Transport for Ssh {
    fn target(&self) -> &str {
        &self.target
    }

    fn exec(
        &self,
        script: &str,
        input: Option<&Path>,
        deadline: Duration,
    ) -> std::result::Result<Output, RunError> {
        self.start_master();
        let mut cmd = self.command(script);
        match input {
            None => output_within(&mut cmd, deadline),
            Some(path) => {
                let file = File::open(path).map_err(|source| RunError::Io {
                    program: path.display().to_string(),
                    source,
                })?;
                output_within_input(&mut cmd, deadline, Stdio::from(file))
            }
        }
    }

    fn exec_streaming(
        &self,
        script: &str,
        deadline: Duration,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> std::result::Result<ExitStatus, RunError> {
        self.start_master();
        stream_lines_within(&mut self.command(script), deadline, sink)
    }
}

/// `~/.atelier/ssh/%C`: the multiplexing socket, in a 0700 directory. `None`
/// (no multiplexing) when there is no home, the directory can't be made
/// private, or the path would exceed the ~104-byte Unix socket limit.
fn control_path() -> Option<PathBuf> {
    if cfg!(windows) {
        return None; // Windows OpenSSH has no ControlMaster.
    }
    let dir = dirs::home_dir()?
        .join(crate::sites::DEFAULT_DIR)
        .join("ssh");
    // %C expands to a 40-hex-char hash of the connection.
    if dir.as_os_str().len() + 1 + 40 >= 100 {
        return None;
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(&dir).ok()?;
        // An existing directory keeps its mode; make sure it is private.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    #[cfg(not(unix))]
    builder.create(&dir).ok()?;
    Some(dir.join("%C"))
}

/// Quote `s` as one POSIX shell word.
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./=:@%+-,".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// A `major.minor.patch` version, prerelease/build suffix ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// The first `[v]X.Y.Z` token in `text` (e.g. `atelier 0.12.0`).
    pub fn find(text: &str) -> Option<Version> {
        text.split_whitespace().find_map(|word| {
            let core = word.trim_start_matches('v');
            let core = core.split(['-', '+']).next()?;
            let mut parts = core.split('.');
            let v = Version {
                major: parts.next()?.parse().ok()?,
                minor: parts.next()?.parse().ok()?,
                patch: parts.next()?.parse().ok()?,
            };
            parts.next().is_none().then_some(v)
        })
    }

    /// Whether this engine is new enough to drive for a Manager at `wanted`:
    /// same or newer major.minor.
    pub fn satisfies(self, wanted: Version) -> bool {
        (self.major, self.minor) >= (wanted.major, wanted.minor)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// This Manager's own version.
pub fn manager_version() -> Version {
    Version::find(MANAGER_VERSION).expect("CARGO_PKG_VERSION is X.Y.Z")
}

/// The server's `atelier` is older than this Manager (major.minor). Returned
/// from [`Session::connect`] so a front-end can offer the copy and retry with
/// [`EngineOptions::replace_older`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineTooOld {
    pub host: String,
    pub path: String,
    /// What it reported (`atelier 0.11.2`), or that it reported nothing usable.
    pub found: String,
    pub wanted: Version,
}

impl std::fmt::Display for EngineTooOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the atelier CLI on {} ({}) is {}, older than this Manager ({}). \
             The Manager can copy atelier {} to ~/.local/bin/atelier on {} \
             (your existing one is left where it is).",
            self.host, self.path, self.found, self.wanted, self.wanted, self.host
        )
    }
}

impl std::error::Error for EngineTooOld {}

/// How to get an engine onto a server that needs one.
#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    /// A local Linux `atelier` binary to copy (`--engine-binary`,
    /// `ATELIER_REMOTE_BINARY`). `None` = this Manager's GitHub release asset.
    pub binary: Option<PathBuf>,
    /// Copy ours to `~/.local/bin` even when an older `atelier` exists.
    pub replace_older: bool,
    /// Copy nothing at all: a server without a usable `atelier` is an error
    /// ([`NoEngine`] / [`EngineTooOld`]). For `doctor`, which only looks.
    pub no_install: bool,
}

impl EngineOptions {
    /// Defaults plus `ATELIER_REMOTE_BINARY`.
    pub fn from_env() -> Self {
        EngineOptions {
            binary: std::env::var_os(BINARY_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            replace_older: false,
            no_install: false,
        }
    }
}

/// The server has no `atelier` CLI and the caller said not to copy one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoEngine {
    pub host: String,
}

impl std::fmt::Display for NoEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} has no atelier CLI on its PATH or in ~/.local/bin",
            self.host
        )
    }
}

impl std::error::Error for NoEngine {}

/// A remote `atelier` command ran and exited non-zero. Its own output (and
/// error message) already reached the user; front-ends exit with `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExit {
    pub host: String,
    /// `` `atelier data backup` ``.
    pub what: String,
    pub code: i32,
}

impl std::fmt::Display for RemoteExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed on {} (exit {})",
            self.what, self.host, self.code
        )
    }
}

impl std::error::Error for RemoteExit {}

/// One site action, as it runs on a remote site (m2-7, D-04): the same
/// `atelier` subcommand the server would run locally. The front-end confirms
/// anything destructive BEFORE building one, so the server is always run
/// non-interactively (`--yes`, stdin closed) and never waits on a prompt.
#[derive(Debug, Clone)]
pub enum Action {
    /// `app status --json` — see [`Session::status`].
    Status,
    /// `app check-update --json` — see [`Session::check_update`].
    CheckUpdate,
    /// `app update [--to V] [--yes]`. `yes` only when the route was confirmed
    /// here first (or there was nothing to confirm).
    Update { to: Option<String>, yes: bool },
    /// `app start`.
    Start,
    /// `app stop`.
    Stop,
    /// `data backup [--label L]` — the backup stays on the server (m2-5 moves it).
    Backup { label: Option<String> },
    /// `data list` — the server's backups.
    BackupList,
    /// `data restore <FILE> --yes`, FILE being a path on the server.
    Restore { file: String },
    /// `site export …` — written on the server.
    Export(ExportOptions),
    /// `site freeze …`.
    Freeze(FreezeOptions),
    /// `site snapshots --json` — see [`Session::snapshots`].
    Snapshots,
    /// `site use <ID>`.
    UseSnapshot { id: String },
    /// `site live`.
    Live,
    /// `site prune --keep N`.
    Prune { keep: u32 },
    /// `doctor --json [--fix]` — see [`Session::doctor`].
    Doctor { fix: bool },
}

impl Action {
    /// The `atelier` arguments after `--site <slug>`.
    pub fn argv(&self) -> Vec<String> {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut v = match self {
            Action::Status => words(&["app", "status", "--json"]),
            Action::CheckUpdate => words(&["app", "check-update", "--json"]),
            Action::Update { .. } => words(&["app", "update"]),
            Action::Start => words(&["app", "start"]),
            Action::Stop => words(&["app", "stop"]),
            Action::Backup { .. } => words(&["data", "backup"]),
            Action::BackupList => words(&["data", "list"]),
            Action::Restore { .. } => words(&["data", "restore"]),
            Action::Export(_) => words(&["site", "export"]),
            Action::Freeze(_) => words(&["site", "freeze"]),
            Action::Snapshots => words(&["site", "snapshots", "--json"]),
            Action::UseSnapshot { id } => words(&["site", "use", id]),
            Action::Live => words(&["site", "live"]),
            Action::Prune { keep } => vec![
                "site".into(),
                "prune".into(),
                "--keep".into(),
                keep.to_string(),
            ],
            Action::Doctor { .. } => words(&["doctor", "--json"]),
        };
        let mut opt = |flag: &str, value: &Option<String>| {
            if let Some(value) = value {
                v.push(flag.to_string());
                v.push(value.clone());
            }
        };
        match self {
            Action::Update { to, yes } => {
                opt("--to", to);
                if *yes {
                    v.push("--yes".into());
                }
            }
            Action::Backup { label } => opt("--label", label),
            Action::Restore { file } => {
                // `--` so a file named like a flag stays a file.
                v.extend(["--yes".into(), "--".into(), file.clone()]);
            }
            Action::Export(o) => {
                opt(
                    "--out",
                    &o.out.as_ref().map(|p| p.to_string_lossy().into_owned()),
                );
                opt("--base-url", &o.base_url);
                for (on, flag) in [
                    (o.zip, "--zip"),
                    (o.include_config, "--include-config"),
                    (o.include_users, "--include-users"),
                    (o.skip_link_check, "--skip-link-check"),
                ] {
                    if on {
                        v.push(flag.into());
                    }
                }
            }
            Action::Freeze(o) => {
                opt("--label", &o.label);
                opt("--base-url", &o.base_url);
                for (on, flag) in [
                    (o.keep, "--keep"),
                    (o.no_serve, "--no-serve"),
                    (o.force, "--force"),
                ] {
                    if on {
                        v.push(flag.into());
                    }
                }
            }
            Action::Doctor { fix: true } => v.push("--fix".into()),
            _ => {}
        }
        v
    }

    /// How long the server may take. Local equivalents of the long ones run
    /// unbounded; over SSH each gets a ceiling that fits its slowest honest run.
    pub fn deadline(&self) -> Duration {
        match self {
            Action::Status
            | Action::BackupList
            | Action::Snapshots
            | Action::UseSnapshot { .. }
            | Action::Live
            | Action::Prune { .. } => CLI_DEADLINE,
            Action::CheckUpdate => CHECK_UPDATE_DEADLINE,
            Action::Start | Action::Stop => LIFECYCLE_DEADLINE,
            Action::Doctor { fix: false } => DOCTOR_DEADLINE,
            Action::Doctor { fix: true } | Action::Export(_) | Action::Freeze(_) => WORK_DEADLINE,
            Action::Backup { .. } | Action::Restore { .. } => DATA_DEADLINE,
            Action::Update { .. } => UPDATE_DEADLINE,
        }
    }
}

/// One `atelier` found on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCli {
    pub path: String,
    /// Its `--version` line (empty when it printed nothing).
    pub reported: String,
}

/// What the probe learned about the server, in one round trip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Probe {
    /// `uname -m`.
    pub arch: String,
    /// `$HOME`.
    pub home: String,
    /// `command -v atelier`, then `~/.local/bin/atelier` (when different).
    pub clis: Vec<RemoteCli>,
}

const PROBE_SCRIPT: &str = r#"printf 'arch\t%s\n' "$(uname -m)"
printf 'home\t%s\n' "$HOME"
p=$(command -v atelier 2>/dev/null) && [ -x "$p" ] && printf 'cli\t%s\t%s\n' "$p" "$("$p" --version 2>/dev/null | head -n 1)"
l="$HOME/.local/bin/atelier"
[ -x "$l" ] && [ "$l" != "$p" ] && printf 'cli\t%s\t%s\n' "$l" "$("$l" --version 2>/dev/null | head -n 1)"
exit 0"#;

const UPLOAD_SCRIPT: &str =
    r#"umask 077; mkdir -p "$HOME/.local/bin" && cat > "$HOME/.local/bin/atelier.tmp""#;
const CHECK_UPLOAD_SCRIPT: &str =
    r#"chmod 755 "$HOME/.local/bin/atelier.tmp" && "$HOME/.local/bin/atelier.tmp" --version"#;
const COMMIT_UPLOAD_SCRIPT: &str =
    r#"mv -f "$HOME/.local/bin/atelier.tmp" "$HOME/.local/bin/atelier""#;
const ABORT_UPLOAD_SCRIPT: &str = r#"rm -f "$HOME/.local/bin/atelier.tmp""#;

impl Probe {
    fn parse(stdout: &str) -> Probe {
        let mut probe = Probe::default();
        for line in stdout.lines() {
            let mut cols = line.splitn(3, '\t');
            match (cols.next(), cols.next()) {
                (Some("arch"), Some(v)) => probe.arch = v.trim().to_string(),
                (Some("home"), Some(v)) => probe.home = v.trim().to_string(),
                (Some("cli"), Some(path)) => probe.clis.push(RemoteCli {
                    path: path.to_string(),
                    reported: cols.next().unwrap_or("").trim().to_string(),
                }),
                _ => {}
            }
        }
        probe
    }
}

/// The Rust target triple of the release asset for a `uname -m`.
pub fn linux_triple(arch: &str) -> Result<&'static str> {
    match arch {
        "x86_64" | "amd64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" | "arm64" => Ok("aarch64-unknown-linux-gnu"),
        "" => bail!("the server didn't report its architecture"),
        other => {
            bail!("the server is {other}; Atelier ships Linux builds for x86_64 and aarch64 only")
        }
    }
}

/// The release asset URL for `triple` at this Manager's version.
pub fn release_asset_url(triple: &str) -> String {
    format!("{RELEASES_BASE}/v{MANAGER_VERSION}/atelier-{triple}.tar.xz")
}

/// A connection to one remote site's engine, ready for [`Session::run`].
pub struct Session<T: Transport> {
    transport: T,
    remote_slug: String,
    engine: String,
    /// What the engine's `--version` said (`atelier 0.12.0`).
    engine_reported: String,
}

impl<T: Transport> Session<T> {
    /// Probe the server and settle its engine: use an `atelier` at least as new
    /// as this Manager (major.minor), else copy ours when there is none (or
    /// when `opts.replace_older`), else fail with [`EngineTooOld`].
    pub fn connect(transport: T, remote_slug: &str, opts: &EngineOptions) -> Result<Self> {
        let wanted = manager_version();
        let out = checked(
            &transport,
            "checking for the atelier CLI",
            transport.exec(PROBE_SCRIPT, None, PROBE_DEADLINE),
        )?;
        let probe = Probe::parse(&String::from_utf8_lossy(&out.stdout));
        let usable = probe
            .clis
            .iter()
            .find(|c| Version::find(&c.reported).is_some_and(|v| v.satisfies(wanted)));
        let (engine, engine_reported) = match (usable, probe.clis.first()) {
            (Some(cli), _) => (cli.path.clone(), cli.reported.clone()),
            (None, Some(old)) if !opts.replace_older || opts.no_install => {
                return Err(anyhow!(EngineTooOld {
                    host: transport.target().to_string(),
                    path: old.path.clone(),
                    found: if old.reported.is_empty() {
                        "of unknown version".to_string()
                    } else {
                        format!("`{}`", old.reported)
                    },
                    wanted,
                }));
            }
            (None, None) if opts.no_install => {
                return Err(anyhow!(NoEngine {
                    host: transport.target().to_string(),
                }));
            }
            _ => (
                install_engine(&transport, &probe, opts, wanted)?,
                format!("atelier {wanted}"),
            ),
        };
        Ok(Session {
            transport,
            remote_slug: remote_slug.to_string(),
            engine,
            engine_reported,
        })
    }

    /// The ssh target this session reaches.
    pub fn host(&self) -> &str {
        self.transport.target()
    }

    /// What the engine reported as its version (`atelier 0.12.0`).
    pub fn engine_version(&self) -> &str {
        &self.engine_reported
    }

    /// The shell line that runs `atelier --site <slug> <args…>` on the server.
    fn script(&self, args: &[&str]) -> String {
        let mut words = vec![
            sh_quote(&self.engine),
            "--site".into(),
            sh_quote(&self.remote_slug),
        ];
        words.extend(args.iter().map(|a| sh_quote(a)));
        words.join(" ")
    }

    /// Run `atelier --site <slug> <args…>` on the server, its output streamed
    /// line by line to `sink` as it arrives. A non-zero exit is a
    /// [`RemoteExit`] carrying the server's exit code; the CLI's own message
    /// has already gone through `sink`.
    pub fn run_streamed(
        &self,
        args: &[&str],
        deadline: Duration,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> Result<()> {
        let host = self.transport.target();
        let what = format!("`atelier {}`", args.join(" "));
        let mut last_err = String::new();
        let outcome =
            self.transport
                .exec_streaming(&self.script(args), deadline, &mut |pipe, line| {
                    if pipe == Pipe::Stderr && !line.trim().is_empty() {
                        last_err = line.trim().to_string();
                    }
                    sink(pipe, line)
                });
        let status = match outcome {
            Ok(status) => status,
            Err(RunError::TimedOut { after, .. }) => bail!(
                "{what} on {host} didn't finish within {} min and the Manager stopped waiting                  — it may still be running on the server; check there before retrying",
                after.as_secs() / 60
            ),
            Err(RunError::Io { source, .. }) => {
                bail!("couldn't run ssh to reach {host}: {source} — is OpenSSH installed?")
            }
        };
        match status.code() {
            Some(0) => Ok(()),
            Some(255) => bail!("lost the connection to {host} during {what}: {last_err}"),
            Some(code) => Err(anyhow!(RemoteExit {
                host: host.to_string(),
                what,
                code,
            })),
            None => bail!("{what} on {host} was killed"),
        }
    }

    /// Run one [`Action`] with its own deadline, streaming its output.
    pub fn run_action(&self, action: &Action, sink: &mut dyn FnMut(Pipe, &str)) -> Result<()> {
        let argv = action.argv();
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.run_streamed(&args, action.deadline(), sink)
    }

    /// Run a `--json` action and parse what it printed into `J`.
    fn json<J: serde::de::DeserializeOwned>(&self, action: &Action) -> Result<J> {
        let argv = action.argv();
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = self.run(&args, action.deadline())?;
        serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "{} answered `{}` with something the Manager can't read",
                self.transport.target(),
                argv.join(" ")
            )
        })
    }

    /// The server's `app check-update --json`.
    pub fn check_update(&self) -> Result<UpdateCheck> {
        self.json(&Action::CheckUpdate)
    }

    /// The server's `site snapshots --json`.
    pub fn snapshots(&self) -> Result<SnapshotList> {
        self.json(&Action::Snapshots)
    }

    /// The server's own `doctor --json [--fix]`, every check marked as run on
    /// the server (m2-9). With `fix` the repair ladder's progress — everything
    /// the server prints before its JSON — goes to `sink` as it happens. The
    /// server exits 1 for an unhealthy report; that is still a report.
    pub fn doctor(&self, fix: bool, sink: &mut dyn FnMut(Pipe, &str)) -> Result<Report> {
        let action = Action::Doctor { fix };
        let mut json = String::new();
        let mut in_json = false;
        let result = self.run_action(&action, &mut |pipe, line| {
            // The pretty-printed report starts with a lone `{` on stdout.
            if pipe == Pipe::Stdout && (in_json || line == "{") {
                in_json = true;
                json.push_str(line);
                json.push('\n');
            } else {
                sink(pipe, line);
            }
        });
        match result {
            Ok(()) => {}
            Err(e) if e.downcast_ref::<RemoteExit>().is_some_and(|x| x.code == 1) && in_json => {}
            Err(e) => return Err(e),
        }
        let mut report: Report = serde_json::from_str(&json).with_context(|| {
            format!(
                "{} answered `atelier doctor --json` with something that isn't a report",
                self.transport.target()
            )
        })?;
        let on = HostTier::Remote(self.transport.target().to_string());
        for check in &mut report.checks {
            check.on = on.clone();
        }
        Ok(report)
    }

    /// The `atelier` path this session drives on the server.
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// Run `atelier --site <remote slug> <args…>` on the server. A non-zero
    /// exit is an error naming the host and carrying the CLI's own message.
    pub fn run(&self, args: &[&str], deadline: Duration) -> Result<Output> {
        let what = format!("`atelier {}`", args.join(" "));
        checked(
            &self.transport,
            &what,
            self.transport.exec(&self.script(args), None, deadline),
        )
    }

    /// The site's status, as the server's `atelier app status --json` says.
    pub fn status(&self) -> Result<Status> {
        let out = self.run(&["app", "status", "--json"], CLI_DEADLINE)?;
        serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "{} answered `app status --json` with something that isn't a status",
                self.transport.target()
            )
        })
    }

    /// Prove the site can be attached (U-03): the server answers and the site
    /// is installed and running there. Returns its status.
    pub fn attach_check(&self) -> Result<Status> {
        let st = self.status()?;
        let host = self.transport.target();
        if !st.installed {
            bail!(
                "site `{}` on {host} is not installed — the Manager attaches to a running \
                 appliance only; set it up on the server first",
                self.remote_slug
            );
        }
        if !st.running {
            bail!(
                "site `{}` on {host} is installed but not running — start it there \
                 (`atelier --site {} app start`) and try again",
                self.remote_slug,
                self.remote_slug
            );
        }
        Ok(st)
    }
}

/// Copy this Manager's Linux engine to `~/.local/bin/atelier` on the server:
/// stream it to a temp name (umask 077), make it executable, check it runs and
/// reports a usable version, then `mv` it into place. Returns its path.
fn install_engine<T: Transport>(
    transport: &T,
    probe: &Probe,
    opts: &EngineOptions,
    wanted: Version,
) -> Result<String> {
    let host = transport.target();
    let triple = linux_triple(&probe.arch).with_context(|| format!("on {host}"))?;
    if probe.home.is_empty() {
        bail!("{host} didn't report a home directory to install the atelier CLI into");
    }
    let local = local_binary(triple, opts)?;
    checked(
        transport,
        "copying the atelier CLI",
        transport.exec(UPLOAD_SCRIPT, Some(local.path()), UPLOAD_DEADLINE),
    )?;
    let verified = checked(
        transport,
        "checking the copied atelier CLI",
        transport.exec(CHECK_UPLOAD_SCRIPT, None, PROBE_DEADLINE),
    )
    .and_then(|out| {
        let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match Version::find(&reported) {
            Some(v) if v.satisfies(wanted) => Ok(()),
            _ => Err(anyhow!(
                "the copied atelier CLI reports `{reported}`, not {wanted} — \
                 is {} a Linux {triple} build of this version?",
                local.path().display()
            )),
        }
    });
    if let Err(e) = verified {
        let _ = transport.exec(ABORT_UPLOAD_SCRIPT, None, PROBE_DEADLINE);
        return Err(e);
    }
    checked(
        transport,
        "installing the atelier CLI",
        transport.exec(COMMIT_UPLOAD_SCRIPT, None, PROBE_DEADLINE),
    )?;
    Ok(format!(
        "{}/.local/bin/atelier",
        probe.home.trim_end_matches('/')
    ))
}

/// Map a transport outcome to a result whose error names the host.
fn checked<T: Transport>(
    transport: &T,
    what: &str,
    outcome: std::result::Result<Output, RunError>,
) -> Result<Output> {
    let host = transport.target();
    let out = match outcome {
        Ok(out) => out,
        Err(RunError::TimedOut { after, .. }) => bail!(
            "no answer from {host} within {}s while {what} — is the server up and reachable?",
            after.as_secs()
        ),
        Err(RunError::Io { source, .. }) => {
            bail!("couldn't run ssh to reach {host}: {source} — is OpenSSH installed?")
        }
    };
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no message")
        .to_string();
    match out.status.code() {
        // ssh itself failed: connect, host key or auth.
        Some(255) if stderr.contains("Permission denied") => bail!(
            "{host} refused the login ({detail}). Check that `ssh {host}` works from a \
             terminal, or that the Manager's key is in ~/.ssh/authorized_keys on the server"
        ),
        Some(255) => bail!("could not connect to {host}: {detail}"),
        Some(code) => bail!("{what} failed on {host} (exit {code}): {detail}"),
        None => bail!("{what} on {host} was killed: {detail}"),
    }
}

/// A private temporary directory (0700), removed with everything in it on drop.
pub(crate) struct PrivateDir(PathBuf);

impl PrivateDir {
    pub(crate) fn new(prefix: &str) -> Result<Self> {
        let mut rand = [0u8; 8];
        getrandom::getrandom(&mut rand).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
        let suffix: String = rand.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("{prefix}-{suffix}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        Ok(PrivateDir(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The local engine binary to copy, with the temp dir it may live in.
pub struct LocalBinary {
    path: PathBuf,
    _dir: Option<PrivateDir>,
}

impl LocalBinary {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `opts.binary`, else this Manager's release asset for `triple`, downloaded
/// and verified against its `.sha256`.
fn local_binary(triple: &str, opts: &EngineOptions) -> Result<LocalBinary> {
    if let Some(path) = &opts.binary {
        if !path.is_file() {
            bail!("engine binary {} does not exist", path.display());
        }
        return Ok(LocalBinary {
            path: path.clone(),
            _dir: None,
        });
    }
    download_release(triple)
}

fn download_release(triple: &str) -> Result<LocalBinary> {
    let dir = PrivateDir::new("atelier-engine")?;
    let url = release_asset_url(triple);
    let archive = dir.path().join(format!("atelier-{triple}.tar.xz"));
    let sums = dir.path().join("archive.sha256");
    let hint = || {
        format!(
            "couldn't fetch the Linux atelier CLI {MANAGER_VERSION} ({url}). Pass \
             --engine-binary <path> (or set {BINARY_ENV}) to copy a local {triple} build"
        )
    };
    for (from, to) in [(url.clone(), &archive), (format!("{url}.sha256"), &sums)] {
        let mut cmd = Command::new("curl");
        cmd.args(["-fsSL", "--max-time", "300", "-o"])
            .arg(to)
            .arg(&from);
        let out = output_within(&mut cmd, Duration::from_secs(310)).with_context(hint)?;
        if !out.status.success() {
            return Err(anyhow!(String::from_utf8_lossy(&out.stderr)
                .trim()
                .to_string()))
            .with_context(hint);
        }
    }
    let expected = std::fs::read_to_string(&sums).with_context(hint)?;
    verify_sha256(&archive, &expected)?;
    let mut tar = Command::new("tar");
    tar.arg("-xJf").arg(&archive).arg("-C").arg(dir.path());
    let out = output_within(&mut tar, Duration::from_secs(120))
        .map_err(|e| anyhow!("couldn't unpack {}: {e}", archive.display()))?;
    if !out.status.success() {
        bail!(
            "couldn't unpack {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let path = dir.path().join(format!("atelier-{triple}")).join("atelier");
    if !path.is_file() {
        bail!("{url} has no atelier binary inside");
    }
    Ok(LocalBinary {
        path,
        _dir: Some(dir),
    })
}

/// Check `file` against a `.sha256` sibling's text (`<hex> [*]<name>`).
pub fn verify_sha256(file: &Path, sums: &str) -> Result<()> {
    let expected = sums
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .context("the .sha256 file holds no SHA-256")?
        .to_ascii_lowercase();
    let bytes =
        std::fs::read(file).with_context(|| format!("could not read {}", file.display()))?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        bail!(
            "{} does not match its published SHA-256 (expected {expected}, got {actual}) — \
             not copying it",
            file.display()
        );
    }
    Ok(())
}

/// Open a session to a remote site's engine over SSH.
pub fn open(site: &Site, opts: &EngineOptions) -> Result<Session<Ssh>> {
    let remote = remote_of(site)?;
    let ssh = Ssh::new(remote, &ssh_key::Keychain)?;
    Session::connect(ssh, &remote.slug, opts)
}

/// Run `atelier --site <remote slug> <args…>` on `site`'s server — the one
/// entry point site actions route through for a remote site.
pub fn run_cli(site: &Site, args: &[&str]) -> Result<Output> {
    open(site, &EngineOptions::from_env())?.run(args, CLI_DEADLINE)
}

/// A remote site's status, from the server's own CLI.
pub fn status(site: &Site) -> Result<Status> {
    open(site, &EngineOptions::from_env())?.status()
}

/// `doctor` for a remote site (m2-9): the checks that genuinely run on this
/// machine — the Manager's key, the SSH login, the server's `atelier` CLI —
/// under [`Tier::Connection`], then the server's own report with every check
/// marked [`HostTier::Remote`]. This machine's Docker, ports and disk are never
/// looked at: they say nothing about the server. Copies no engine.
pub fn doctor(site: &Site, fix: bool, sink: &mut dyn FnMut(Pipe, &str)) -> Result<Report> {
    let remote = remote_of(site)?;
    let mut checks = Vec::new();
    let transport = match remote.key {
        KeyMode::Ssh => Ssh::new(remote, &ssh_key::Keychain),
        KeyMode::Guided => {
            let made = Ssh::new(remote, &ssh_key::Keychain);
            checks.push(match &made {
                Ok(_) => Check::ok("remote.key", Tier::Connection, "The Manager's key for this server")
                    .detail("in your OS keychain"),
                Err(e) => Check::bad(
                    "remote.key",
                    Tier::Connection,
                    "The Manager's key for this server",
                    Severity::Fail,
                    format!(
                        "Re-create it with `atelier sites remove {0}` then                          `atelier sites add {0} --host {1} --key guided`, or use an SSH alias",
                        site.slug, remote.host
                    ),
                )
                .detail(format!("{e:#}")),
            });
            made
        }
    };
    Ok(match transport {
        Ok(t) => doctor_over(t, &remote.slug, fix, checks, sink),
        Err(_) => {
            checks.push(server_skipped(
                &remote.host,
                "the Manager has no key to log in with",
            ));
            Report {
                checks,
                actions: Vec::new(),
            }
        }
    })
}

/// [`doctor`] over any transport: `checks` so far, plus the login and engine
/// checks, plus the server's own report.
pub fn doctor_over<T: Transport>(
    transport: T,
    remote_slug: &str,
    fix: bool,
    mut checks: Vec<Check>,
    sink: &mut dyn FnMut(Pipe, &str),
) -> Report {
    const SSH: (&str, &str) = ("remote.ssh", "SSH login to the server");
    const CLI: (&str, &str) = ("remote.cli", "atelier CLI on the server");
    let host = transport.target().to_string();
    let opts = EngineOptions {
        no_install: true,
        ..EngineOptions::from_env()
    };
    let session = match Session::connect(transport, remote_slug, &opts) {
        Ok(session) => session,
        Err(e) => {
            let detail = format!("{e:#}");
            let engine_problem = e.downcast_ref::<EngineTooOld>().is_some()
                || e.downcast_ref::<NoEngine>().is_some();
            if engine_problem {
                checks.push(Check::ok(SSH.0, Tier::Connection, SSH.1).detail(host.clone()));
                checks.push(
                    Check::bad(
                        CLI.0,
                        Tier::Connection,
                        CLI.1,
                        Severity::Fail,
                        format!(
                            "Run `atelier --site <SLUG> app status` in a terminal: the Manager \
                             offers to copy atelier {} to ~/.local/bin on {host}",
                            manager_version()
                        ),
                    )
                    .detail(detail),
                );
                checks.push(server_skipped(
                    &host,
                    "the server has no usable atelier CLI",
                ));
            } else {
                checks.push(
                    Check::bad(
                        SSH.0,
                        Tier::Connection,
                        SSH.1,
                        Severity::Fail,
                        format!(
                            "Check that `ssh {host}` works from a terminal (host up, key \
                             accepted, host key known)"
                        ),
                    )
                    .detail(detail),
                );
                checks.push(Check::skipped(
                    CLI.0,
                    Tier::Connection,
                    CLI.1,
                    "couldn't log in",
                ));
                checks.push(server_skipped(&host, "couldn't log in"));
            }
            return Report {
                checks,
                actions: Vec::new(),
            };
        }
    };
    checks.push(Check::ok(SSH.0, Tier::Connection, SSH.1).detail(host.clone()));
    checks.push(Check::ok(CLI.0, Tier::Connection, CLI.1).detail(format!(
        "{} — {}, works with this Manager ({})",
        session.engine(),
        session.engine_version(),
        manager_version()
    )));
    match session.doctor(fix, sink) {
        Ok(server) => {
            checks.extend(server.checks);
            Report {
                checks,
                actions: server.actions,
            }
        }
        Err(e) => {
            checks.push(
                Check::bad(
                    "remote.doctor",
                    Tier::Host,
                    "The server's own checks",
                    Severity::Fail,
                    format!("Run `atelier --site {remote_slug} doctor` on {host} to see why"),
                )
                .detail(format!("{e:#}"))
                .on(HostTier::Remote(host)),
            );
            Report {
                checks,
                actions: Vec::new(),
            }
        }
    }
}

/// The placeholder for the server's own checks when they couldn't run.
fn server_skipped(host: &str, why: &str) -> Check {
    Check::skipped("remote.doctor", Tier::Host, "The server's own checks", why)
        .on(HostTier::Remote(host.to_string()))
}

fn remote_of(site: &Site) -> Result<&Remote> {
    site.remote
        .as_ref()
        .with_context(|| format!("site `{}` is not a remote site", site.slug))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    type Reply = std::result::Result<Output, RunError>;
    type Rule = (&'static str, Box<dyn Fn() -> Reply>);

    fn out(code: i32, stdout: &str, stderr: &str) -> Reply {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    /// Answers each script with the reply of the first rule whose needle it
    /// contains, and records every call.
    struct Fake {
        rules: Vec<Rule>,
        calls: RefCell<Vec<(String, Option<PathBuf>)>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                rules: Vec::new(),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn on(mut self, needle: &'static str, reply: impl Fn() -> Reply + 'static) -> Self {
            self.rules.push((needle, Box::new(reply)));
            self
        }
        fn ran(&self, needle: &str) -> bool {
            self.calls.borrow().iter().any(|(s, _)| s.contains(needle))
        }
    }

    impl Transport for &Fake {
        fn target(&self) -> &str {
            "deploy@example.com"
        }
        fn exec(&self, script: &str, input: Option<&Path>, _: Duration) -> Reply {
            self.calls
                .borrow_mut()
                .push((script.to_string(), input.map(Path::to_path_buf)));
            for (needle, reply) in &self.rules {
                if script.contains(needle) {
                    return reply();
                }
            }
            panic!("unexpected script: {script}");
        }
        fn exec_streaming(
            &self,
            script: &str,
            deadline: Duration,
            sink: &mut dyn FnMut(Pipe, &str),
        ) -> std::result::Result<ExitStatus, RunError> {
            let out = self.exec(script, None, deadline)?;
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                sink(Pipe::Stdout, line);
            }
            for line in String::from_utf8_lossy(&out.stderr).lines() {
                sink(Pipe::Stderr, line);
            }
            Ok(out.status)
        }
    }

    fn probe_reply(clis: &str) -> Reply {
        out(0, &format!("arch\tx86_64\nhome\t/home/deploy\n{clis}"), "")
    }

    const STATUS_JSON: &str = r#"{"installed":true,"running":true,"reachable":true,
        "console_url":"http://localhost:41221/atelier","site_url":"http://localhost:41221/",
        "image":"ghcr.io/aincient-labs/atelier:latest","channel":"stable",
        "image_digest":null,"version":"v0.20.0"}"#;

    fn current() -> String {
        format!("atelier {MANAGER_VERSION}")
    }

    #[test]
    fn status_runs_the_servers_cli_and_parses_its_json() {
        let cli = format!("cli\t/usr/local/bin/atelier\t{}\n", current());
        let fake = Fake::new()
            .on("uname -m", move || probe_reply(&cli))
            .on("app status --json", || out(0, STATUS_JSON, ""));
        let session = Session::connect(&fake, "prod", &EngineOptions::default()).unwrap();
        assert_eq!(session.engine(), "/usr/local/bin/atelier");
        let st = session.attach_check().unwrap();
        assert!(st.running && st.installed);
        assert_eq!(st.version.as_deref(), Some("v0.20.0"));
        assert!(fake.ran("/usr/local/bin/atelier --site prod app status --json"));
        assert!(!fake.ran("cat >"), "nothing uploaded");
    }

    #[test]
    fn a_stopped_or_missing_site_is_not_attached() {
        let cli = format!("cli\t/usr/local/bin/atelier\t{}\n", current());
        let stopped = STATUS_JSON.replace(r#""running":true"#, r#""running":false"#);
        let fake = Fake::new()
            .on("uname -m", move || probe_reply(&cli))
            .on("app status --json", move || out(0, &stopped, ""));
        let s = Session::connect(&fake, "prod", &EngineOptions::default()).unwrap();
        let err = format!("{:#}", s.attach_check().unwrap_err());
        assert!(
            err.contains("not running") && err.contains("example.com"),
            "{err}"
        );

        let cli = format!("cli\t/usr/local/bin/atelier\t{}\n", current());
        let fake = Fake::new()
            .on("uname -m", move || probe_reply(&cli))
            .on("app status --json", || {
                out(1, "", "error: no site named `prod` (registered: default)\n")
            });
        let s = Session::connect(&fake, "prod", &EngineOptions::default()).unwrap();
        let err = format!("{:#}", s.attach_check().unwrap_err());
        assert!(
            err.contains("exit 1")
                && err.contains("no site named `prod`")
                && err.contains("example.com"),
            "{err}"
        );
    }

    fn temp_binary() -> (PrivateDir, PathBuf) {
        let dir = PrivateDir::new("atelier-remote-test").unwrap();
        let path = dir.path().join("atelier");
        std::fs::write(&path, b"\x7fELF fake").unwrap();
        (dir, path)
    }

    #[test]
    fn a_server_without_a_cli_gets_ours_copied_verified_and_moved_into_place() {
        let (_dir, bin) = temp_binary();
        let fake = Fake::new()
            .on("uname -m", || probe_reply(""))
            .on("cat >", || out(0, "", ""))
            .on("--version", || out(0, &current(), ""))
            .on("mv -f", || out(0, "", ""));
        let opts = EngineOptions {
            binary: Some(bin.clone()),
            ..Default::default()
        };
        let s = Session::connect(&fake, "default", &opts).unwrap();
        assert_eq!(s.engine(), "/home/deploy/.local/bin/atelier");
        let calls = fake.calls.borrow();
        let upload = calls.iter().find(|(s, _)| s.contains("cat >")).unwrap();
        assert!(upload.0.starts_with("umask 077;"), "{}", upload.0);
        assert_eq!(upload.1.as_deref(), Some(bin.as_path()), "binary is stdin");
        let order: Vec<_> = calls
            .iter()
            .map(|(s, _)| s.split_whitespace().next().unwrap().to_string())
            .collect();
        assert_eq!(order, ["printf", "umask", "chmod", "mv"]);
    }

    #[test]
    fn a_copied_binary_that_reports_the_wrong_version_is_removed() {
        let (_dir, bin) = temp_binary();
        let fake = Fake::new()
            .on("uname -m", || probe_reply(""))
            .on("cat >", || out(0, "", ""))
            .on("--version", || out(126, "", "cannot execute binary file"))
            .on("rm -f", || out(0, "", ""));
        let opts = EngineOptions {
            binary: Some(bin),
            ..Default::default()
        };
        let err = Session::connect(&fake, "default", &opts).err().unwrap();
        assert!(format!("{err:#}").contains("cannot execute"), "{err:#}");
        assert!(fake.ran("rm -f"));
        assert!(!fake.ran("mv -f"));
    }

    #[test]
    fn an_older_cli_is_refused_unless_replacing_is_allowed() {
        let fake = Fake::new().on("uname -m", || {
            probe_reply("cli\t/usr/local/bin/atelier\tatelier 0.0.9\n")
        });
        let err = Session::connect(&fake, "prod", &EngineOptions::default())
            .err()
            .unwrap();
        let old = err.downcast_ref::<EngineTooOld>().expect("typed error");
        assert_eq!(old.path, "/usr/local/bin/atelier");
        assert!(err.to_string().contains("older than this Manager"));
        assert_eq!(fake.calls.borrow().len(), 1, "nothing copied");

        let (_dir, bin) = temp_binary();
        let fake = Fake::new()
            .on("uname -m", || {
                probe_reply("cli\t/usr/local/bin/atelier\tatelier 0.0.9\n")
            })
            .on("cat >", || out(0, "", ""))
            .on("--version", || out(0, &current(), ""))
            .on("mv -f", || out(0, "", ""));
        let opts = EngineOptions {
            binary: Some(bin),
            replace_older: true,
            ..Default::default()
        };
        let s = Session::connect(&fake, "prod", &opts).unwrap();
        assert_eq!(s.engine(), "/home/deploy/.local/bin/atelier");
    }

    #[test]
    fn a_newer_local_bin_wins_over_an_older_one_on_path() {
        let clis = format!(
            "cli\t/usr/bin/atelier\tatelier 0.0.1\ncli\t/home/deploy/.local/bin/atelier\t{}\n",
            current()
        );
        let fake = Fake::new().on("uname -m", move || probe_reply(&clis));
        let s = Session::connect(&fake, "prod", &EngineOptions::default()).unwrap();
        assert_eq!(s.engine(), "/home/deploy/.local/bin/atelier");
    }

    #[test]
    fn auth_failure_and_timeout_name_the_host() {
        let fake = Fake::new().on("uname -m", || {
            out(
                255,
                "",
                "deploy@example.com: Permission denied (publickey).\n",
            )
        });
        let err = format!(
            "{:#}",
            Session::connect(&fake, "prod", &EngineOptions::default())
                .err()
                .unwrap()
        );
        assert!(
            err.contains("deploy@example.com refused the login"),
            "{err}"
        );

        let fake = Fake::new().on("uname -m", || {
            out(255, "", "ssh: Could not resolve hostname nope\n")
        });
        let err = format!(
            "{:#}",
            Session::connect(&fake, "prod", &EngineOptions::default())
                .err()
                .unwrap()
        );
        assert!(
            err.contains("could not connect to deploy@example.com"),
            "{err}"
        );

        let fake = Fake::new().on("uname -m", || {
            Err(RunError::TimedOut {
                command: "ssh".into(),
                after: PROBE_DEADLINE,
            })
        });
        let err = format!(
            "{:#}",
            Session::connect(&fake, "prod", &EngineOptions::default())
                .err()
                .unwrap()
        );
        assert!(
            err.contains("no answer from deploy@example.com within 30s"),
            "{err}"
        );
    }

    #[test]
    fn an_unsupported_architecture_is_refused_before_copying() {
        let fake = Fake::new().on("uname -m", || out(0, "arch\triscv64\nhome\t/h\n", ""));
        let err = Session::connect(&fake, "prod", &EngineOptions::default())
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("riscv64"), "{err:#}");
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn run_quotes_every_argument() {
        let cli = format!("cli\t/opt/my tools/atelier\t{}\n", current());
        let fake = Fake::new()
            .on("uname -m", move || probe_reply(&cli))
            .on("export", || out(0, "", ""));
        let s = Session::connect(&fake, "prod", &EngineOptions::default()).unwrap();
        s.run(&["site", "export", "it's; rm -rf /"], CLI_DEADLINE)
            .unwrap();
        assert!(fake.ran(r#"'/opt/my tools/atelier' --site prod site export 'it'\''s; rm -rf /'"#));
    }

    #[test]
    fn the_ssh_command_is_batch_mode_with_a_deadline_and_a_safe_target() {
        let ssh = Ssh::with_identity("deploy@example.com", None);
        let cmd = ssh.command("echo hi");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(cmd.get_program(), "ssh");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}")));
        let n = args.len();
        assert_eq!(
            &args[n - 3..],
            ["--", "deploy@example.com", "sh -c 'echo hi'"]
        );
        assert!(!args.iter().any(|a| a == "-i"));

        let key = ssh_key::materialise("not really a key\n").unwrap();
        let key_path = key.path().to_path_buf();
        let ssh = Ssh::with_identity("h", Some(key));
        let args: Vec<String> = ssh
            .command("true")
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], key_path.to_string_lossy());
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
        drop(ssh);
        assert!(!key_path.exists(), "the key file goes with the transport");
    }

    #[test]
    fn versions_and_quoting() {
        let v = Version::find("atelier 0.12.3").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (0, 12, 3));
        assert_eq!(Version::find("v1.2.3-rc.1").unwrap().to_string(), "1.2.3");
        assert_eq!(Version::find("atelier"), None);
        assert_eq!(Version::find(""), None);
        let want = Version::find("0.12.0").unwrap();
        assert!(Version::find("0.12.0").unwrap().satisfies(want));
        assert!(Version::find("0.13.0").unwrap().satisfies(want));
        assert!(Version::find("1.0.0").unwrap().satisfies(want));
        assert!(!Version::find("0.11.9").unwrap().satisfies(want));
        assert_eq!(sh_quote("app"), "app");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn release_assets_and_checksums() {
        assert_eq!(linux_triple("x86_64").unwrap(), "x86_64-unknown-linux-gnu");
        assert_eq!(linux_triple("arm64").unwrap(), "aarch64-unknown-linux-gnu");
        assert!(linux_triple("armv7l").is_err());
        assert_eq!(
            release_asset_url("x86_64-unknown-linux-gnu"),
            format!(
                "https://github.com/aincient-labs/manager/releases/download/v{MANAGER_VERSION}/atelier-x86_64-unknown-linux-gnu.tar.xz"
            )
        );
        let dir = PrivateDir::new("atelier-sha-test").unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_sha256(&file, &format!("{abc} *a\n")).unwrap();
        verify_sha256(&file, &abc.to_uppercase()).unwrap();
        assert!(verify_sha256(&file, &"0".repeat(64)).is_err());
        assert!(verify_sha256(&file, "garbage").is_err());
    }

    #[test]
    fn the_probe_parses_paths_and_versions() {
        let p = Probe::parse(
            "arch\taarch64\nhome\t/root\ncli\t/usr/bin/atelier\tatelier 0.12.0\ncli\t/root/.local/bin/atelier\t\n",
        );
        assert_eq!(p.arch, "aarch64");
        assert_eq!(p.home, "/root");
        assert_eq!(p.clis.len(), 2);
        assert_eq!(p.clis[1].reported, "");
    }

    fn session(fake: &Fake) -> Session<&Fake> {
        Session::connect(fake, "prod", &EngineOptions::default()).unwrap()
    }

    fn with_cli(fake: Fake) -> Fake {
        let cli = format!("cli\t/usr/local/bin/atelier\t{}\n", current());
        fake.on("uname -m", move || probe_reply(&cli))
    }

    /// The streamed script of the last call.
    fn last(fake: &Fake) -> String {
        fake.calls.borrow().last().unwrap().0.clone()
    }

    #[test]
    fn every_action_sends_the_matching_local_subcommand() {
        let export = ExportOptions {
            out: Some("/srv/out dir".into()),
            base_url: Some("https://example.com".into()),
            zip: true,
            include_config: false,
            include_users: true,
            skip_link_check: true,
        };
        let freeze = FreezeOptions {
            label: Some("spring".into()),
            keep: true,
            no_serve: false,
            force: true,
            base_url: None,
        };
        let cases: Vec<(Action, &str, Duration)> = vec![
            (Action::Status, "app status --json", CLI_DEADLINE),
            (
                Action::CheckUpdate,
                "app check-update --json",
                CHECK_UPDATE_DEADLINE,
            ),
            (
                Action::Update {
                    to: None,
                    yes: true,
                },
                "app update --yes",
                UPDATE_DEADLINE,
            ),
            (
                Action::Update {
                    to: Some("0.21.0".into()),
                    yes: false,
                },
                "app update --to 0.21.0",
                UPDATE_DEADLINE,
            ),
            (Action::Start, "app start", LIFECYCLE_DEADLINE),
            (Action::Stop, "app stop", LIFECYCLE_DEADLINE),
            (
                Action::Backup {
                    label: Some("pre move".into()),
                },
                "data backup --label 'pre move'",
                DATA_DEADLINE,
            ),
            (Action::Backup { label: None }, "data backup", DATA_DEADLINE),
            (Action::BackupList, "data list", CLI_DEADLINE),
            (
                Action::Restore {
                    file: "/home/deploy/.atelier/backups/a b.tar.gz".into(),
                },
                "data restore --yes -- '/home/deploy/.atelier/backups/a b.tar.gz'",
                DATA_DEADLINE,
            ),
            (
                Action::Export(export),
                "site export --out '/srv/out dir' --base-url https://example.com --zip \
                 --include-users --skip-link-check",
                WORK_DEADLINE,
            ),
            (
                Action::Freeze(freeze),
                "site freeze --label spring --keep --force",
                WORK_DEADLINE,
            ),
            (Action::Snapshots, "site snapshots --json", CLI_DEADLINE),
            (
                Action::UseSnapshot { id: "s-1".into() },
                "site use s-1",
                CLI_DEADLINE,
            ),
            (Action::Live, "site live", CLI_DEADLINE),
            (
                Action::Prune { keep: 3 },
                "site prune --keep 3",
                CLI_DEADLINE,
            ),
            (
                Action::Doctor { fix: false },
                "doctor --json",
                DOCTOR_DEADLINE,
            ),
            (
                Action::Doctor { fix: true },
                "doctor --json --fix",
                WORK_DEADLINE,
            ),
        ];
        for (action, args, deadline) in cases {
            let fake = with_cli(Fake::new()).on("--site prod", || out(0, "done\n", ""));
            let s = session(&fake);
            let mut lines = Vec::new();
            s.run_action(&action, &mut |_, l| lines.push(l.to_string()))
                .unwrap();
            assert_eq!(
                last(&fake),
                format!("/usr/local/bin/atelier --site prod {args}"),
                "{action:?}"
            );
            assert_eq!(action.deadline(), deadline, "{action:?}");
            assert_eq!(lines, ["done"], "output streamed through");
        }
    }

    #[test]
    fn a_failing_remote_command_carries_its_exit_code_and_streams_its_message() {
        let fake = with_cli(Fake::new()).on("data backup", || {
            out(
                3,
                "Backing up…\n",
                "error: the database container is down\n",
            )
        });
        let s = session(&fake);
        let mut seen = Vec::new();
        let err = s
            .run_action(&Action::Backup { label: None }, &mut |p, l| {
                seen.push((p, l.to_string()))
            })
            .unwrap_err();
        let exit = err.downcast_ref::<RemoteExit>().expect("typed exit");
        assert_eq!(exit.code, 3);
        assert_eq!(exit.host, "deploy@example.com");
        assert_eq!(
            err.to_string(),
            "`atelier data backup` failed on deploy@example.com (exit 3)"
        );
        assert!(seen.contains(&(
            Pipe::Stderr,
            "error: the database container is down".to_string()
        )));

        // ssh's own 255 is a lost connection, not the CLI's verdict.
        let fake = with_cli(Fake::new()).on("app update", || {
            out(
                255,
                "",
                "Connection to example.com closed by remote host.\n",
            )
        });
        let err = session(&fake)
            .run_action(
                &Action::Update {
                    to: None,
                    yes: true,
                },
                &mut |_, _| {},
            )
            .unwrap_err();
        assert!(err.downcast_ref::<RemoteExit>().is_none());
        assert!(
            err.to_string()
                .contains("lost the connection to deploy@example.com"),
            "{err}"
        );
    }

    const CHECK_JSON: &str = r#"{"image":"ghcr.io/aincient-labs/atelier:latest",
        "current":"sha256:a","latest":"sha256:b","current_version":"v0.20.0",
        "latest_version":"v0.21.0","update_available":true,"problem":null,
        "plan":{"from":"0.20.0","target_image":"ghcr.io/aincient-labs/atelier:latest",
          "steps":[{"image":"ghcr.io/aincient-labs/atelier:v0.20.5","version":"0.20.5",
                    "is_target":false,"reason":"0.21 migrates from 0.20.5 on"},
                   {"image":"ghcr.io/aincient-labs/atelier:latest","version":"0.21.0",
                    "is_target":true,"reason":null}],
          "problem":null,"explicit_target":false}}"#;

    #[test]
    fn json_actions_parse_into_the_local_structs() {
        let fake = with_cli(Fake::new())
            .on("check-update --json", || out(0, CHECK_JSON, ""))
            .on("snapshots --json", || {
                out(
                    0,
                    r#"{"serving":"s-2","snapshots":[{"id":"s-2","pages":12,"keep":true}]}"#,
                    "",
                )
            });
        let s = session(&fake);
        let check = s.check_update().unwrap();
        assert_eq!(check.update_available, Some(true));
        assert!(check.plan.as_ref().unwrap().is_stepped());
        let snaps = s.snapshots().unwrap();
        assert_eq!(snaps.serving, "s-2");
        assert_eq!(snaps.snapshots[0].pages, 12);

        let fake = with_cli(Fake::new()).on("check-update", || out(0, "not json", ""));
        let err = format!("{:#}", session(&fake).check_update().unwrap_err());
        assert!(err.contains("can't read"), "{err}");
    }

    const DOCTOR_JSON: &str = r#"{
  "manager_version": "0.12.0",
  "healthy": false,
  "failures": 1,
  "checks": [
    {"id": "docker.installed", "tier": "host", "label": "Docker installed",
     "severity": "ok", "detail": "27.3.1"},
    {"id": "host.disk", "tier": "host", "label": "Free disk space",
     "severity": "ok", "detail": "41.0 GB free"},
    {"id": "site.pending_updates", "tier": "site", "label": "Database updates applied",
     "severity": "fail", "remedy": "Run doctor --fix", "fix": "run_updates"}
  ],
  "actions": []
}"#;

    #[test]
    fn remote_doctor_labels_every_check_with_the_machine_it_ran_on() {
        // Exit 1 = unhealthy report: still a report.
        let fake = with_cli(Fake::new()).on("doctor --json", || out(1, DOCTOR_JSON, ""));
        let report = doctor_over(&fake, "prod", false, Vec::new(), &mut |_, _| {});
        let by_id = |id: &str| report.checks.iter().find(|c| c.id == id).unwrap();

        for id in ["remote.ssh", "remote.cli"] {
            let c = by_id(id);
            assert_eq!(c.tier, Tier::Connection, "{id}");
            assert_eq!(c.on, HostTier::Local, "{id} ran here");
            assert_eq!(c.severity, Severity::Ok, "{id}");
        }
        assert!(by_id("remote.cli")
            .detail
            .as_deref()
            .unwrap()
            .contains("/usr/local/bin/atelier"));
        let server = HostTier::Remote("deploy@example.com".into());
        for id in ["docker.installed", "host.disk", "site.pending_updates"] {
            assert_eq!(by_id(id).on, server, "{id} ran on the server");
        }
        assert_eq!(by_id("host.disk").detail.as_deref(), Some("41.0 GB free"));
        assert!(!report.healthy());
        assert!(fake.ran("/usr/local/bin/atelier --site prod doctor --json"));
        // Nothing of this machine's host tier sneaks in.
        assert_eq!(
            report
                .checks
                .iter()
                .filter(|c| c.tier == Tier::Host && c.on == HostTier::Local)
                .count(),
            0
        );
        // The marker survives a JSON round trip; local checks omit it.
        let json = serde_json::to_value(by_id("host.disk")).unwrap();
        assert_eq!(json["on"]["remote"], "deploy@example.com");
        let json = serde_json::to_value(by_id("remote.ssh")).unwrap();
        assert!(json.get("on").is_none());
    }

    #[test]
    fn remote_doctor_fix_streams_progress_and_parses_the_final_report() {
        let fake = with_cli(Fake::new()).on("doctor --json --fix", || {
            out(
                0,
                &format!("Run the pending database updates\n[converge] ok\n{DOCTOR_JSON}"),
                "",
            )
        });
        let mut progress = Vec::new();
        let report = doctor_over(&fake, "prod", true, Vec::new(), &mut |_, l| {
            progress.push(l.to_string())
        });
        assert_eq!(
            progress,
            ["Run the pending database updates", "[converge] ok"]
        );
        assert!(report.checks.iter().any(|c| c.id == "site.pending_updates"));
    }

    #[test]
    fn remote_doctor_reports_login_and_engine_failures_without_copying() {
        let fake = Fake::new().on("uname -m", || {
            out(
                255,
                "",
                "deploy@example.com: Permission denied (publickey).\n",
            )
        });
        let report = doctor_over(&fake, "prod", false, Vec::new(), &mut |_, _| {});
        let sev: Vec<_> = report
            .checks
            .iter()
            .map(|c| (c.id.to_string(), c.severity))
            .collect();
        assert_eq!(
            sev,
            [
                ("remote.ssh".to_string(), Severity::Fail),
                ("remote.cli".to_string(), Severity::Skipped),
                ("remote.doctor".to_string(), Severity::Skipped),
            ]
        );
        assert_eq!(
            report.checks[2].on,
            HostTier::Remote("deploy@example.com".into())
        );

        // No CLI on the server: doctor copies nothing, it says so.
        let fake = Fake::new().on("uname -m", || probe_reply(""));
        let report = doctor_over(&fake, "prod", false, Vec::new(), &mut |_, _| {});
        assert_eq!(report.checks[0].severity, Severity::Ok);
        assert_eq!(report.checks[1].id, "remote.cli");
        assert_eq!(report.checks[1].severity, Severity::Fail);
        assert!(!fake.ran("cat >"), "nothing uploaded");
        assert_eq!(fake.calls.borrow().len(), 1);

        // An older CLI: also a failed engine check, not a copy.
        let fake = Fake::new().on("uname -m", || {
            probe_reply("cli\t/usr/bin/atelier\tatelier 0.0.9\n")
        });
        let report = doctor_over(&fake, "prod", false, Vec::new(), &mut |_, _| {});
        assert_eq!(report.checks[1].severity, Severity::Fail);
        assert!(report.checks[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("older than this Manager"));
    }
}
