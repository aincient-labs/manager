//! Location and on-disk layout of an Atelier appliance stack.
//!
//! The stack is a directory (default `~/.atelier`, override with `ATELIER_HOME`)
//! holding a `compose.yaml` + `.env` — the same pair `docker/install.sh` writes.
//! The manager owns this directory so the CLI and GUI converge on one source of truth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// The registry repository every published appliance image comes from.
pub const IMAGE_REPO: &str = "ghcr.io/aincient-labs/atelier-cms";
/// Default image tag, matching `docker/install.sh` — the [stable](Channel::Stable)
/// channel, i.e. released versions.
pub const DEFAULT_IMAGE: &str = "ghcr.io/aincient-labs/atelier-cms:latest";
/// What the default used to be, before there were tagged releases to point at.
///
/// An install still carrying this exact string was never *asked* which channel it
/// wanted — it got edge because edge was all there was. Those are the installs
/// [`Stack::migrate_default_channel`] moves onto stable, once.
pub const LEGACY_DEFAULT_IMAGE: &str = "ghcr.io/aincient-labs/atelier-cms:edge";
/// The registry repository the appliance was published from until 2026-07-18, when
/// the public artifact repo — and with it the GHCR package — was renamed `atelier`
/// → `atelier-cms`.
///
/// The old name does not merely point somewhere stale: it does not answer at all
/// (GHCR's token endpoint 403s `DENIED` for it anonymously), so every registry read
/// through it fails, permanently. Installs made by manager ≤ v0.2.0 or by the
/// pre-rename `install.sh` have it written into their `.env`, and nothing re-reads
/// [`DEFAULT_IMAGE`] — so the manager cannot grow out of this on its own; the
/// pointer on disk has to be rewritten. That is
/// [`Stack::migrate_image_repo`] (manager#1).
pub const LEGACY_IMAGE_REPO: &str = "ghcr.io/aincient-labs/atelier";
/// `.env` key recording the channel the operator **chose**, as opposed to the one
/// they happen to be on. See [`Stack::migrate_default_channel`] for why the
/// distinction has to be written down.
pub const CHANNEL_KEY: &str = "AINCIENT_CHANNEL";
/// Default console port — "AINCI" in leet (4=A,1=I,2=N,2=C,1=I).
pub const DEFAULT_PORT: u16 = 41221;

/// Which stream of appliance images an install follows.
///
/// The registry publishes two moving tags — `:latest`, retagged on every release,
/// and `:edge`, rebuilt on every merge to main — plus immutable `:vX.Y.Z` tags.
/// A channel is therefore just a policy about which tag `AINCIENT_IMAGE` names;
/// anything that isn't one of the two moving tags (a version tag, a digest, a
/// locally-built image) is [`Pinned`](Channel::Pinned): it doesn't move on its
/// own, so no update can arrive through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// `:latest` — released versions only. The default.
    Stable,
    /// `:edge` — every merge to main. Unreleased; may break.
    Edge,
    /// A specific version, digest, or a private image. Never moves.
    Pinned,
}

impl Channel {
    /// The channels a UI may OFFER. Only stable: `Edge` is still switchable by
    /// name on the CLI, but it is not something to drift onto from a menu, and
    /// `Pinned` is reached by naming an image rather than by choosing a channel.
    ///
    /// Edge is published (our own smoke/e2e lanes run on it) and is deliberately
    /// not deleted — it is *demoted*. It ships unreleased work, and until an edge
    /// build's stamp became version-comparable no upgrade floor could gate one at
    /// all, which is how a six-week-old edge install walked into the 2026-08-06
    /// data-loss incident. See `plans/channel-hardening.md`.
    pub const OFFERED: [Channel; 1] = [Channel::Stable];

    /// Whether this channel is one we support people following. False for `Edge`.
    pub fn is_supported(self) -> bool {
        !matches!(self, Channel::Edge)
    }

    /// The moving tag this channel follows, if it is one.
    pub fn tag(self) -> Option<&'static str> {
        match self {
            Channel::Stable => Some("latest"),
            Channel::Edge => Some("edge"),
            Channel::Pinned => None,
        }
    }

    /// The full image reference for this channel.
    pub fn image(self) -> Option<String> {
        self.tag().map(|tag| format!("{IMAGE_REPO}:{tag}"))
    }

    /// The machine name — what's written to `.env` and accepted on the CLI.
    pub fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Edge => "edge",
            Channel::Pinned => "pinned",
        }
    }

    /// One line of prose, for a status line or a GUI row.
    pub fn describe(self) -> &'static str {
        match self {
            Channel::Stable => "stable — released versions",
            Channel::Edge => "edge — every build off main, unreleased",
            Channel::Pinned => "pinned — this exact image, no updates",
        }
    }

    /// Parse an operator-supplied channel name. Generous about the synonyms
    /// people reach for (`latest`, `release`, `main`, `dev`) — the registry tag
    /// and the channel name aren't the same word, and confusing them shouldn't be
    /// an error.
    pub fn parse(s: &str) -> Option<Channel> {
        match s.trim().to_lowercase().as_str() {
            "stable" | "latest" | "release" | "releases" => Some(Channel::Stable),
            "edge" | "main" | "dev" | "nightly" => Some(Channel::Edge),
            _ => None,
        }
    }

    /// Classify an image reference. Only our own repository's moving tags count
    /// as channels — someone running a fork or a local build is pinned, whatever
    /// they called the tag.
    pub fn of_image(image: &str) -> Channel {
        let Some((repo, tag)) = image.rsplit_once(':') else {
            return Channel::Pinned;
        };
        // `repo@sha256:…` splits at the digest's colon, leaving the `@` behind.
        if repo != IMAGE_REPO {
            return Channel::Pinned;
        }
        match tag {
            "latest" => Channel::Stable,
            "edge" => Channel::Edge,
            _ => Channel::Pinned,
        }
    }
}

