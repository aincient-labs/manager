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

use crate::docker::{
    output_within, output_within_input, output_within_io, stream_lines_within, Pipe, RunError,
};
use crate::doctor::{Check, HostTier, Report, Severity, Tier};
use crate::ops::{
    check_identity, sha256_file, snapshot_manifest, BackupInfo, ExportOptions, FreezeOptions,
    IdentityCheck, SnapshotList, Status, UpdateCheck,
};
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

    /// Run `script` with stdin closed and its stdout written into the local
    /// file `dest` (created or truncated) — a server file streamed home
    /// without being held in memory. The returned `stdout` is empty.
    fn download(
        &self,
        script: &str,
        dest: &Path,
        deadline: Duration,
    ) -> std::result::Result<Output, RunError>;
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

    fn download(
        &self,
        script: &str,
        dest: &Path,
        deadline: Duration,
    ) -> std::result::Result<Output, RunError> {
        self.start_master();
        let file = create_private(dest).map_err(|source| RunError::Io {
            program: dest.display().to_string(),
            source,
        })?;
        output_within_io(
            &mut self.command(script),
            deadline,
            Stdio::null(),
            Some(file),
        )
    }
}

/// Create (or truncate) `path` readable by its owner only: a backup is the
/// whole site's database.
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
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
    /// `data backup [--label L] [--json]` — see [`Session::backup`].
    Backup { label: Option<String>, json: bool },
    /// `data list` — the server's backups.
    BackupList,
    /// `data restore --yes [--other-site] -- <FILE>`, FILE being a path on the
    /// server (an uploaded one: [`Session::restore_upload`]).
    Restore { file: String, other_site: bool },
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
            Action::Backup { label, json } => {
                opt("--label", label);
                if *json {
                    v.push("--json".into());
                }
            }
            Action::Restore { file, other_site } => {
                v.push("--yes".into());
                if *other_site {
                    v.push("--other-site".into());
                }
                // `--` so a file named like a flag stays a file.
                v.extend(["--".into(), file.clone()]);
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

/// What [`Session::backup`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBackup {
    /// The archive as the server reports it (path on the server).
    pub server: BackupInfo,
    /// The verified copy on this machine; `None` when asked to keep it
    /// server-only.
    pub local: Option<PathBuf>,
}

/// Prints `<sha256 hex> <size>` for the file in `$p` (set before it), with
/// whichever of `sha256sum` / `shasum` the server has.
const DIGEST_SCRIPT: &str = r#"[ -f "$p" ] || { echo "no such file: $p" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then s=$(sha256sum < "$p"); else s=$(shasum -a 256 < "$p"); fi || exit 1
printf '%s %s\n' "${s%% *}" "$(wc -c < "$p" | tr -d ' ')""#;

/// Parse [`DIGEST_SCRIPT`]'s answer.
fn parse_digest(stdout: &str) -> Option<(String, u64)> {
    let mut words = stdout.split_whitespace();
    let sha = words.next()?.to_ascii_lowercase();
    let size = words.next()?.parse().ok()?;
    (sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some((sha, size))
}

/// A file name safe to create locally or on the server: only
/// `[A-Za-z0-9._-]`, never starting with `.`, extension kept.
fn safe_file_name(name: &str) -> Option<String> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.').to_string();
    (!clean.is_empty() && clean.len() <= 200).then_some(clean)
}

impl<T: Transport> Session<T> {
    /// SHA-256 and size of a file on the server, computed there.
    pub fn remote_digest(&self, path: &str) -> Result<(String, u64)> {
        let script = format!("p={}\n{DIGEST_SCRIPT}", sh_quote(path));
        let out = checked(
            &self.transport,
            "checking the archive on the server",
            self.transport.exec(&script, None, DATA_DEADLINE),
        )?;
        parse_digest(&String::from_utf8_lossy(&out.stdout)).with_context(|| {
            format!(
                "{} didn't report a SHA-256 for {path}",
                self.transport.target()
            )
        })
    }

