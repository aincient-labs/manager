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
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::docker::{output_within, output_within_input, RunError};
use crate::ops::Status;
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
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    // %C expands to a 40-hex-char hash of the connection.
    (dir.as_os_str().len() + 1 + 40 < 100).then(|| dir.join("%C"))
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
}

impl EngineOptions {
    /// Defaults plus `ATELIER_REMOTE_BINARY`.
    pub fn from_env() -> Self {
        EngineOptions {
            binary: std::env::var_os(BINARY_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            replace_older: false,
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
        let engine = match (usable, probe.clis.first()) {
            (Some(cli), _) => cli.path.clone(),
            (None, Some(old)) if !opts.replace_older => {
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
            _ => install_engine(&transport, &probe, opts, wanted)?,
        };
        Ok(Session {
            transport,
            remote_slug: remote_slug.to_string(),
            engine,
        })
    }

    /// The `atelier` path this session drives on the server.
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// Run `atelier --site <remote slug> <args…>` on the server. A non-zero
    /// exit is an error naming the host and carrying the CLI's own message.
    pub fn run(&self, args: &[&str], deadline: Duration) -> Result<Output> {
        let mut words = vec![
            sh_quote(&self.engine),
            "--site".into(),
            sh_quote(&self.remote_slug),
        ];
        words.extend(args.iter().map(|a| sh_quote(a)));
        let what = format!("`atelier {}`", args.join(" "));
        checked(
            &self.transport,
            &what,
            self.transport.exec(&words.join(" "), None, deadline),
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
        .filter(|l| !l.is_empty())
        .last()
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
        rules: Vec<(&'static str, Box<dyn Fn() -> Reply>)>,
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
            replace_older: false,
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
            replace_older: false,
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
}