/// Rewrite a reference to the retired [`LEGACY_IMAGE_REPO`] onto [`IMAGE_REPO`],
/// keeping whatever it names there — tag or digest. `None` when the reference isn't
/// on the old repository, which includes every current one.
///
/// The boundary check is the point: [`IMAGE_REPO`] itself *starts with*
/// [`LEGACY_IMAGE_REPO`] (`…/atelier` is a prefix of `…/atelier-cms`), so a plain
/// prefix test would rename today's images into `…/atelier-cms-cms`. Only a `:` or
/// `@` — or nothing at all, an implicit `:latest` — may follow.
///
/// The tag survives on purpose: the rename moved the package, so `:v0.1.1` and any
/// digest published before it still resolve under the new name, and an operator who
/// pinned a version pinned a version, not a repository.
pub fn rename_legacy_repo(image: &str) -> Option<String> {
    let rest = image.strip_prefix(LEGACY_IMAGE_REPO)?;
    if !(rest.is_empty() || rest.starts_with(':') || rest.starts_with('@')) {
        return None;
    }
    Some(format!("{IMAGE_REPO}{rest}"))
}

/// The Compose stack written into the stack directory. Kept byte-for-byte in
/// step with the `cat > compose.yaml` heredoc in `docker/install.sh`: the slim
/// runtime topology (app + db, no build context, no updater sidecar).
pub const COMPOSE_TEMPLATE: &str = r#"name: atelier
services:
  db:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_DB: aincient
      POSTGRES_USER: aincient
      POSTGRES_PASSWORD: ${DB_PASSWORD:-aincient}
    volumes:
      - db-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U aincient -d aincient"]
      interval: 10s
      retries: 10
  app:
    image: ${AINCIENT_IMAGE:-ghcr.io/aincient-labs/atelier-cms:latest}
    depends_on:
      db:
        condition: service_healthy
    environment:
      DATABASE_URL: pgsql://aincient:${DB_PASSWORD:-aincient}@db/aincient
      HASH_SALT: ${HASH_SALT:?set HASH_SALT in .env}
      AINCIENT_TRUSTED_HOSTS: ${AINCIENT_TRUSTED_HOSTS:-}
      AINCIENT_ADMIN_PASS: ${ADMIN_PASS:-}
    ports:
      - "${HTTP_PORT:-41221}:80"
    volumes:
      - files:/opt/drupal/web/sites/default/files
      - private:/opt/drupal/private
    restart: unless-stopped
volumes:
  db-data:
  files:
  private:
"#;

/// A located appliance stack directory.
#[derive(Debug, Clone)]
pub struct Stack {
    pub home: PathBuf,
}

/// Options for scaffolding a fresh stack.
#[derive(Debug, Default, Clone)]
pub struct InstallOptions {
    /// Override image tag (defaults to [`DEFAULT_IMAGE`]).
    pub image: Option<String>,
    /// Override console port (defaults to [`DEFAULT_PORT`]).
    pub http_port: Option<u16>,
}

impl Stack {
    /// Resolve the stack directory: `ATELIER_HOME` if set, else `~/.atelier`.
    pub fn locate() -> Result<Self> {
        let home = match std::env::var_os("ATELIER_HOME") {
            Some(p) => PathBuf::from(p),
            None => dirs::home_dir()
                .context("could not determine your home directory")?
                .join(".atelier"),
        };
        Ok(Self { home })
    }

    pub fn compose_path(&self) -> PathBuf {
        self.home.join("compose.yaml")
    }