    /// `data backup` on the server (m2-5, D-03), then — unless `home` is
    /// `None` — a verified copy into `home` on this machine. The server's
    /// progress goes to `sink`; its `--json` answer says which archive it
    /// wrote. A server whose CLI has no `data backup --json` is asked without
    /// it and the path read from its "Backup written to" line.
    pub fn backup(
        &self,
        label: Option<String>,
        home: Option<&Path>,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> Result<RemoteBackup> {
        let server = self.backup_on_server(label, sink)?;
        let local = match home {
            Some(dir) => Some(fetch_backup(&self.transport, &server, dir)?),
            None => None,
        };
        Ok(RemoteBackup { server, local })
    }

    fn backup_on_server(
        &self,
        label: Option<String>,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> Result<BackupInfo> {
        let host = self.transport.target().to_string();
        let mut stdout = String::new();
        let mut no_json_flag = false;
        let action = Action::Backup {
            label: label.clone(),
            json: true,
        };
        let result = self.run_action(&action, &mut |pipe, line| match pipe {
            Pipe::Stdout => {
                stdout.push_str(line);
                stdout.push('\n');
            }
            Pipe::Stderr => {
                if line.contains("--json") && line.contains("unexpected argument") {
                    no_json_flag = true;
                }
                sink(pipe, line)
            }
        });
        match result {
            Ok(()) => {
                return serde_json::from_str(&stdout).with_context(|| {
                    format!(
                    "{host} answered `data backup --json` with something the Manager can't read"
                )
                })
            }
            Err(e)
                if no_json_flag && e.downcast_ref::<RemoteExit>().is_some_and(|x| x.code == 2) => {}
            Err(e) => return Err(e),
        }
        // An engine from before `--json`: the human line names the archive.
        let mut path = None;
        self.run_action(&Action::Backup { label, json: false }, &mut |pipe, line| {
            let plain = strip_ansi(line);
            if let Some(rest) = plain.trim().strip_prefix("Backup written to ") {
                path = Some(rest.trim().to_string());
            }
            sink(pipe, line)
        })?;
        let path = path.with_context(|| {
            format!("{host} made a backup but didn't say where — find it with `atelier --site <SLUG> data list` there")
        })?;
        let (sha256, size) = self.remote_digest(&path)?;
        Ok(BackupInfo {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            path,
            size,
            sha256,
            site_id: None,
            site_name: None,
        })
    }

    /// Whether the local archive `file` may be restored onto this site
    /// (m2-6, D-04): its manifest against the server's own `site_id`. Run
    /// before the upload, so another site's archive is refused without
    /// moving a byte; the server's restore checks again.
    pub fn check_restore_identity(&self, file: &Path, other_site: bool) -> Result<IdentityCheck> {
        let st = self.status()?;
        check_identity(
            snapshot_manifest(file).as_ref(),
            st.site_id.as_deref(),
            &format!("site `{}` on {}", self.remote_slug, self.transport.target()),
            other_site,
        )
    }

    /// Restore the local archive `file` onto this site: upload it to a private
    /// temp dir on the server (`umask 077`, `mktemp -d`), check its SHA-256
    /// there, run the server's `data restore` on it (with `--other-site` when
    /// asked), and remove the temp dir whatever happened.
    pub fn restore_upload(
        &self,
        file: &Path,
        other_site: bool,
        sink: &mut dyn FnMut(Pipe, &str),
    ) -> Result<()> {
        let host = self.transport.target().to_string();
        let original = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = safe_file_name(&original).unwrap_or_else(|| "backup.tar.gz".to_string());
        let (size, sha256) = sha256_file(file)?;
        let upload = format!(
            "umask 077\nd=$(mktemp -d \"${{TMPDIR:-/tmp}}/atelier-restore.XXXXXX\") || exit 1\n\
             cat > \"$d\"/{name} || {{ rm -rf \"$d\"; exit 1; }}\n\
             printf '%s\\n' \"$d\"",
            name = sh_quote(&name)
        );
        let out = checked(
            &self.transport,
            "uploading the backup",
            self.transport.exec(&upload, Some(file), DATA_DEADLINE),
        )?;
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !dir.starts_with('/') || dir.contains('\n') {
            bail!("{host} didn't say where it put the uploaded backup");
        }
        let remote_file = format!("{dir}/{name}");
        let outcome = (|| {
            let (theirs, their_size) = self.remote_digest(&remote_file)?;
            if theirs != sha256 || their_size != size {
                bail!(
                    "the upload to {host} arrived damaged ({their_size} bytes, sha256 {theirs}; \
                     sent {size} bytes, sha256 {sha256}) — nothing was restored"
                );
            }
            self.run_action(
                &Action::Restore {
                    file: remote_file.clone(),
                    other_site,
                },
                sink,
            )
        })();
        let _ = self.transport.exec(
            &format!("rm -rf -- {}", sh_quote(&dir)),
            None,
            PROBE_DEADLINE,
        );
        outcome
    }
}

/// Copy the server's archive `info` into `dir` on this machine: stream it
/// with `cat` into a temp name, check its size and SHA-256 against what the
/// server computed, then rename it into place. On any failure the temp file
/// is removed and nothing named like a backup is left behind.
pub fn fetch_backup<T: Transport>(transport: &T, info: &BackupInfo, dir: &Path) -> Result<PathBuf> {
    let host = transport.target();
    let name = info
        .path
        .rsplit('/')
        .next()
        .and_then(safe_file_name)
        .with_context(|| {
            format!(
                "{host} named its backup `{}`, not a usable file name",
                info.path
            )
        })?;
    create_private_dir(dir)?;
    let dest = dir.join(&name);
    let tmp = dir.join(format!(".{name}.part-{}", std::process::id()));
    let result = (|| {
        let out = transport
            .download(
                &format!("cat -- {}", sh_quote(&info.path)),
                &tmp,
                DATA_DEADLINE,
            )
            .map_err(|e| anyhow!("copying the backup from {host}: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "copying the backup from {host} failed: {}",
                err.lines()
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or("no message")
                    .trim()
            );
        }
        // Owner-only whatever the transport created it with: it is the
        // site's whole database.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("could not make {} private", tmp.display()))?;
        }
        let (size, sha) = sha256_file(&tmp)?;
        if size != info.size || sha != info.sha256.to_ascii_lowercase() {
            bail!(
                "the copy from {host} doesn't match the server's archive ({size} bytes, sha256 \
                 {sha}; the server has {} bytes, sha256 {}) — not kept. The backup is still on \
                 {host}: {}",
                info.size,
                info.sha256,
                info.path
            );
        }
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("could not move the copy into {}", dest.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map(|()| dest)
}

/// `dir` and its parents, `dir` itself private to the owner.
fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not make {} private", dir.display()))?;
    }
    Ok(())
}

/// `line` without ANSI colour escapes.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Where a remote site's backups are kept on this machine:
/// `~/.atelier/remote/<slug>/backups` (D-03).
pub fn local_backups_dir(site: &Site) -> PathBuf {
    site.stack().backups_dir()
}

// --- Open console (m2-4, D-02) ----------------------------------------------

/// How the console of a remote site is reached from this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleRoute {
    /// The domain recorded for the site: open these directly.
    Domain {
        console_url: String,
        site_url: String,
    },
    /// No domain: an SSH tunnel from a local port in
    /// [`crate::sites::TUNNEL_PORTS`] to the appliance's port on the server.
    Tunnel {
        /// The appliance's published port on the server (from its status).
        server_port: u16,
        /// The console's path, as the server's own console URL has it.
        console_path: String,
    },
}

/// Split `scheme://authority/path` into (`scheme://authority`, `/path`).
fn split_url(url: &str) -> Option<(&str, &str)> {
    let after = url.find("://")? + 3;
    match url[after..].find('/') {
        Some(i) => Some((&url[..after + i], &url[after + i..])),
        None => Some((url, "")),
    }
}

/// The port in an `http(s)://host[:port]` authority (80/443 by scheme).
fn port_of(base: &str) -> Option<u16> {
    let (scheme, authority) = base.split_once("://")?;
    let host_port = authority.rsplit('@').next()?;
    match host_port.rsplit_once(':') {
        Some((_, port)) if !port.contains(']') => port.parse().ok(),
        _ => match scheme {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        },
    }
}

/// A recorded domain as a URL base: `https://` unless it names a scheme.
pub fn domain_base(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('/');
    if d.starts_with("http://") || d.starts_with("https://") {
        d.to_string()
    } else {
        format!("https://{d}")
    }
}

/// Decide how to reach a remote site's console. The PATH always comes from
/// the server's own status (the same `Stack::console_url` derivation a local
/// site uses), so nothing here hardcodes `/atelier`; only the origin changes.
pub fn console_route(remote: &Remote, status: &Status) -> Result<ConsoleRoute> {
    let (server_base, console_path) = split_url(&status.console_url).with_context(|| {
        format!(
            "{} reported a console URL the Manager can't read: `{}`",
            remote.host, status.console_url
        )
    })?;
    let site_path = split_url(&status.site_url).map(|(_, p)| p).unwrap_or("");
    if let Some(domain) = &remote.domain {
        let base = domain_base(domain);
        return Ok(ConsoleRoute::Domain {
            console_url: format!("{base}{console_path}"),
            site_url: format!("{base}{site_path}"),
        });
    }
    let server_port = port_of(server_base).with_context(|| {
        format!(
            "{} reported a console URL without a port: `{}`",
            remote.host, status.console_url
        )
    })?;
    Ok(ConsoleRoute::Tunnel {
        server_port,
        console_path: console_path.to_string(),
    })
}

/// The server's status as this machine sees it: with a recorded domain, the
/// console and site URLs on that domain; without one, both empty (only a
/// tunnel reaches it — `app open`), never the server's own localhost URLs.
pub fn status_for_here(remote: &Remote, mut status: Status) -> (Status, ConsoleRoute) {
    let route = console_route(remote, &status).unwrap_or(ConsoleRoute::Tunnel {
        server_port: 0,
        console_path: String::new(),
    });
    match &route {
        ConsoleRoute::Domain {
            console_url,
            site_url,
        } => {
            status.console_url = console_url.clone();
            status.site_url = site_url.clone();
        }
        ConsoleRoute::Tunnel { .. } => {
            status.console_url = String::new();
            status.site_url = String::new();
        }
    }
    (status, route)
}

/// How long a new tunnel may take to start listening.
pub const TUNNEL_READY: Duration = Duration::from_secs(CONNECT_TIMEOUT_SECS + 5);
/// How many tunnel ports to try when one is taken between probe and bind.
const TUNNEL_ATTEMPTS: usize = 3;

impl Ssh {
    /// `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<server_port> <target>`, with
    /// its own connection (never the shared master: a forward held by the
    /// master would outlive this tunnel) and `ExitOnForwardFailure`, so a
    /// port someone else took makes it exit instead of idling uselessly.
    pub fn tunnel_command(&self, local_port: u16, server_port: u16) -> Command {
        let mut cmd = Command::new("ssh");
        let mut args = self.base_args();
        // Drop the ControlPath the base args may carry.
        if let Some(i) = args
            .iter()
            .position(|a| a.to_str().is_some_and(|s| s.starts_with("ControlPath=")))
        {
            args.drain(i - 1..=i);
        }
        cmd.args(args)
            .args(["-o", "ControlPath=none"])
            .args(["-o", "ExitOnForwardFailure=yes"])
            .arg("-N")
            .arg("-L")
            .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{server_port}"))
            .arg("--")
            .arg(&self.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// A running SSH tunnel to a remote console. Dropping it closes the tunnel;
/// a front-end holds it for as long as the console should stay reachable.
#[derive(Debug)]
pub struct Tunnel {
    child: std::process::Child,
    /// The local port it listens on (127.0.0.1 only).
    pub local_port: u16,
    /// The console through it: `http://127.0.0.1:<port><path>`. 127.0.0.1,
    /// not `localhost`, so its cookies stay apart from local sites'.
    pub url: String,
}

impl Tunnel {
    /// Block until the tunnel ends (the connection drops, or the process is
    /// interrupted). An error carries ssh's last words.
    pub fn wait(&mut self) -> Result<()> {
        let status = self.child.wait().context("waiting for the ssh tunnel")?;
        if status.success() {
            return Ok(());
        }
        let mut err = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut err);
        }
        bail!(
            "the tunnel closed: {}",
            err.lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("ssh exited")
                .trim()
        )
    }

    /// Whether ssh is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `cmd` and wait until something accepts on 127.0.0.1:`port`, within
/// `ready`. `Ok(None)` when the child exited first (e.g. the port was taken
/// after all, under `ExitOnForwardFailure`) — with its stderr — so the caller
/// can try another port.
pub fn spawn_until_listening(
    mut cmd: Command,
    port: u16,
    url: String,
    ready: Duration,
) -> Result<std::result::Result<Tunnel, String>> {
    let mut child = cmd
        .spawn()
        .context("couldn't run ssh — is OpenSSH installed?")?;
    let until = Instant::now() + ready;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut err);
            }
            return Ok(Err(err.trim().to_string()));
        }
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return Ok(Ok(Tunnel {
                child,
                local_port: port,
                url,
            }));
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the tunnel didn't start listening on 127.0.0.1:{port} within {}s",
                ready.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Open a tunnel to `server_port` on `ssh`'s server, on the first free port of
/// [`crate::sites::TUNNEL_PORTS`], and wait until it answers.
pub fn open_tunnel(ssh: &Ssh, server_port: u16, console_path: &str) -> Result<Tunnel> {
    let mut tried = Vec::new();
    let mut last_err = String::new();
    for _ in 0..TUNNEL_ATTEMPTS {
        let port = crate::sites::next_tunnel_port(|p| {
            !tried.contains(&p) && crate::sites::loopback_port_is_free(p)
        })
        .with_context(|| {
            format!(
                "every tunnel port ({}–{}) on this machine is in use",
                crate::sites::TUNNEL_PORTS.start(),
                crate::sites::TUNNEL_PORTS.end()
            )
        })?;
        tried.push(port);
        let url = format!("http://127.0.0.1:{port}{console_path}");
        match spawn_until_listening(
            ssh.tunnel_command(port, server_port),
            port,
            url,
            TUNNEL_READY,
        )? {
            Ok(tunnel) => return Ok(tunnel),
            Err(err) => last_err = err,
        }
    }
    bail!(
        "couldn't open an SSH tunnel to {}: {}",
        ssh.target(),
        if last_err.is_empty() {
            "ssh exited"
        } else {
            &last_err
        }
    )
}

/// What opening a remote console gives a front-end: the URL to open, and —
/// when it goes through a tunnel — the [`Tunnel`] to hold while it is used.
#[derive(Debug)]
pub struct ConsoleAccess {
    pub url: String,
    pub tunnel: Option<Tunnel>,
}

/// Reach `site`'s console (m2-4, D-02): its recorded domain, else a tunnel.
/// The server's status supplies the console path and its port.
pub fn console_access(site: &Site) -> Result<ConsoleAccess> {
    let session = open(site, &EngineOptions::from_env())?;
    console_access_in(&session, site)
}

/// [`console_access`] over a session the caller already holds.
pub fn console_access_in<T: Transport>(session: &Session<T>, site: &Site) -> Result<ConsoleAccess> {
    let remote = remote_of(site)?;
    let status = session.status()?;
    match console_route(remote, &status)? {
        ConsoleRoute::Domain { console_url, .. } => Ok(ConsoleAccess {
            url: console_url,
            tunnel: None,
        }),
        ConsoleRoute::Tunnel {
            server_port,
            console_path,
        } => {
            let ssh = Ssh::new(remote, &ssh_key::Keychain)?;
            let tunnel = open_tunnel(&ssh, server_port, &console_path)?;
            Ok(ConsoleAccess {
                url: tunnel.url.clone(),
                tunnel: Some(tunnel),
            })
        }
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
        fn download(&self, script: &str, dest: &Path, deadline: Duration) -> Reply {
            let mut out = self.exec(script, None, deadline)?;
            std::fs::write(dest, std::mem::take(&mut out.stdout)).unwrap();
            Ok(out)
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
                    json: true,
                },
                "data backup --label 'pre move' --json",
                DATA_DEADLINE,
            ),
            (
                Action::Backup {
                    label: None,
                    json: false,
                },
                "data backup",
                DATA_DEADLINE,
            ),
            (Action::BackupList, "data list", CLI_DEADLINE),
            (
                Action::Restore {
                    file: "/home/deploy/.atelier/backups/a b.tar.gz".into(),
                    other_site: false,
                },
                "data restore --yes -- '/home/deploy/.atelier/backups/a b.tar.gz'",
                DATA_DEADLINE,
            ),
            (
                Action::Restore {
                    file: "/tmp/x.tar.gz".into(),
                    other_site: true,
                },
                "data restore --yes --other-site -- /tmp/x.tar.gz",
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
            .run_action(
                &Action::Backup {
                    label: None,
                    json: false,
                },
                &mut |p, l| seen.push((p, l.to_string())),
            )
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

    // --- m2-4 console ---------------------------------------------------------

    fn status_with(console_url: &str, site_url: &str) -> Status {
        let mut st: Status = serde_json::from_str(STATUS_JSON).unwrap();
        st.console_url = console_url.into();
        st.site_url = site_url.into();
        st
    }

    fn remote_with(domain: Option<&str>) -> Remote {
        Remote {
            host: "deploy@example.com".into(),
            slug: "prod".into(),
            key: KeyMode::Ssh,
            domain: domain.map(str::to_string),
        }
    }

    #[test]
    fn a_recorded_domain_opens_the_console_there_with_the_servers_path() {
        let st = status_with("http://localhost:41221/atelier", "http://localhost:41221");
        let route = console_route(&remote_with(Some("cms.example.com")), &st).unwrap();
        assert_eq!(
            route,
            ConsoleRoute::Domain {
                console_url: "https://cms.example.com/atelier".into(),
                site_url: "https://cms.example.com".into(),
            }
        );
        // An explicit scheme/port is kept; the path still comes from the server.
        let st = status_with("http://localhost:41230/console", "http://localhost:41230/");
        let route = console_route(&remote_with(Some("http://10.0.0.5:8080/")), &st).unwrap();
        assert_eq!(
            route,
            ConsoleRoute::Domain {
                console_url: "http://10.0.0.5:8080/console".into(),
                site_url: "http://10.0.0.5:8080/".into(),
            }
        );
    }

    #[test]
    fn no_domain_means_a_tunnel_to_the_servers_console_port() {
        let st = status_with("http://localhost:41230/atelier", "http://localhost:41230");
        assert_eq!(
            console_route(&remote_with(None), &st).unwrap(),
            ConsoleRoute::Tunnel {
                server_port: 41230,
                console_path: "/atelier".into(),
            }
        );
        assert!(console_route(&remote_with(None), &status_with("garbage", "")).is_err());
    }

    #[test]
    fn remote_status_never_shows_the_servers_localhost() {
        let st = status_with("http://localhost:41221/atelier", "http://localhost:41221");
        let (here, route) = status_for_here(&remote_with(None), st.clone());
        assert!(matches!(route, ConsoleRoute::Tunnel { .. }));
        assert_eq!(
            (here.console_url.as_str(), here.site_url.as_str()),
            ("", "")
        );
        let (here, _) = status_for_here(&remote_with(Some("cms.example.com")), st);
        assert_eq!(here.console_url, "https://cms.example.com/atelier");
    }

    #[test]
    fn the_tunnel_listens_on_loopback_in_its_own_range_without_the_shared_master() {
        let ssh = Ssh::with_identity("deploy@example.com", None);
        let cmd = ssh.tunnel_command(41300, 41221);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(
            joined.contains("-N -L 127.0.0.1:41300:127.0.0.1:41221 -- deploy@example.com"),
            "{joined}"
        );
        assert!(joined.contains("ExitOnForwardFailure=yes"), "{joined}");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert_eq!(
            args.iter()
                .filter(|a| a.starts_with("ControlPath="))
                .collect::<Vec<_>>(),
            vec!["ControlPath=none"],
            "{joined}"
        );
        assert!(!joined.contains("ControlMaster"), "{joined}");
    }

    #[test]
    fn a_tunnel_is_ready_once_its_port_answers_and_closes_on_drop() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stderr(Stdio::piped());
        let mut tunnel = spawn_until_listening(cmd, port, "u".into(), Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(tunnel.local_port, port);
        assert!(tunnel.is_alive());
        let pid = tunnel.child.id();
        drop(tunnel);
        // Reaped on drop: the pid no longer names a live `sleep`.
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "tunnel process outlived its handle");

        // ssh exiting first (port taken, login refused) is reported, not waited out.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unused = free.local_addr().unwrap().port();
        drop(free);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo 'bind: Address already in use' >&2; exit 255"])
            .stderr(Stdio::piped());
        let got = spawn_until_listening(cmd, unused, "u".into(), Duration::from_secs(5)).unwrap();
        assert_eq!(got.unwrap_err(), "bind: Address already in use");
    }

    // --- m2-5 backups home ----------------------------------------------------

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    const ARCHIVE: &[u8] = b"pretend this is a tar.gz of the whole site";
    const SERVER_PATH: &str = "/home/deploy/.atelier/backups/aincient-20261030-101500.tar.gz";

    fn backup_json(sha: &str, size: usize) -> String {
        serde_json::to_string(&BackupInfo {
            path: SERVER_PATH.into(),
            name: "aincient-20261030-101500.tar.gz".into(),
            size: size as u64,
            sha256: sha.into(),
            site_id: Some("11111111-1111-4111-8111-111111111111".into()),
            site_name: Some("prod".into()),
        })
        .unwrap()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn a_backup_is_copied_home_and_verified() {
        let json = backup_json(&sha_hex(ARCHIVE), ARCHIVE.len());
        let fake = with_cli(Fake::new())
            .on("data backup --json", move || out(0, &json, "Backing up…\n"))
            .on("cat --", || {
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: ARCHIVE.to_vec(),
                    stderr: Vec::new(),
                })
            });
        let home = PrivateDir::new("atelier-backup-home").unwrap();
        let dir = home.path().join("remote/prod/backups");
        let mut progress = Vec::new();
        let got = session(&fake)
            .backup(None, Some(&dir), &mut |_, l| progress.push(l.to_string()))
            .unwrap();
        assert_eq!(got.server.path, SERVER_PATH);
        let local = got.local.unwrap();
        assert_eq!(local, dir.join("aincient-20261030-101500.tar.gz"));
        assert_eq!(std::fs::read(&local).unwrap(), ARCHIVE);
        assert_eq!(files_in(&dir), vec!["aincient-20261030-101500.tar.gz"]);
        assert!(progress.contains(&"Backing up…".to_string()));
        assert!(fake.ran(&format!("cat -- {SERVER_PATH}")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&local).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_damaged_copy_is_removed_and_reported() {
        let json = backup_json(&sha_hex(ARCHIVE), ARCHIVE.len());
        let fake = with_cli(Fake::new())
            .on("data backup --json", move || out(0, &json, ""))
            .on("cat --", || {
                out(0, "pretend this is a tar.gz of the whole sitf", "")
            });
        let home = PrivateDir::new("atelier-backup-home").unwrap();
        let dir = home.path().join("backups");
        let err = session(&fake)
            .backup(None, Some(&dir), &mut |_, _| {})
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("doesn't match") && text.contains(SERVER_PATH),
            "{text}"
        );
        assert!(files_in(&dir).is_empty(), "{:?}", files_in(&dir));

        // A failed transfer leaves nothing behind either.
        let json = backup_json(&sha_hex(ARCHIVE), ARCHIVE.len());
        let fake = with_cli(Fake::new())
            .on("data backup --json", move || out(0, &json, ""))
            .on("cat --", || out(1, "partial", "cat: Permission denied\n"));
        let err = session(&fake)
            .backup(None, Some(&dir), &mut |_, _| {})
            .unwrap_err();
        assert!(format!("{err:#}").contains("Permission denied"), "{err:#}");
        assert!(files_in(&dir).is_empty());
    }

    #[test]
    fn no_copy_keeps_the_backup_on_the_server() {
        let json = backup_json(&sha_hex(ARCHIVE), ARCHIVE.len());
        let fake =
            with_cli(Fake::new()).on("data backup --label pre --json", move || out(0, &json, ""));
        let got = session(&fake)
            .backup(Some("pre".into()), None, &mut |_, _| {})
            .unwrap();
        assert!(got.local.is_none());
        assert!(fake.ran("data backup --label pre --json"));
        assert!(!fake.ran("cat --"));
    }

    #[test]
    fn an_engine_without_backup_json_still_gets_its_backup_fetched() {
        let sha = sha_hex(ARCHIVE);
        let digest = format!("{sha} {}\n", ARCHIVE.len());
        let fake = with_cli(Fake::new())
            .on("data backup --json", || {
                out(2, "", "error: unexpected argument '--json' found\n")
            })
            .on("data backup", || {
                out(
                    0,
                    &format!("\u{1b}[32mBackup written to\u{1b}[0m {SERVER_PATH}\n"),
                    "",
                )
            })
            .on("sha256sum", move || out(0, &digest, ""))
            .on("cat --", || {
                out(0, std::str::from_utf8(ARCHIVE).unwrap(), "")
            });
        let home = PrivateDir::new("atelier-backup-home").unwrap();
        let got = session(&fake)
            .backup(None, Some(home.path()), &mut |_, _| {})
            .unwrap();
        assert_eq!(got.server.path, SERVER_PATH);
        assert_eq!(got.server.sha256, sha);
        assert_eq!(std::fs::read(got.local.unwrap()).unwrap(), ARCHIVE);
    }

    // --- m2-6 restore of a laptop archive ---------------------------------------

    fn local_archive(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let stage = dir.join(format!("stage-{name}"));
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("manifest.json"), manifest).unwrap();
        let file = dir.join(name);
        assert!(Command::new("tar")
            .arg("czf")
            .arg(&file)
            .arg("-C")
            .arg(&stage)
            .arg("manifest.json")
            .status()
            .unwrap()
            .success());
        file
    }

    const TMP_DIR: &str = "/tmp/atelier-restore.Ab12Cd";

    #[test]
    fn a_laptop_archive_is_uploaded_verified_restored_and_cleaned_up() {
        let scratch = PrivateDir::new("atelier-upload").unwrap();
        let file = local_archive(scratch.path(), "aincient-x.tar.gz", "{}");
        let bytes = std::fs::read(&file).unwrap();
        let digest = format!("{} {}\n", sha_hex(&bytes), bytes.len());
        let fake = with_cli(Fake::new())
            .on("mktemp -d", || out(0, &format!("{TMP_DIR}\n"), ""))
            .on("sha256sum", move || out(0, &digest, ""))
            .on("data restore", || out(0, "Restore complete.\n", ""))
            .on("rm -rf --", || out(0, "", ""));
        session(&fake)
            .restore_upload(&file, true, &mut |_, _| {})
            .unwrap();
        let calls = fake.calls.borrow();
        let scripts: Vec<&str> = calls.iter().skip(1).map(|(s, _)| s.as_str()).collect();
        assert_eq!(scripts.len(), 4, "{scripts:#?}");
        // 1. upload under umask 077 into a fresh mktemp dir, from the local file.
        assert!(scripts[0].starts_with("umask 077") && scripts[0].contains("mktemp -d"));
        assert_eq!(calls[1].1.as_deref(), Some(file.as_path()));
        // 2. verified on the server, 3. restored from there, 4. removed.
        assert!(scripts[1].contains(&format!("p={TMP_DIR}/aincient-x.tar.gz")));
        assert!(scripts[2].ends_with(&format!(
            "data restore --yes --other-site -- {TMP_DIR}/aincient-x.tar.gz"
        )));
        assert_eq!(scripts[3], format!("rm -rf -- {TMP_DIR}"));
    }