    /// The Docker Compose **project name** for this stack — the namespace its
    /// containers, volumes and network live under.
    ///
    /// Derived from the stack directory so that a custom `ATELIER_HOME` gets a
    /// genuinely independent set of containers/volumes (compose otherwise keys
    /// everything off the project name, and the on-disk `compose.yaml` pins a
    /// literal `name: atelier` — so without an explicit `-p` two homes would
    /// silently share one appliance). The canonical `~/.atelier` sanitizes to
    /// `atelier`, matching the template's `name:`, so existing installs keep the
    /// exact same containers and volumes — this is backward compatible.
    pub fn project_name(&self) -> String {
        let base = self
            .home
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("atelier");
        // Compose project names allow only [a-z0-9_-] and must start with an
        // alphanumeric. Lowercase, map anything else to '-', then trim leading
        // separators (so `.atelier` → `atelier`).
        let mapped: String = base
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        if trimmed.is_empty() {
            "atelier".to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn env_path(&self) -> PathBuf {
        self.home.join(".env")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.home.join("backups")
    }

    /// True once a `compose.yaml` exists — i.e. the stack has been laid down.
    pub fn exists(&self) -> bool {
        self.compose_path().is_file()
    }

    /// Parse the `.env` into key/value pairs. Missing file → empty map.
    pub fn read_env(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let Ok(text) = std::fs::read_to_string(self.env_path()) else {
            return map;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        map
    }

    pub fn env_get(&self, key: &str) -> Option<String> {
        self.read_env().get(key).cloned().filter(|v| !v.is_empty())
    }

    pub fn http_port(&self) -> u16 {
        self.env_get("HTTP_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PORT)
    }

    /// URL of the Atelier **console** (the chat workspace at `/atelier`), not the
    /// bare site root — this is what "open console" should land on.
    pub fn console_url(&self) -> String {
        format!("http://localhost:{}/atelier", self.http_port())
    }

    /// URL of Drupal's login form. "Login" sends the operator straight here to
    /// sign in themselves — the manager never displays the admin password.
    pub fn login_url(&self) -> String {
        format!("http://localhost:{}/user/login", self.http_port())
    }

    /// URL of the public site root — the front page a visitor sees, as opposed
    /// to the console at `/atelier`. Anonymous-viewable, so "View my site" lands
    /// here with no login wall.
    pub fn site_url(&self) -> String {
        format!("http://localhost:{}", self.http_port())
    }

    pub fn image(&self) -> String {
        self.env_get("AINCIENT_IMAGE")
            .unwrap_or_else(|| DEFAULT_IMAGE.to_string())
    }

    /// Which channel this install is actually on — derived from the image, which
    /// is the only thing Docker obeys. [`CHANNEL_KEY`] is never consulted here:
    /// it records a *choice*, and a hand-edited `AINCIENT_IMAGE` must win over a
    /// stale marker rather than be contradicted by it.
    pub fn channel(&self) -> Channel {
        Channel::of_image(&self.image())
    }

    /// The channel the operator explicitly chose, if they ever did — `None` on an
    /// install that predates channels or was never switched.
    pub fn chosen_channel(&self) -> Option<Channel> {
        self.env_get(CHANNEL_KEY).and_then(|v| Channel::parse(&v))
    }

    /// Point the stack at a channel's tag and record the choice. Returns the image
    /// now configured.
    ///
    /// Writing `.env` is the whole of it — the image only actually changes on the
    /// next pull, which is why callers follow this with an update rather than
    /// claiming the switch already happened.
    pub fn set_channel(&self, channel: Channel) -> Result<String> {
        let image = channel
            .image()
            .context("`pinned` isn't a channel you can switch to — name an image instead")?;
        let mut env = self.read_env();
        env.insert("AINCIENT_IMAGE".to_string(), image.clone());
        env.insert(CHANNEL_KEY.to_string(), channel.name().to_string());
        self.write_env(&env)?;
        Ok(image)
    }

    /// Point the stack at one exact image, WITHOUT recording a channel choice.
    ///
    /// This is how a stepped upgrade walks its waypoints: each hop runs a specific
    /// `:vX.Y.Z` tag, which by [`Channel::of_image`] reads as `pinned`. Writing
    /// [`CHANNEL_KEY`] here would turn a transient hop into a permanent decision —
    /// the operator would come out of a routine update pinned to a version tag,
    /// receiving no further releases, because a step on the way was mistaken for a
    /// destination. The chosen channel is a fact about the operator, not about
    /// which image happens to be running mid-route.
    pub fn set_image(&self, image: &str) -> Result<()> {
        let mut env = self.read_env();
        env.insert("AINCIENT_IMAGE".to_string(), image.to_string());
        self.write_env(&env)
    }

    /// Move an install that never chose a channel off the old `:edge` default and
    /// onto stable, once. Returns `Some((from, to))` if it moved anything.
    ///
    /// Before there were releases to point at, `:edge` *was* the default, so every
    /// install made in that era is following unreleased builds without ever having
    /// asked to. Flipping [`DEFAULT_IMAGE`] alone wouldn't reach them: the tag was
    /// baked into their `.env` at install time and nothing re-reads the default.
    ///
    /// The guard is [`CHANNEL_KEY`] plus an exact match on [`LEGACY_DEFAULT_IMAGE`]:
    /// a marker means the operator picked this channel and must be left alone, and
    /// any other image (a version tag, a fork, a local build) is a decision too.
    /// The migration writes the marker itself, so it can only ever fire once and a
    /// deliberate move back to edge afterwards sticks.
    pub fn migrate_default_channel(&self) -> Result<Option<(String, String)>> {
        if !self.pending_default_channel_migration() {
            return Ok(None);
        }
        let to = self.set_channel(Channel::Stable)?;
        Ok(Some((LEGACY_DEFAULT_IMAGE.to_string(), to)))
    }

    /// Move an install off the retired [`LEGACY_IMAGE_REPO`] and onto [`IMAGE_REPO`],
    /// keeping its tag. Returns `Some((from, to))` if it rewrote anything.
    ///
    /// Unlike [`migrate_default_channel`](Self::migrate_default_channel) this is
    /// **not** guarded by [`CHANNEL_KEY`], and that asymmetry is the whole design: a
    /// channel is a preference, so a recorded choice must be respected, but a
    /// repository that no longer answers is a dead pointer — there is no intent in it
    /// worth preserving, and leaving it in place only preserves the failure. It
    /// deliberately writes through [`set_image`](Self::set_image) rather than
    /// `set_channel`, because renaming a repository decides nothing about which
    /// channel the operator follows.
    ///
    /// Idempotent by construction: once rewritten the reference no longer matches, so
    /// this can only fire once per install.
    pub fn migrate_image_repo(&self) -> Result<Option<(String, String)>> {
        let Some((from, to)) = self.pending_image_repo_migration() else {
            return Ok(None);
        };
        self.set_image(&to)?;
        Ok(Some((from, to)))
    }

    /// Whether [`migrate_image_repo`](Self::migrate_image_repo) would rewrite this
    /// install, and to what — asked separately for the same reason as the channel
    /// migration's counterpart.
    pub fn pending_image_repo_migration(&self) -> Option<(String, String)> {
        if !self.exists() {
            return None;
        }
        let from = self.env_get("AINCIENT_IMAGE")?;
        rename_legacy_repo(&from).map(|to| (from, to))
    }

    /// The image this install will be configured to run once the legacy repairs have
    /// been applied — what the *registry* has to be asked about, as opposed to
    /// [`image`](Self::image), which is what Docker is running right now.
    ///
    /// The two diverge on exactly the installs those repairs exist for, and a caller
    /// that conflates them gets a wrong answer rather than a failed one: probing the
    /// registry for the recorded image asks a name that no longer exists, while
    /// probing the local daemon for the prospective one asks for an image that was
    /// never pulled ("install Atelier first" — to someone whose site is running).
    ///
    /// Composed in the order the repairs run, because the second reads the first's
    /// output: a pre-rename `…/atelier:edge` becomes [`LEGACY_DEFAULT_IMAGE`], which
    /// is then the very string the channel migration moves onto stable.
    pub fn prospective_image(&self) -> String {
        let image = self.image();
        let image = rename_legacy_repo(&image).unwrap_or(image);
        if self.chosen_channel().is_none() && image == LEGACY_DEFAULT_IMAGE {
            if let Some(stable) = Channel::Stable.image() {
                return stable;
            }
        }
        image
    }

    /// Whether [`migrate_default_channel`](Self::migrate_default_channel) would
    /// move this install — asked separately so a caller can take precautions (a
    /// snapshot, a warning) *before* the `.env` changes under it.
    pub fn pending_default_channel_migration(&self) -> bool {
        self.exists()
            && self.chosen_channel().is_none()
            && self.env_get("AINCIENT_IMAGE").as_deref() == Some(LEGACY_DEFAULT_IMAGE)
    }

    /// Serialize the env map back to `.env`, keeping its `0600` perms.
    fn write_env(&self, env: &BTreeMap<String, String>) -> Result<()> {
        let body: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        write_private(&self.env_path(), &body)
    }

    /// Lay down `compose.yaml` + `.env` if absent. Never clobbers an existing
    /// `.env` (preserves `HASH_SALT`); reconciles the image and port tunables on
    /// a re-run, mirroring `install.sh`.
    pub fn ensure_scaffold(&self, opts: &InstallOptions) -> Result<()> {
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("could not create stack directory {}", self.home.display()))?;

        let compose = self.compose_path();
        if !compose.is_file() {
            std::fs::write(&compose, COMPOSE_TEMPLATE)
                .with_context(|| format!("could not write {}", compose.display()))?;
        }

        let env_path = self.env_path();

        if !env_path.is_file() {
            let image = opts.image.clone().unwrap_or_else(|| DEFAULT_IMAGE.to_string());
            let port = opts.http_port.unwrap_or(DEFAULT_PORT);
            // A fresh install has chosen its channel by definition — whatever the
            // image it was pointed at implies. Recording it here is what keeps the
            // legacy-edge migration from ever looking at this install again.
            let contents = format!(
                "HASH_SALT={salt}\n\
                 AINCIENT_IMAGE={image}\n\
                 AINCIENT_CHANNEL={channel}\n\
                 HTTP_PORT={port}\n\
                 ADMIN_PASS=\n",
                salt = hash_salt(),
                channel = Channel::of_image(&image).name(),
            );
            write_private(&env_path, &contents)?;
        } else {
            // Reconcile only the tunables the caller explicitly chose this run;
            // keep secrets AND any previously-chosen port/image on disk. Reinstall
            // (and the GUI) pass `InstallOptions::default()`, so overriding
            // unconditionally here would silently reset a custom port or image to
            // the defaults on every re-run. Fall back to the defaults only when a
            // value is genuinely absent.
            let mut env = self.read_env();
            match &opts.image {
                Some(image) => {
                    env.insert("AINCIENT_IMAGE".to_string(), image.clone());
                    // Naming an image is choosing a channel (often `pinned`), so
                    // the marker moves with it — otherwise the legacy-edge
                    // migration could later "correct" a deliberate `--image
                    // …:edge` back to stable.
                    env.insert(
                        CHANNEL_KEY.to_string(),
                        Channel::of_image(image).name().to_string(),
                    );
                }
                None => {
                    env.entry("AINCIENT_IMAGE".to_string())
                        .or_insert_with(|| DEFAULT_IMAGE.to_string());
                }
            }
            match opts.http_port {
                Some(port) => {
                    env.insert("HTTP_PORT".to_string(), port.to_string());
                }
                None => {
                    env.entry("HTTP_PORT".to_string())
                        .or_insert_with(|| DEFAULT_PORT.to_string());
                }
            }
            self.write_env(&env)?;
        }
        Ok(())
    }

    /// Repair the stack's own files — what `doctor --fix` calls when
    /// `compose.yaml` is missing/unparseable or `.env` lost its `HASH_SALT`.
    ///
    /// Distinct from [`ensure_scaffold`](Self::ensure_scaffold), which is
    /// deliberately non-destructive: it never overwrites an existing
    /// `compose.yaml` and never adds keys to an existing `.env`. Those are the
    /// right semantics for install (don't clobber a user's edits) and exactly
    /// the wrong ones for repair, where the file is already broken.
    ///
    /// A replaced `compose.yaml` is moved aside rather than deleted — if the
    /// operator had hand-edited it (a custom port mapping, an extra volume), the
    /// repair must not silently eat that. **Data volumes are never touched**:
    /// they're named in the template identically, so the rewritten file
    /// re-adopts the same `db-data`/`files`/`private` volumes.
    ///
    /// Returns a line per change, for the report.
    pub fn repair_scaffold(&self) -> Result<Vec<String>> {
        let mut changes = Vec::new();
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("could not create stack directory {}", self.home.display()))?;

        let compose = self.compose_path();
        if compose.is_file() {
            let current = std::fs::read_to_string(&compose).unwrap_or_default();
            if current != COMPOSE_TEMPLATE {
                let aside = self.home.join(format!(
                    "compose.yaml.replaced-{}",
                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                ));
                std::fs::rename(&compose, &aside).with_context(|| {
                    format!("could not move the old compose.yaml to {}", aside.display())
                })?;
                std::fs::write(&compose, COMPOSE_TEMPLATE)
                    .with_context(|| format!("could not write {}", compose.display()))?;
                changes.push(format!(
                    "rewrote compose.yaml (previous kept as {})",
                    aside.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        } else {
            std::fs::write(&compose, COMPOSE_TEMPLATE)
                .with_context(|| format!("could not write {}", compose.display()))?;
            changes.push("wrote a fresh compose.yaml".to_string());
        }

        // Fill in only what's missing. An existing salt is never regenerated —
        // rotating it would invalidate every session and every one-time login
        // link for no reason.
        let mut env = self.read_env();
        if env.get("HASH_SALT").map(|s| s.len() < 32).unwrap_or(true) {
            env.insert("HASH_SALT".to_string(), hash_salt());
            changes.push("generated a new HASH_SALT".to_string());
        }
        match env.get("AINCIENT_IMAGE").and_then(|i| rename_legacy_repo(i)) {
            // A reference to the retired repository is a broken stack file in the same
            // sense as a missing key: it names somewhere that cannot answer. Repaired
            // here as well as in `migrate_image_repo` so a checkup fixes it without
            // requiring the operator to attempt an update first.
            Some(fixed) => {
                changes.push(format!("repointed the image at {fixed} (the old name is retired)"));
                env.insert("AINCIENT_IMAGE".to_string(), fixed);
            }
            None if !env.contains_key("AINCIENT_IMAGE") => {
                env.insert("AINCIENT_IMAGE".to_string(), DEFAULT_IMAGE.to_string());
                changes.push("restored the image setting".to_string());
            }
            None => {}
        }
        if !env.contains_key("HTTP_PORT") {
            env.insert("HTTP_PORT".to_string(), DEFAULT_PORT.to_string());
            changes.push("restored the port setting".to_string());
        }
        if !changes.is_empty() {
            self.write_env(&env)?;
        }
        Ok(changes)
    }
}

/// Write a file containing secrets with `0600` perms where the platform supports it.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 32 random bytes, hex-encoded — a Drupal `HASH_SALT` (matches `openssl rand -hex 32`).
fn hash_salt() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory per test, cleaned up on drop.
    struct TempStack(Stack);
    impl TempStack {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "atelier-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            TempStack(Stack { home: dir })
        }
    }
    impl Drop for TempStack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.home);
        }
    }

    #[test]
    fn scaffold_writes_compose_and_env() {
        let ts = TempStack::new();
        let stack = &ts.0;
        assert!(!stack.exists());

        stack.ensure_scaffold(&InstallOptions::default()).unwrap();

        assert!(stack.exists());
        assert_eq!(std::fs::read_to_string(stack.compose_path()).unwrap(), COMPOSE_TEMPLATE);

        let env = stack.read_env();
        assert_eq!(env.get("HASH_SALT").unwrap().len(), 64);
        assert!(env.get("HASH_SALT").unwrap().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(env.get("AINCIENT_IMAGE").map(String::as_str), Some(DEFAULT_IMAGE));
        assert_eq!(env.get("HTTP_PORT").map(String::as_str), Some("41221"));
        // No AI key is written — a provider is connected via in-app onboarding.
        assert_eq!(env.get("AINCIENT_AI_KEY"), None);
    }

    #[test]
    fn scaffold_records_provided_port() {
        let ts = TempStack::new();
        let opts = InstallOptions {
            image: None,
            http_port: Some(8080),
        };
        ts.0.ensure_scaffold(&opts).unwrap();

        assert_eq!(ts.0.http_port(), 8080);
        assert_eq!(ts.0.console_url(), "http://localhost:8080/atelier");
        assert_eq!(ts.0.site_url(), "http://localhost:8080");
    }

    #[test]
    fn re_scaffold_preserves_secrets_but_reconciles_tunables() {
        let ts = TempStack::new();
        let stack = &ts.0;
        ts.0.ensure_scaffold(&InstallOptions::default()).unwrap();
        let salt = stack.env_get("HASH_SALT").unwrap();

        // Re-run pointing at a new image + port.
        stack
            .ensure_scaffold(&InstallOptions {
                image: Some("ghcr.io/aincient-labs/atelier-cms:v2".into()),
                http_port: Some(9000),
            })
            .unwrap();

        assert_eq!(stack.env_get("HASH_SALT"), Some(salt), "salt must be preserved");
        assert_eq!(stack.image(), "ghcr.io/aincient-labs/atelier-cms:v2");
        assert_eq!(stack.http_port(), 9000);
    }

    #[test]
    fn re_scaffold_with_defaults_preserves_existing_tunables() {
        // A reinstall passes `InstallOptions::default()`. It must NOT reset a
        // custom port/image back to the defaults — the whole appliance would move
        // ports out from under the user.
        let ts = TempStack::new();
        let stack = &ts.0;
        stack
            .ensure_scaffold(&InstallOptions {
                image: Some("ghcr.io/aincient-labs/atelier-cms:pinned".into()),
                http_port: Some(51000),
            })
            .unwrap();

        // Re-run exactly as reinstall does — no overrides.
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();

        assert_eq!(
            stack.image(),
            "ghcr.io/aincient-labs/atelier-cms:pinned",
            "a default re-run must keep the previously-chosen image"
        );
        assert_eq!(
            stack.http_port(),
            51000,
            "a default re-run must keep the previously-chosen port"
        );
    }

    #[test]
    fn project_name_defaults_to_atelier_for_canonical_home() {
        // ~/.atelier must sanitize to `atelier`, matching the template's `name:`,
        // so existing installs keep the same containers/volumes.
        let stack = Stack {
            home: PathBuf::from("/Users/someone/.atelier"),
        };
        assert_eq!(stack.project_name(), "atelier");
    }

    #[test]
    fn project_name_is_distinct_for_custom_home() {
        let stack = Stack {
            home: PathBuf::from("/tmp/atelier-staging"),
        };
        assert_eq!(stack.project_name(), "atelier-staging");

        // Sanitizes odd characters and leading separators.
        let odd = Stack {
            home: PathBuf::from("/tmp/.My Stack!"),
        };
        assert_eq!(odd.project_name(), "my-stack-");
    }

    #[test]
    fn repair_rewrites_a_broken_compose_but_keeps_the_original() {
        let ts = TempStack::new();
        let stack = &ts.0;
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();
        let salt = stack.env_get("HASH_SALT").unwrap();
        std::fs::write(stack.compose_path(), "services:\n  app:\n    image: [[[broken\n").unwrap();

        let changes = stack.repair_scaffold().unwrap();

        assert_eq!(
            std::fs::read_to_string(stack.compose_path()).unwrap(),
            COMPOSE_TEMPLATE,
            "the broken compose.yaml must be replaced by the template"
        );
        assert_eq!(
            stack.env_get("HASH_SALT"),
            Some(salt),
            "a healthy salt must never be rotated — it would drop every session"
        );
        assert!(changes.iter().any(|c| c.contains("rewrote compose.yaml")));
        // The operator's file is moved aside, not deleted — they may have
        // hand-edited it.
        let kept: Vec<_> = std::fs::read_dir(&stack.home)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("compose.yaml.replaced-"))
            .collect();
        assert_eq!(kept.len(), 1, "the previous compose.yaml is preserved");
    }