    #[test]
    fn a_damaged_upload_is_not_restored_and_still_cleaned_up() {
        let scratch = PrivateDir::new("atelier-upload").unwrap();
        let file = local_archive(scratch.path(), "b.tar.gz", "{}");
        let fake = with_cli(Fake::new())
            .on("mktemp -d", || out(0, &format!("{TMP_DIR}\n"), ""))
            .on("sha256sum", || {
                out(0, &format!("{} 3\n", "0".repeat(64)), "")
            })
            .on("data restore", || {
                panic!("must not restore a damaged upload")
            })
            .on("rm -rf --", || out(0, "", ""));
        let err = session(&fake)
            .restore_upload(&file, false, &mut |_, _| {})
            .unwrap_err();
        assert!(err.to_string().contains("arrived damaged"), "{err}");
        assert_eq!(last(&fake), format!("rm -rf -- {TMP_DIR}"));

        // A failing restore also cleans up, and keeps the server's exit code.
        let bytes = std::fs::read(&file).unwrap();
        let digest = format!("{} {}\n", sha_hex(&bytes), bytes.len());
        let fake = with_cli(Fake::new())
            .on("mktemp -d", || out(0, &format!("{TMP_DIR}\n"), ""))
            .on("sha256sum", move || out(0, &digest, ""))
            .on("data restore", || out(4, "", "error: refused\n"))
            .on("rm -rf --", || out(0, "", ""));
        let err = session(&fake)
            .restore_upload(&file, false, &mut |_, _| {})
            .unwrap_err();
        assert_eq!(err.downcast_ref::<RemoteExit>().map(|e| e.code), Some(4));
        assert_eq!(last(&fake), format!("rm -rf -- {TMP_DIR}"));
    }