    #[test]
    fn repair_restores_a_missing_hash_salt_without_touching_other_keys() {
        let ts = TempStack::new();
        let stack = &ts.0;
        stack
            .ensure_scaffold(&InstallOptions {
                image: Some("ghcr.io/aincient-labs/atelier-cms:pinned".into()),
                http_port: Some(51000),
            })
            .unwrap();
        // Drop the salt, exactly as a truncated/hand-edited .env would.
        std::fs::write(
            stack.env_path(),
            "AINCIENT_IMAGE=ghcr.io/aincient-labs/atelier-cms:pinned\nHTTP_PORT=51000\n",
        )
        .unwrap();

        let changes = stack.repair_scaffold().unwrap();

        let new_salt = stack.env_get("HASH_SALT").expect("a salt is written back");
        assert_eq!(new_salt.len(), 64);
        assert!(new_salt.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(changes.iter().any(|c| c.contains("HASH_SALT")));
        // The repair must not reset the operator's chosen image/port.
        assert_eq!(stack.image(), "ghcr.io/aincient-labs/atelier-cms:pinned");
        assert_eq!(stack.http_port(), 51000);
    }

    #[test]
    fn repair_is_a_no_op_on_a_healthy_stack() {
        let ts = TempStack::new();
        let stack = &ts.0;
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();
        let before = std::fs::read_to_string(stack.env_path()).unwrap();

        let changes = stack.repair_scaffold().unwrap();

        assert!(changes.is_empty(), "nothing to repair: {changes:?}");
        assert_eq!(std::fs::read_to_string(stack.env_path()).unwrap(), before);
        // And no stray backup file was created.
        let strays = std::fs::read_dir(&stack.home)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("replaced-"))
            .count();
        assert_eq!(strays, 0);
    }

    #[test]
    fn repair_builds_a_stack_from_nothing() {
        let ts = TempStack::new();
        let stack = &ts.0;
        assert!(!stack.exists());

        stack.repair_scaffold().unwrap();

        assert!(stack.exists());
        assert_eq!(stack.env_get("HASH_SALT").unwrap().len(), 64);
        assert_eq!(stack.image(), DEFAULT_IMAGE);
        assert_eq!(stack.http_port(), DEFAULT_PORT);
    }

    #[test]
    fn a_fresh_install_is_on_the_stable_channel() {
        let ts = TempStack::new();
        ts.0.ensure_scaffold(&InstallOptions::default()).unwrap();

        assert_eq!(ts.0.image(), "ghcr.io/aincient-labs/atelier-cms:latest");
        assert_eq!(ts.0.channel(), Channel::Stable);
        // The marker is written on install, so this stack can never be mistaken
        // for one that never chose.
        assert_eq!(ts.0.chosen_channel(), Some(Channel::Stable));
    }

    #[test]
    fn channels_are_classified_from_the_image() {
        assert_eq!(
            Channel::of_image("ghcr.io/aincient-labs/atelier-cms:latest"),
            Channel::Stable
        );
        assert_eq!(
            Channel::of_image("ghcr.io/aincient-labs/atelier-cms:edge"),
            Channel::Edge
        );
        // A version tag doesn't move, so it isn't a channel.
        assert_eq!(
            Channel::of_image("ghcr.io/aincient-labs/atelier-cms:v0.1.2"),
            Channel::Pinned
        );
        assert_eq!(
            Channel::of_image("ghcr.io/aincient-labs/atelier-cms@sha256:abc123"),
            Channel::Pinned
        );
        // Someone else's `:latest` is not our stable channel.
        assert_eq!(Channel::of_image("example.com/fork/atelier-cms:latest"), Channel::Pinned);
        assert_eq!(Channel::of_image("atelier-cms-local"), Channel::Pinned);
    }