    #[test]
    fn a_laptop_archive_from_another_site_is_refused_before_upload() {
        let scratch = PrivateDir::new("atelier-upload").unwrap();
        let theirs = local_archive(
            scratch.path(),
            "theirs.tar.gz",
            r#"{"site_id":"22222222-2222-4222-8222-222222222222","site_slug":"blog"}"#,
        );
        let ours = local_archive(
            scratch.path(),
            "ours.tar.gz",
            r#"{"site_id":"11111111-1111-4111-8111-111111111111"}"#,
        );
        let st = STATUS_JSON.replace(
            r#""version":"v0.20.0""#,
            r#""version":"v0.20.0","site_id":"11111111-1111-4111-8111-111111111111""#,
        );
        let fake = with_cli(Fake::new()).on("app status --json", move || out(0, &st, ""));
        let s = session(&fake);
        let err = s.check_restore_identity(&theirs, false).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("blog") && text.contains("`prod` on deploy@example.com"),
            "{text}"
        );
        assert!(matches!(
            s.check_restore_identity(&theirs, true).unwrap(),
            IdentityCheck::OtherAllowed(_)
        ));
        assert_eq!(
            s.check_restore_identity(&ours, false).unwrap(),
            IdentityCheck::Same
        );
        assert!(!fake.ran("mktemp"), "nothing uploaded");
    }
}