    #[test]
    fn set_channel_rewrites_the_image_and_records_the_choice() {
        let ts = TempStack::new();
        let stack = &ts.0;
        stack.ensure_scaffold(&InstallOptions::default()).unwrap();
        let salt = stack.env_get("HASH_SALT").unwrap();

        let image = stack.set_channel(Channel::Edge).unwrap();

        assert_eq!(image, "ghcr.io/aincient-labs/atelier-cms:edge");
        assert_eq!(stack.image(), image);
        assert_eq!(stack.channel(), Channel::Edge);
        assert_eq!(stack.chosen_channel(), Some(Channel::Edge));
        assert_eq!(stack.env_get("HASH_SALT"), Some(salt), "salt must survive a switch");
    }

    #[test]
    fn a_legacy_edge_install_is_moved_to_stable_once() {
        // Exactly what the pre-channels installer wrote: the old default image,
        // and no channel marker at all.
        let ts = legacy_stack(LEGACY_DEFAULT_IMAGE);
        let stack = &ts.0;

        let moved = stack.migrate_default_channel().unwrap();

        assert_eq!(
            moved,
            Some((LEGACY_DEFAULT_IMAGE.to_string(), DEFAULT_IMAGE.to_string()))
        );
        assert_eq!(stack.channel(), Channel::Stable);

        // Once only: a deliberate move back to edge afterwards must stick, even
        // though the image is once again the legacy string.
        stack.set_channel(Channel::Edge).unwrap();
        assert_eq!(stack.migrate_default_channel().unwrap(), None);
        assert_eq!(stack.channel(), Channel::Edge);
    }

    /// Write exactly what a pre-channels installer left behind: an image string and
    /// no channel marker at all.
    fn legacy_stack(image: &str) -> TempStack {
        let ts = TempStack::new();
        std::fs::create_dir_all(&ts.0.home).unwrap();
        std::fs::write(ts.0.compose_path(), COMPOSE_TEMPLATE).unwrap();
        std::fs::write(
            ts.0.env_path(),
            format!("HASH_SALT={}\nAINCIENT_IMAGE={image}\nHTTP_PORT=41221\n", "a".repeat(64)),
        )
        .unwrap();
        ts
    }

    #[test]
    fn the_retired_repository_is_renamed_without_eating_the_current_one() {
        // The trap: `…/atelier` is a prefix of `…/atelier-cms`, so the current
        // repository must not be renamed into `…/atelier-cms-cms`.
        assert_eq!(rename_legacy_repo(DEFAULT_IMAGE), None);
        assert_eq!(rename_legacy_repo(LEGACY_DEFAULT_IMAGE), None);
        assert_eq!(rename_legacy_repo("ghcr.io/aincient-labs/atelier-cms@sha256:abc"), None);

        assert_eq!(
            rename_legacy_repo("ghcr.io/aincient-labs/atelier:edge").as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms:edge")
        );
        // A pinned tag and a digest keep what they named — the package moved, so
        // both still resolve under the new name.
        assert_eq!(
            rename_legacy_repo("ghcr.io/aincient-labs/atelier:v0.1.1").as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms:v0.1.1")
        );
        assert_eq!(
            rename_legacy_repo("ghcr.io/aincient-labs/atelier@sha256:abc").as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms@sha256:abc")
        );
        // Bare, i.e. an implicit `:latest`.
        assert_eq!(
            rename_legacy_repo("ghcr.io/aincient-labs/atelier").as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms")
        );
        // Someone else's repository of the same name is not ours to rewrite.
        assert_eq!(rename_legacy_repo("example.com/aincient-labs/atelier:edge"), None);
    }

    #[test]
    fn a_pre_rename_install_is_repaired_then_moved_to_stable() {
        // manager#1: installed by manager ≤ v0.2.0, so it carries the retired
        // repository name *and* predates channels.
        let ts = legacy_stack("ghcr.io/aincient-labs/atelier:edge");
        let stack = &ts.0;

        // Before anything: the repo is the pending repair, and the channel migration
        // cannot see this install at all — it matches on the exact legacy default.
        assert!(!stack.pending_default_channel_migration());
        assert_eq!(
            stack.pending_image_repo_migration(),
            Some((
                "ghcr.io/aincient-labs/atelier:edge".to_string(),
                LEGACY_DEFAULT_IMAGE.to_string()
            ))
        );
        // And the route is planned against where the repairs land, not `.env`.
        assert_eq!(stack.prospective_image(), DEFAULT_IMAGE);

        let renamed = stack.migrate_image_repo().unwrap();
        assert_eq!(
            renamed,
            Some((
                "ghcr.io/aincient-labs/atelier:edge".to_string(),
                LEGACY_DEFAULT_IMAGE.to_string()
            ))
        );
        // Renaming a repository decides nothing about channels, so no marker yet…
        assert_eq!(stack.chosen_channel(), None);
        // …which is precisely what lets the channel migration now recognise it.
        assert!(stack.pending_default_channel_migration());
        assert_eq!(
            stack.migrate_default_channel().unwrap(),
            Some((LEGACY_DEFAULT_IMAGE.to_string(), DEFAULT_IMAGE.to_string()))
        );
        assert_eq!(stack.image(), DEFAULT_IMAGE);
        assert_eq!(stack.chosen_channel(), Some(Channel::Stable));

        // Once only.
        assert_eq!(stack.migrate_image_repo().unwrap(), None);
        assert_eq!(stack.migrate_default_channel().unwrap(), None);
    }

    #[test]
    fn the_rename_fixes_the_repository_but_respects_a_chosen_channel() {
        let ts = legacy_stack("ghcr.io/aincient-labs/atelier:edge");
        let stack = &ts.0;
        // An operator who asked for edge keeps edge — on the repository that answers.
        stack.set_channel(Channel::Edge).unwrap();
        stack.set_image("ghcr.io/aincient-labs/atelier:edge").unwrap();

        assert_eq!(stack.prospective_image(), LEGACY_DEFAULT_IMAGE);
        assert_eq!(
            stack.migrate_image_repo().unwrap().map(|(_, to)| to),
            Some(LEGACY_DEFAULT_IMAGE.to_string())
        );
        assert_eq!(stack.chosen_channel(), Some(Channel::Edge));
        assert_eq!(stack.migrate_default_channel().unwrap(), None, "the choice stands");
    }

    #[test]
    fn a_pinned_legacy_version_keeps_its_version() {
        let ts = legacy_stack("ghcr.io/aincient-labs/atelier:v0.1.1");
        let stack = &ts.0;

        assert_eq!(stack.prospective_image(), "ghcr.io/aincient-labs/atelier-cms:v0.1.1");
        stack.migrate_image_repo().unwrap();
        assert_eq!(stack.image(), "ghcr.io/aincient-labs/atelier-cms:v0.1.1");
        // A pinned install was never a candidate for the channel move.
        assert_eq!(stack.migrate_default_channel().unwrap(), None);
    }

    #[test]
    fn a_current_install_has_nothing_to_repair() {
        let ts = TempStack::new();
        ts.0.ensure_scaffold(&InstallOptions::default()).unwrap();

        assert_eq!(ts.0.pending_image_repo_migration(), None);
        assert_eq!(ts.0.migrate_image_repo().unwrap(), None);
        assert_eq!(ts.0.prospective_image(), DEFAULT_IMAGE);
        assert_eq!(ts.0.image(), DEFAULT_IMAGE);

        // And with no stack at all there is nothing to repair either.
        let empty = TempStack::new();
        assert_eq!(empty.0.pending_image_repo_migration(), None);
        assert_eq!(empty.0.migrate_image_repo().unwrap(), None);
    }

    #[test]
    fn migration_leaves_pinned_and_chosen_installs_alone() {
        // A pinned version tag is a decision, not a leftover default.
        let pinned = TempStack::new();
        pinned
            .0
            .ensure_scaffold(&InstallOptions {
                image: Some("ghcr.io/aincient-labs/atelier-cms:v0.1.1".into()),
                http_port: None,
            })
            .unwrap();
        assert_eq!(pinned.0.migrate_default_channel().unwrap(), None);
        assert_eq!(pinned.0.image(), "ghcr.io/aincient-labs/atelier-cms:v0.1.1");

        // `--image …:edge` records the choice, so the migration must not undo it.
        let chose_edge = TempStack::new();
        chose_edge
            .0
            .ensure_scaffold(&InstallOptions {
                image: Some(LEGACY_DEFAULT_IMAGE.into()),
                http_port: None,
            })
            .unwrap();
        assert_eq!(chose_edge.0.chosen_channel(), Some(Channel::Edge));
        assert_eq!(chose_edge.0.migrate_default_channel().unwrap(), None);
        assert_eq!(chose_edge.0.channel(), Channel::Edge);

        // And with no stack at all there is nothing to migrate.
        let empty = TempStack::new();
        assert_eq!(empty.0.migrate_default_channel().unwrap(), None);
    }

    #[test]
    fn defaults_apply_without_a_stack() {
        let ts = TempStack::new();
        assert_eq!(ts.0.image(), DEFAULT_IMAGE);
        assert_eq!(ts.0.http_port(), DEFAULT_PORT);
        assert!(ts.0.read_env().is_empty());
    }
}
