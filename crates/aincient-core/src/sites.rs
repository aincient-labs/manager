//! The multi-site registry: `~/.atelier/sites.toml` (DECISIONS 0393).
//!
//! Per-site isolation already works — one [`Stack`] directory is one appliance
//! with its own Compose project, volumes, port and backups. What this module adds
//! is a written-down answer to "which site?", so it no longer lives only in a
//! process environment variable:
//!
//! ```toml
//! active = "default"
//!
//! [[site]]
//! slug = "default"
//! label = "My Site"
//! home = "~/.atelier"
//! project = "atelier"        # PINNED, not derived — survives a relabel
//!
//! [[site]]
//! slug = "blog"
//! label = "Blog"
//! home = "~/.atelier/sites/blog"
//! project = "atelier-blog"
//!
//! [[site]]                   # a REMOTE site (M2, D-06): no home, no project
//! slug = "prod"
//! label = "Production"
//! host = "deploy@example.com"   # an ssh target or ~/.ssh/config alias
//! remote_site = "default"       # its slug on the server
//! key = "guided"                # omitted = the user's own ssh setup (D-05)
//! domain = "cms.example.com"    # optional: where its console is public (D-02)
//! ```
//!
//! A remote site is driven through the server's own `atelier` CLI over SSH
//! (D-01, see [`crate::remote`]). It has no stack directory here and claims no
//! port; every home/project/port rule below applies to local sites only.
//!
//! **No code path here moves, renames or deletes a stack directory.** A stack's
//! Compose project is derived from its directory name, so relocating one orphans
//! its `db-data` volume — the site comes back empty while the bytes sit on disk
//! under a name nothing reads. Legacy adoption therefore registers `~/.atelier`
//! *where it is*. This module writes `sites.toml` (via temp file + rename in the
//! same directory) and, for [`add_site`] only, a new site's stack files into a
//! new or empty `sites/<slug>/`. [`remove_site`] unregisters; it deletes nothing.
//!
//! Resolution order — explicit beats implicit:
//! `--site <slug>` > `ATELIER_HOME` > `sites.toml` `active` > `~/.atelier`.
//! [`resolve`] is a pure function of those inputs; [`locate`] gathers them from
//! the process.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::net::TcpListener;
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::stack::{Stack, DEFAULT_PORT};

/// The registry's file name, inside the default stack directory.
pub const SITES_FILE: &str = "sites.toml";
/// Local ports SSH tunnels to remote consoles listen on (D-02). Local sites
/// never get one of these ([`next_free_port`] skips the range and `sites add
/// --port` refuses it), so a tunnel can never take a local site's port.
pub const TUNNEL_PORTS: RangeInclusive<u16> = 41300..=41399;
/// Name of the default stack directory under the user's home.
pub const DEFAULT_DIR: &str = ".atelier";
/// Sub-directory of the default stack that holds every site created from now on.
pub const SITES_DIR: &str = "sites";
/// Slug the adopted legacy `~/.atelier` is registered under.
pub const DEFAULT_SLUG: &str = "default";
/// Label the adopted legacy `~/.atelier` is registered under.
pub const DEFAULT_LABEL: &str = "My Site";
/// Compose project of the legacy `~/.atelier` — what [`Stack::project_name`]
/// already derives for it, and what it must keep forever.
pub const DEFAULT_PROJECT: &str = "atelier";

/// The default stack directory: `<user home>/.atelier`.
pub fn default_home(user_home: &Path) -> PathBuf {
    user_home.join(DEFAULT_DIR)
}

/// Where the registry lives: `<user home>/.atelier/sites.toml`.
pub fn registry_path(user_home: &Path) -> PathBuf {
    default_home(user_home).join(SITES_FILE)
}

/// Where a new registry site's stack goes: `<user home>/.atelier/sites/<slug>`.
pub fn site_home(user_home: &Path, slug: &str) -> PathBuf {
    default_home(user_home).join(SITES_DIR).join(slug)
}

/// The pinned Compose project for a new registry site: `atelier-<slug>`.
///
/// Not the bare slug — `sites/blog` would otherwise claim the host-global
/// Compose project `blog` and could collide with an unrelated stack.
pub fn project_for_slug(slug: &str) -> String {
    format!("{DEFAULT_PROJECT}-{slug}")
}

/// Whether `slug` is a valid site slug: `[a-z0-9][a-z0-9-]*`, at most 40 chars.
/// It becomes both a directory name and part of a Compose project name, so it
/// is held to the intersection of what both accept.
pub fn is_valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    slug.len() <= 40
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// How the Manager authenticates to a remote site's server (D-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyMode {
    /// The user's own SSH setup: `~/.ssh/config` alias, agent, default keys.
    #[default]
    Ssh,
    /// A per-server ed25519 key the Manager generated, held in the OS keychain.
    Guided,
}

impl KeyMode {
    fn as_toml(self) -> Option<String> {
        match self {
            KeyMode::Ssh => None,
            KeyMode::Guided => Some("guided".to_string()),
        }
    }

    fn from_toml(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("ssh") => Ok(KeyMode::Ssh),
            Some("guided") => Ok(KeyMode::Guided),
            Some(other) => bail!("unknown key mode `{other}` (expected `guided` or `ssh`)"),
        }
    }
}

/// Where a remote site lives: the server and its slug there (D-06).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The ssh target: an alias from `~/.ssh/config`, `host` or `user@host`.
    pub host: String,
    /// The site's slug in the **server's** registry (what `--site` gets there).
    pub slug: String,
    /// How to authenticate.
    pub key: KeyMode,
    /// Where the site's console is public, as the user recorded it: a host
    /// (`cms.example.com`, https assumed) or an `http(s)://host[:port]` base.
    /// `None`: the console is reached through an SSH tunnel (D-02).
    pub domain: Option<String>,
}

/// Whether `domain` is a usable public base for a remote console: a host name
/// (optionally `:port`), optionally prefixed by `http://` or `https://`, with
/// no path, credentials, whitespace or anything a shell or URL would reinterpret.
pub fn is_valid_domain(domain: &str) -> bool {
    let host = domain
        .strip_prefix("https://")
        .or_else(|| domain.strip_prefix("http://"))
        .unwrap_or(domain)
        .trim_end_matches('/');
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['-', '.', ':'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
}

/// Whether `host` is an ssh target we are willing to pass to `ssh`: printable,
/// no whitespace or shell metacharacters, and never starting with `-` (which
/// `ssh` would read as an option).
pub fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-@:[]%/".contains(c))
}

/// One registered site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Machine name — what `--site` takes. Stable.
    pub slug: String,
    /// Human name. Free to change; nothing keys off it.
    pub label: String,
    /// The stack directory (absolute; `~` already expanded).
    pub home: PathBuf,
    /// The Compose project, **pinned** at registration — never re-derived.
    pub project: String,
    /// `Some` for a site on another machine (M2): then `home` is only a
    /// Manager-side placeholder (`~/.atelier/remote/<slug>`, never a stack and
    /// never written by this module) and `project` is empty.
    pub remote: Option<Remote>,
}

impl Site {
    /// Whether this site lives on another machine.
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// A remote site's registry entry, pointing at `host`. Its slug on the
    /// server defaults to `slug`.
    pub fn new_remote(
        user_home: &Path,
        slug: &str,
        label: Option<&str>,
        host: &str,
        remote_slug: Option<&str>,
        key: KeyMode,
    ) -> Site {
        Site {
            slug: slug.to_string(),
            label: label
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .unwrap_or(slug)
                .to_string(),
            home: remote_placeholder_home(user_home, slug),
            project: String::new(),
            remote: Some(Remote {
                host: host.to_string(),
                slug: remote_slug.unwrap_or(slug).to_string(),
                key,
                domain: None,
            }),
        }
    }

    /// This site's [`Stack`], carrying its slug, label and pinned project.
    pub fn stack(&self) -> Stack {
        Stack {
            home: self.home.clone(),
            slug: Some(self.slug.clone()),
            label: Some(self.label.clone()),
            project: Some(self.project.clone()),
        }
    }
}

/// The parsed `sites.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    /// Slug of the active site, if one is set.
    pub active: Option<String>,
    /// Every registered site, in file order.
    pub sites: Vec<Site>,
}

/// On-disk shape. `home` stays a string so `~` survives a round trip.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default, rename = "site")]
    sites: Vec<SiteEntry>,
}

/// A site entry. `home`/`project` are required for a local site and absent for
/// a remote one; the remote fields are absent for a local one, so a local-only
/// file round-trips byte for byte (D-06).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteEntry {
    slug: String,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
}

/// Where a remote site's placeholder `home` points: `~/.atelier/remote/<slug>`.
/// Nothing in this module creates it; it only keeps `Site::home` meaningful
/// (and away from any real stack) for a site that has none here.
pub fn remote_placeholder_home(user_home: &Path, slug: &str) -> PathBuf {
    default_home(user_home).join("remote").join(slug)
}

const FILE_HEADER: &str = "# Atelier sites — managed by the atelier CLI and the manager app.\n\
# Each site's `project` is pinned: editing it detaches the site from its data.\n\n";

impl Registry {
    /// Read the registry. `Ok(None)` when the file does not exist — no registry,
    /// which means exactly today's single-stack behaviour. A file that exists but
    /// cannot be read, parsed or validated is an **error**, never treated as
    /// empty: silently replacing it would forget every site it lists.
    pub fn load(user_home: &Path) -> Result<Option<Self>> {
        let path = registry_path(user_home);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        Self::parse(&text, user_home)
            .map(Some)
            .with_context(|| format!("{} is not a valid sites registry", path.display()))
    }

    /// Parse and validate registry text. `user_home` expands `~`.
    pub fn parse(text: &str, user_home: &Path) -> Result<Self> {
        let file: RegistryFile = toml::from_str(text).context("could not parse TOML")?;
        let mut sites = Vec::with_capacity(file.sites.len());
        for entry in file.sites {
            if let Some(host) = entry.host {
                if entry.home.is_some() || entry.project.is_some() {
                    bail!(
                        "site `{}` is remote (host `{host}`): it has no local `home` or `project`",
                        entry.slug
                    );
                }
                let key = KeyMode::from_toml(entry.key.as_deref())
                    .with_context(|| format!("site `{}`", entry.slug))?;
                sites.push(Site::new_remote(
                    user_home,
                    &entry.slug,
                    Some(&entry.label),
                    &host,
                    entry.remote_site.as_deref(),
                    key,
                ));
                // `new_remote` falls back to the slug for an empty label; keep
                // the file's label verbatim.
                let site = sites.last_mut().expect("just pushed");
                site.label = entry.label;
                if let Some(remote) = site.remote.as_mut() {
                    remote.domain = entry.domain;
                }
                continue;
            }
            if entry.remote_site.is_some() || entry.key.is_some() || entry.domain.is_some() {
                bail!(
                    "site `{}` has `remote_site`/`key`/`domain` but no `host`",
                    entry.slug
                );
            }
            let (Some(raw_home), Some(project)) = (entry.home, entry.project) else {
                bail!("site `{}` needs a `home` and a `project`", entry.slug);
            };
            let home = expand_home(&raw_home, user_home).with_context(|| {
                format!("site `{}`: home `{raw_home}` is not absolute", entry.slug)
            })?;
            sites.push(Site {
                slug: entry.slug,
                label: entry.label,
                home,
                project,
                remote: None,
            });
        }
        let registry = Registry {
            active: file.active,
            sites,
        };
        registry.validate()?;
        Ok(registry)
    }

    /// Reject a registry two sites could collide in, or that points at nothing.
    pub fn validate(&self) -> Result<()> {
        let mut slugs = HashSet::new();
        let mut projects = HashSet::new();
        let mut homes = HashSet::new();
        let mut targets = HashSet::new();
        for site in &self.sites {
            if !is_valid_slug(&site.slug) {
                bail!("`{}` is not a valid site slug (a-z, 0-9, -)", site.slug);
            }
            if let Some(remote) = &site.remote {
                if !is_valid_host(&remote.host) {
                    bail!(
                        "site `{}`: `{}` is not a usable ssh target",
                        site.slug,
                        remote.host
                    );
                }
                if !is_valid_slug(&remote.slug) {
                    bail!(
                        "site `{}`: `{}` is not a valid site slug on the server",
                        site.slug,
                        remote.slug
                    );
                }
                if let Some(domain) = &remote.domain {
                    if !is_valid_domain(domain) {
                        bail!(
                            "site `{}`: `{domain}` is not a usable domain (a host name, or \
                             https://host[:port])",
                            site.slug
                        );
                    }
                }
                if !slugs.insert(site.slug.as_str()) {
                    bail!("site `{}` is listed twice", site.slug);
                }
                if !targets.insert((remote.host.as_str(), remote.slug.as_str())) {
                    bail!(
                        "two sites point at site `{}` on {}",
                        remote.slug,
                        remote.host
                    );
                }
                // No home, project or port: nothing local to collide on.
                continue;
            }
            if site.project.trim().is_empty() {
                bail!("site `{}` has no Compose project", site.slug);
            }
            if !slugs.insert(site.slug.as_str()) {
                bail!("site `{}` is listed twice", site.slug);
            }
            if !projects.insert(site.project.as_str()) {
                bail!(
                    "two sites share the Compose project `{}` — they would share one database",
                    site.project
                );
            }
            if !homes.insert(normalize(&site.home)) {
                bail!("two sites share the directory {}", site.home.display());
            }
        }
        if let Some(active) = &self.active {
            if !slugs.contains(active.as_str()) {
                bail!("the active site `{active}` is not registered");
            }
        }
        Ok(())
    }

    /// The registry as `sites.toml` text. Homes under `user_home` are written as
    /// `~/…` so the file stays readable and portable across a home rename.
    pub fn to_toml(&self, user_home: &Path) -> Result<String> {
        let file = RegistryFile {
            active: self.active.clone(),
            sites: self
                .sites
                .iter()
                .map(|s| match &s.remote {
                    None => SiteEntry {
                        slug: s.slug.clone(),
                        label: s.label.clone(),
                        home: Some(contract_home(&s.home, user_home)),
                        project: Some(s.project.clone()),
                        host: None,
                        remote_site: None,
                        key: None,
                        domain: None,
                    },
                    Some(r) => SiteEntry {
                        slug: s.slug.clone(),
                        label: s.label.clone(),
                        home: None,
                        project: None,
                        host: Some(r.host.clone()),
                        remote_site: Some(r.slug.clone()),
                        key: r.key.as_toml(),
                        domain: r.domain.clone(),
                    },
                })
                .collect(),
        };
        let body = toml::to_string(&file).context("could not serialize the sites registry")?;
        Ok(format!("{FILE_HEADER}{body}"))
    }

    /// Validate, then write atomically: a temp file beside `sites.toml`, renamed
    /// over it. A crash mid-write leaves the previous registry intact. Creates
    /// nothing but that one file (and the `~/.atelier` directory if absent).
    pub fn save(&self, user_home: &Path) -> Result<()> {
        self.validate()?;
        let text = self.to_toml(user_home)?;
        let dir = default_home(user_home);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        let path = registry_path(user_home);
        let tmp = dir.join(format!(".{SITES_FILE}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("could not write {}", tmp.display()))?;
        // The only rename in this module: our own temp file onto sites.toml.
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("could not replace {}", path.display()));
        }
        Ok(())
    }

    /// The site registered under `slug`.
    pub fn get(&self, slug: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.slug == slug)
    }

    /// The site whose directory is `home` (compared lexically, without touching
    /// the filesystem), if any.
    pub fn find_by_home(&self, home: &Path) -> Option<&Site> {
        let want = normalize(home);
        self.local_sites().find(|s| normalize(&s.home) == want)
    }

    /// The sites with a stack on this machine — the only ones home, project
    /// and port rules apply to.
    pub fn local_sites(&self) -> impl Iterator<Item = &Site> {
        self.sites.iter().filter(|s| !s.is_remote())
    }

    /// The active site, if one is set.
    pub fn active_site(&self) -> Option<&Site> {
        self.active.as_deref().and_then(|slug| self.get(slug))
    }

    /// The registered site, other than the one at `except_home`, that claims
    /// `port` in its `.env` — i.e. a site that cannot run at the same time.
    pub fn port_claimant(&self, port: u16, except_home: &Path) -> Option<&Site> {
        let except = normalize(except_home);
        self.local_sites().find(|s| {
            normalize(&s.home) != except && {
                let stack = s.stack();
                stack.env_path().is_file() && stack.http_port() == port
            }
        })
    }

    /// Every console port a registered site has claimed in its `.env`. A site
    /// with no `.env` yet (never installed) claims nothing; one whose `.env`
    /// lacks `HTTP_PORT` runs on — and so claims — [`DEFAULT_PORT`].
    pub fn used_ports(&self) -> BTreeSet<u16> {
        self.local_sites()
            .map(Site::stack)
            .filter(|stack| stack.env_path().is_file())
            .map(|stack| stack.http_port())
            .collect()
    }
}

/// Expand a stored `home`: `~` / `~/…` against `user_home`, else it must be
/// absolute. A relative path would resolve against whatever directory the
/// command happens to run from, so it is refused rather than guessed at.
fn expand_home(raw: &str, user_home: &Path) -> Option<PathBuf> {
    if raw == "~" {
        return Some(user_home.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Some(user_home.join(rest));
    }
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

/// Inverse of [`expand_home`] for writing.
fn contract_home(home: &Path, user_home: &Path) -> String {
    match normalize(home).strip_prefix(normalize(user_home)) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
        Err(_) => home.to_string_lossy().into_owned(),
    }
}

/// Lexical normalization: drops `.` components and trailing separators. No
/// filesystem access, so it is safe on paths that do not exist.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Which rung of the resolution order chose the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--site <slug>`.
    Flag,
    /// `ATELIER_HOME`.
    Env,
    /// The registry's `active` site.
    Active,
    /// The fallback `~/.atelier`.
    Default,
}

/// The answer to "which site?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The stack directory to operate on.
    pub home: PathBuf,
    /// The registry entry for `home`, when it is registered. `None` for a
    /// hand-rolled `ATELIER_HOME` or an unadopted `~/.atelier`: those keep
    /// today's derived project name.
    pub site: Option<Site>,
    /// Which rung chose it.
    pub source: Source,
}

impl Resolution {
    /// The Compose project this stack must run under: the pinned one when the
    /// home is registered, else today's derivation from the directory name.
    pub fn project(&self) -> String {
        self.clone().into_stack().project_name()
    }

    /// The [`Stack`] to operate on — a registered site's (pinned project), or
    /// an unregistered one at `home` (derived project).
    pub fn into_stack(self) -> Stack {
        match self.site {
            Some(site) => site.stack(),
            None => Stack::at(self.home),
        }
    }
}

/// Resolve the stack — `--site` > `ATELIER_HOME` > registry `active` >
/// `~/.atelier` — purely from its inputs (no environment or filesystem reads).
///
/// - `site_flag`: the `--site` value.
/// - `atelier_home`: the `ATELIER_HOME` value, used verbatim as today.
/// - `registry`: the loaded registry, `None` when there is none.
/// - `user_home`: the user's home directory; only needed by the `~/.atelier`
///   fallback, so an `ATELIER_HOME` keeps working without one, as today.
///
/// With no registry and no `--site` this is exactly the old `Stack::locate`.
pub fn resolve(
    site_flag: Option<&str>,
    atelier_home: Option<&Path>,
    registry: Option<&Registry>,
    user_home: Option<&Path>,
) -> Result<Resolution> {
    if let Some(slug) = site_flag {
        let Some(registry) = registry else {
            bail!("no site named `{slug}`: no sites are registered yet");
        };
        let Some(site) = registry.get(slug) else {
            let known: Vec<&str> = registry.sites.iter().map(|s| s.slug.as_str()).collect();
            bail!(
                "no site named `{slug}` (registered: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            );
        };
        return Ok(Resolution {
            home: site.home.clone(),
            site: Some(site.clone()),
            source: Source::Flag,
        });
    }
    if let Some(home) = atelier_home {
        // A registered home keeps its pinned project even when reached by path;
        // anything else is hand-rolled and stays unregistered.
        return Ok(Resolution {
            home: home.to_path_buf(),
            site: registry.and_then(|r| r.find_by_home(home)).cloned(),
            source: Source::Env,
        });
    }
    if let Some(site) = registry.and_then(Registry::active_site) {
        return Ok(Resolution {
            home: site.home.clone(),
            site: Some(site.clone()),
            source: Source::Active,
        });
    }
    let home = default_home(user_home.context("could not determine your home directory")?);
    Ok(Resolution {
        site: registry.and_then(|r| r.find_by_home(&home)).cloned(),
        home,
        source: Source::Default,
    })
}

/// [`resolve`] with its inputs gathered from the process: `ATELIER_HOME`, the
/// user's home directory and `sites.toml`.
///
/// The registry is only read when a rung needs it. Under `ATELIER_HOME` (no
/// `--site`) an unreadable registry is ignored, so the operator's override can
/// never be broken by a bad file; for `--site` and the `active` rung a corrupt
/// registry is an error rather than a silent fall-through to the wrong site.
pub fn locate(site_flag: Option<&str>) -> Result<Resolution> {
    let atelier_home: Option<OsString> = std::env::var_os("ATELIER_HOME");
    let user_home = dirs::home_dir();
    locate_in(
        site_flag,
        atelier_home.as_deref().map(Path::new),
        user_home.as_deref(),
    )
}

/// [`locate`] with its inputs injected: reads the registry under `user_home`
/// (as [`load_effective`] sees it) and resolves. What `locate` does after
/// reading the process environment — split out so it is testable on a temp dir.
pub fn locate_in(
    site_flag: Option<&str>,
    atelier_home: Option<&Path>,
    user_home: Option<&Path>,
) -> Result<Resolution> {
    let registry = match (user_home, site_flag, atelier_home) {
        (None, _, _) => None,
        (Some(uh), None, Some(_)) => load_effective(uh).ok().flatten(),
        (Some(uh), _, _) => load_effective(uh)?,
    };
    resolve(site_flag, atelier_home, registry.as_ref(), user_home)
}

/// The registry as commands should see it: what is on disk plus — **in memory
/// only** — the entry [`adopt_legacy`] would add for an installed, unregistered
/// `~/.atelier`. Lets `--site default` and `sites list` work before anything
/// has written `sites.toml`, without writing it. Resolution is unchanged by the
/// preview: the legacy site's pinned project is the `atelier` it derives to.
pub fn load_effective(user_home: &Path) -> Result<Option<Registry>> {
    let loaded = Registry::load(user_home)?;
    let Some(site) = legacy_candidate(loaded.as_ref(), user_home) else {
        return Ok(loaded);
    };
    let mut registry = loaded.unwrap_or_default();
    if registry.active.is_none() {
        registry.active = Some(site.slug.clone());
    }
    registry.sites.insert(0, site);
    Ok(Some(registry))
}

/// The entry adoption would add, or `None` when there is nothing to adopt (no
/// installed `~/.atelier`, already registered, or the `default` slug is taken —
/// which [`adopt_legacy`] reports as an error when it actually runs).
fn legacy_candidate(registry: Option<&Registry>, user_home: &Path) -> Option<Site> {
    let home = default_home(user_home);
    if !Stack::at(home.clone()).exists() {
        return None;
    }
    if let Some(r) = registry {
        if r.find_by_home(&home).is_some() || r.get(DEFAULT_SLUG).is_some() {
            return None;
        }
    }
    Some(Site {
        slug: DEFAULT_SLUG.to_string(),
        label: DEFAULT_LABEL.to_string(),
        home,
        project: DEFAULT_PROJECT.to_string(),
        remote: None,
    })
}

/// The first port from [`DEFAULT_PORT`] upward that no registered site claims
/// (`used`) and that `is_free` reports bindable on this host. Deterministic in
/// its inputs; `None` only if every port up to 65535 is taken.
///
/// Pair with [`Registry::used_ports`] and [`host_port_is_free`]. Run legacy
/// adoption first, so an installed `~/.atelier`'s port counts as used even
/// while it is stopped.
pub fn next_free_port(used: &BTreeSet<u16>, mut is_free: impl FnMut(u16) -> bool) -> Option<u16> {
    (DEFAULT_PORT..=u16::MAX)
        .find(|port| !TUNNEL_PORTS.contains(port) && !used.contains(port) && is_free(*port))
}

/// The first port in [`TUNNEL_PORTS`] that `is_free` reports bindable, for an
/// SSH tunnel to a remote console. `None` when the whole range is taken.
pub fn next_tunnel_port(mut is_free: impl FnMut(u16) -> bool) -> Option<u16> {
    TUNNEL_PORTS.clone().find(|port| is_free(*port))
}

/// Whether `port` can be bound on loopback right now — what a tunnel needs
/// (it listens on 127.0.0.1 only).
pub fn loopback_port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Whether `port` can be bound on this host right now — on all interfaces
/// (where Docker publishes) and on loopback. Probes and releases immediately.
pub fn host_port_is_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Register an existing `~/.atelier` **in place**, once: as slug `default`,
/// label "My Site", project `atelier` (what it already runs under), and active
/// if nothing else is. Returns the new entry, or `None` when there was nothing
/// to do — no installed `~/.atelier`, or it is already registered.
///
/// One-way and idempotent, like the `Stack::migrate_*` repairs. It writes only
/// `sites.toml`; the stack directory is never moved, renamed or touched.
/// A corrupt registry is an error, never overwritten.
pub fn adopt_legacy(user_home: &Path) -> Result<Option<Site>> {
    let home = default_home(user_home);
    let legacy = Stack::at(home.clone());
    if !legacy.exists() {
        return Ok(None);
    }
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    if registry.find_by_home(&home).is_some() {
        return Ok(None);
    }
    if registry.get(DEFAULT_SLUG).is_some() {
        bail!(
            "cannot register {} as `{DEFAULT_SLUG}`: another site already uses that slug",
            home.display()
        );
    }
    // The pin must equal what the stack has always run under, or adoption would
    // detach it from its volumes.
    debug_assert_eq!(legacy.project_name(), DEFAULT_PROJECT);
    let site = Site {
        slug: DEFAULT_SLUG.to_string(),
        label: DEFAULT_LABEL.to_string(),
        home,
        project: DEFAULT_PROJECT.to_string(),
        remote: None,
    };
    registry.sites.insert(0, site.clone());
    if registry.active.is_none() {
        registry.active = Some(site.slug.clone());
    }
    registry.save(user_home)?;
    Ok(Some(site))
}

// --- `atelier sites …` -------------------------------------------------------
//
// Each command below runs [`adopt_legacy`] before it writes the registry, so the
// first write on a machine with an existing `~/.atelier` registers it in place
// first. None of them moves, renames or deletes a stack directory: `add` only
// creates files in a new (or empty) `sites/<slug>/`, and everything else edits
// `sites.toml` alone.

/// One row of `atelier sites list` — everything that can be read from disk.
/// Whether the site is running is Docker's to answer; the front-end asks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SiteRow {
    pub slug: String,
    pub label: String,
    pub home: PathBuf,
    pub project: String,
    /// The console port from the site's `.env`; `None` before it is set up.
    pub port: Option<u16>,
    /// A `compose.yaml` has been laid down.
    pub installed: bool,
    /// The image the site's `.env` names; `None` before it is set up.
    pub image: Option<String>,
    /// This is the registry's active site.
    pub active: bool,
    /// The ssh target of a remote site (M2); absent for a local one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// A remote site's slug on its server; absent for a local one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_site: Option<String>,
}

impl SiteRow {
    /// This site's [`Stack`] (pinned project).
    pub fn stack(&self) -> Stack {
        Stack {
            home: self.home.clone(),
            slug: Some(self.slug.clone()),
            label: Some(self.label.clone()),
            project: Some(self.project.clone()),
        }
    }
}

/// The user's home directory — what every `sites` command is rooted at.
pub fn user_home() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine your home directory")
}

/// Every registered site (plus an installed `~/.atelier` not yet adopted — see
/// [`load_effective`]), in registry order. Read-only: writes nothing.
pub fn list_sites(user_home: &Path) -> Result<Vec<SiteRow>> {
    let Some(registry) = load_effective(user_home)? else {
        return Ok(Vec::new());
    };
    Ok(registry
        .sites
        .iter()
        .map(|site| {
            let stack = site.stack();
            // A remote site's state is the server's to answer; nothing here is
            // read for it (its placeholder home holds no stack).
            let configured = !site.is_remote() && stack.env_path().is_file();
            SiteRow {
                slug: site.slug.clone(),
                label: site.label.clone(),
                home: site.home.clone(),
                project: site.project.clone(),
                port: configured.then(|| stack.http_port()),
                installed: !site.is_remote() && stack.exists(),
                image: configured.then(|| stack.image()),
                active: registry.active.as_deref() == Some(site.slug.as_str()),
                host: site.remote.as_ref().map(|r| r.host.clone()),
                remote_site: site.remote.as_ref().map(|r| r.slug.clone()),
            }
        })
        .collect())
}

/// What [`add_site`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedSite {
    pub site: Site,
    /// The console port the site was set up on.
    pub port: u16,
    /// `sites/<slug>/` already held a stack (a site removed earlier): it was
    /// registered again where it is, with its data and its port.
    pub reregistered: bool,
    /// It became the active site (there was none).
    pub activated: bool,
}

/// Register a new site `slug` at `~/.atelier/sites/<slug>`, pinned to project
/// `atelier-<slug>`, and lay down its stack files on a port no other site
/// claims — so `ops::install` on it pulls and starts without a collision.
///
/// - `port`: an explicit console port; refused when another registered site
///   claims it or `is_free` says the host has it taken. `None` allocates with
///   [`next_free_port`].
/// - `is_free`: the host probe — [`host_port_is_free`] in production.
///
/// When `sites/<slug>/` already holds a stack (left behind by `sites remove`,
/// which never deletes anything), it is registered again **in place** and keeps
/// its port unless `port` says otherwise. A non-empty directory that is not a
/// stack is refused untouched. The new site becomes active only when no site is.
pub fn add_site(
    user_home: &Path,
    slug: &str,
    label: Option<&str>,
    port: Option<u16>,
    mut is_free: impl FnMut(u16) -> bool,
) -> Result<AddedSite> {
    if !is_valid_slug(slug) {
        bail!(
            "`{slug}` is not a valid site name — use lowercase letters, digits and `-` \
             (starting with a letter or digit, at most 40 characters)"
        );
    }
    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    if registry.get(slug).is_some() {
        bail!("a site named `{slug}` is already registered");
    }

    let site = Site {
        slug: slug.to_string(),
        label: label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(slug)
            .to_string(),
        home: site_home(user_home, slug),
        project: project_for_slug(slug),
        remote: None,
    };
    // Every collision the registry can name (project, directory) is refused
    // before anything is written to disk.
    let mut candidate = registry.clone();
    candidate.sites.push(site.clone());
    candidate.validate()?;

    let stack = site.stack();
    let reregistered = stack.exists();
    if !reregistered && dir_has_entries(&site.home)? {
        bail!(
            "{} already exists and is not an Atelier site — nothing was changed. \
             Pick another name.",
            site.home.display()
        );
    }

    let claimed = |p: u16| {
        registry
            .port_claimant(p, &site.home)
            .map(|s| s.slug.clone())
    };
    let chosen = match port {
        Some(p) => {
            if TUNNEL_PORTS.contains(&p) {
                bail!(
                    "ports {}–{} are kept for SSH tunnels to remote sites — pick another \
                     with --port",
                    TUNNEL_PORTS.start(),
                    TUNNEL_PORTS.end()
                );
            }
            if let Some(other) = claimed(p) {
                bail!("port {p} is already the console port of site `{other}`");
            }
            let own = reregistered && stack.env_path().is_file() && stack.http_port() == p;
            if !own && !is_free(p) {
                bail!("port {p} is in use on this machine — pick another with --port");
            }
            p
        }
        None if reregistered && stack.env_path().is_file() => {
            let p = stack.http_port();
            if let Some(other) = claimed(p) {
                bail!(
                    "{} is set up on port {p}, which site `{other}` now uses — \
                     pass --port to give it another",
                    site.home.display()
                );
            }
            p
        }
        None => next_free_port(&registry.used_ports(), &mut is_free)
            .context("no free port left to give the new site")?,
    };

    // Creates the directory and writes compose.yaml / edge.conf / .env where
    // absent; on a re-registered stack only an explicit --port is written.
    let explicit = port.is_some() || !stack.env_path().is_file();
    stack.ensure_scaffold(&crate::stack::InstallOptions {
        image: None,
        http_port: explicit.then_some(chosen),
    })?;

    registry.sites.push(site.clone());
    let activated = registry.active.is_none();
    if activated {
        registry.active = Some(site.slug.clone());
    }
    registry.save(user_home)?;
    Ok(AddedSite {
        site,
        port: chosen,
        reregistered,
        activated,
    })
}

/// What [`add_remote_site`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedRemoteSite {
    pub site: Site,
    /// It became the active site (there was none).
    pub activated: bool,
}

/// Register `site` (built with [`Site::new_remote`]) — a site that already runs
/// on another machine (U-03: attach only, no remote install).
///
/// `verify` is the caller's proof that the server answers and runs that site
/// (in production: [`crate::remote`] over SSH). It runs after every local check
/// and **before** anything is written; when it fails, `sites.toml` is left
/// exactly as it was — not even legacy adoption has happened.
pub fn add_remote_site(
    user_home: &Path,
    site: Site,
    verify: impl FnOnce(&Site) -> Result<()>,
) -> Result<AddedRemoteSite> {
    if !is_valid_slug(&site.slug) {
        bail!(
            "`{}` is not a valid site name — use lowercase letters, digits and `-` \
             (starting with a letter or digit, at most 40 characters)",
            site.slug
        );
    }
    if !site.is_remote() {
        bail!("internal: add_remote_site called with a local site");
    }
    // Check against the registry as commands see it (incl. an unadopted
    // ~/.atelier) without writing anything yet.
    let effective = load_effective(user_home)?.unwrap_or_default();
    if effective.get(&site.slug).is_some() {
        bail!("a site named `{}` is already registered", site.slug);
    }
    let mut candidate = effective;
    candidate.sites.push(site.clone());
    candidate.validate()?;

    verify(&site)?;

    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    registry.sites.push(site.clone());
    let activated = registry.active.is_none();
    if activated {
        registry.active = Some(site.slug.clone());
    }
    registry.save(user_home)?;
    Ok(AddedRemoteSite { site, activated })
}

/// Make `slug` the active site — what `atelier` operates on when neither
/// `--site` nor `ATELIER_HOME` says otherwise.
pub fn use_site(user_home: &Path, slug: &str) -> Result<Site> {
    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    let site = registered(&registry, slug)?.clone();
    registry.active = Some(site.slug.clone());
    registry.save(user_home)?;
    Ok(site)
}

/// Change a site's **label**. Its slug, directory and pinned Compose project
/// are untouched — the label is the one thing nothing keys off.
pub fn rename_site(user_home: &Path, slug: &str, label: &str) -> Result<Site> {
    let label = label.trim();
    if label.is_empty() {
        bail!("the new name can't be empty");
    }
    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    registered(&registry, slug)?;
    let site = registry
        .sites
        .iter_mut()
        .find(|s| s.slug == slug)
        .expect("checked above");
    site.label = label.to_string();
    let site = site.clone();
    registry.save(user_home)?;
    Ok(site)
}

/// Record (or with `None`, clear) where a remote site is public — its
/// [`Remote::domain`]. Only a remote site has one; a domain is validated with
/// [`is_valid_domain`] and stored as given.
pub fn set_domain(user_home: &Path, slug: &str, domain: Option<&str>) -> Result<Site> {
    let domain = domain.map(str::trim).filter(|d| !d.is_empty());
    if let Some(d) = domain.filter(|d| !is_valid_domain(d)) {
        bail!("`{d}` is not a usable domain — a host name, or https://host[:port]");
    }
    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    registered(&registry, slug)?;
    let site = registry
        .sites
        .iter_mut()
        .find(|s| s.slug == slug)
        .expect("checked above");
    let Some(remote) = site.remote.as_mut() else {
        bail!(
            "`{slug}` is a site on this computer — a domain is only recorded for a site on a \
             server (its console here is always its local address)"
        );
    };
    remote.domain = domain.map(str::to_string);
    let site = site.clone();
    registry.save(user_home)?;
    Ok(site)
}

/// What [`remove_site`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedSite {
    /// The entry that was removed — its directory and volumes still exist.
    pub site: Site,
    /// It was the active site; nothing is active now (`~/.atelier` is the
    /// fallback again).
    pub was_active: bool,
}

/// **Unregister** `slug`: drop it from `sites.toml`. Nothing on disk or in
/// Docker is deleted — the stack directory, its backups and its volumes stay,
/// and `add_site` with the same slug registers it again in place.
///
/// The site at `~/.atelier` is refused: adoption registers it again on the next
/// write, so removing it could never stick.
pub fn remove_site(user_home: &Path, slug: &str) -> Result<RemovedSite> {
    adopt_legacy(user_home)?;
    let mut registry = Registry::load(user_home)?.unwrap_or_default();
    let site = registered(&registry, slug)?.clone();
    if normalize(&site.home) == normalize(&default_home(user_home)) {
        bail!(
            "`{slug}` is the site in {}, which is always registered — it can't be removed",
            site.home.display()
        );
    }
    registry.sites.retain(|s| s.slug != slug);
    let was_active = registry.active.as_deref() == Some(slug);
    if was_active {
        registry.active = None;
    }
    registry.save(user_home)?;
    Ok(RemovedSite { site, was_active })
}

/// The console port `ops::install` should be given for `stack`, honouring the
/// rule that `http_port: None` means *allocate* for a registry site:
///
/// - an explicit `--port` always wins;
/// - a stack with a `.env` keeps the port it has (`None`) — re-running install
///   must never move it;
/// - a registered site under `sites/` that has no `.env` yet gets a port no
///   other site claims;
/// - anything else (`~/.atelier`, a hand-rolled `ATELIER_HOME`) keeps meaning
///   [`DEFAULT_PORT`] (`None`).
pub fn port_for_install(
    stack: &Stack,
    explicit: Option<u16>,
    registry: Option<&Registry>,
    user_home: Option<&Path>,
    is_free: impl FnMut(u16) -> bool,
) -> Option<u16> {
    if explicit.is_some() {
        return explicit;
    }
    if stack.env_path().is_file() || stack.slug.is_none() {
        return None;
    }
    if user_home.is_some_and(|uh| normalize(&stack.home) == normalize(&default_home(uh))) {
        return None;
    }
    let used = registry.map(Registry::used_ports).unwrap_or_default();
    next_free_port(&used, is_free)
}

fn registered<'a>(registry: &'a Registry, slug: &str) -> Result<&'a Site> {
    registry.get(slug).with_context(|| {
        let known: Vec<&str> = registry.sites.iter().map(|s| s.slug.as_str()).collect();
        format!(
            "no site named `{slug}` (registered: {})",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )
    })
}

/// Whether `dir` exists and holds anything. Absent is `false`.
fn dir_has_entries(dir: &Path) -> Result<bool> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("could not read {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fake user home in a unique temp directory, removed on drop.
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "atelier-sites-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempHome(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn site(user_home: &Path, slug: &str) -> Site {
        Site {
            slug: slug.to_string(),
            label: slug.to_uppercase(),
            home: site_home(user_home, slug),
            project: project_for_slug(slug),
            remote: None,
        }
    }

    fn legacy_site(user_home: &Path) -> Site {
        Site {
            slug: DEFAULT_SLUG.to_string(),
            label: DEFAULT_LABEL.to_string(),
            home: default_home(user_home),
            project: DEFAULT_PROJECT.to_string(),
            remote: None,
        }
    }

    /// Lay down an installed legacy `~/.atelier` with some data-ish files.
    fn install_legacy(user_home: &Path) -> Stack {
        let stack = Stack::at(default_home(user_home));
        stack
            .ensure_scaffold(&crate::stack::InstallOptions::default())
            .unwrap();
        std::fs::create_dir_all(stack.backups_dir()).unwrap();
        std::fs::write(stack.backups_dir().join("aincient-1.tar.gz"), b"bundle").unwrap();
        stack
    }

    /// Every file under `dir` with its bytes.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(dir).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let th = TempHome::new();
        let uh = th.path();
        let registry = Registry {
            active: Some("blog".to_string()),
            sites: vec![
                legacy_site(uh),
                site(uh, "blog"),
                Site {
                    slug: "elsewhere".to_string(),
                    label: "Off home".to_string(),
                    home: PathBuf::from("/srv/atelier-elsewhere"),
                    project: "atelier-elsewhere".to_string(),
                    remote: None,
                },
            ],
        };
        registry.save(uh).unwrap();

        let text = std::fs::read_to_string(registry_path(uh)).unwrap();
        assert!(text.contains("home = \"~/.atelier\""), "{text}");
        assert!(text.contains("home = \"~/.atelier/sites/blog\""), "{text}");
        assert!(text.contains("home = \"/srv/atelier-elsewhere\""), "{text}");
        assert!(text.contains("project = \"atelier-blog\""), "{text}");
        assert!(text.find("active").unwrap() < text.find("[[site]]").unwrap());

        assert_eq!(Registry::load(uh).unwrap(), Some(registry));
        // No temp file left behind.
        let names: Vec<_> = std::fs::read_dir(default_home(uh))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(SITES_FILE)]);
    }

    #[test]
    fn parses_the_documented_schema() {
        let uh = Path::new("/Users/someone");
        let text = r#"
active = "default"

[[site]]
slug = "default"
label = "My Site"
home = "~/.atelier"
project = "atelier"        # PINNED, not derived

[[site]]
slug = "blog"
label = "Blog"
home = "~/.atelier/sites/blog"
project = "atelier-blog"
"#;
        let r = Registry::parse(text, uh).unwrap();
        assert_eq!(r.active.as_deref(), Some("default"));
        assert_eq!(
            r.sites,
            vec![legacy_site(uh), {
                let mut s = site(uh, "blog");
                s.label = "Blog".to_string();
                s
            }]
        );
    }

    #[test]
    fn a_missing_registry_is_none_not_an_error() {
        let th = TempHome::new();
        assert_eq!(Registry::load(th.path()).unwrap(), None);
    }

    #[test]
    fn a_corrupt_registry_is_an_error_and_is_left_alone() {
        let th = TempHome::new();
        let uh = th.path();
        std::fs::create_dir_all(default_home(uh)).unwrap();
        let cases = [
            "this is = = not toml",
            "[[site]]\nslug = \"x\"\n", // missing fields
            "unknown = 1\n",            // unknown key
            "active = \"nope\"\n",      // dangling active
            "[[site]]\nslug = \"Bad Slug\"\nlabel = \"\"\nhome = \"/a\"\nproject = \"p\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"\"\nhome = \"rel/dir\"\nproject = \"p\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"\"\nhome = \"/a\"\nproject = \"p\"\n\
             [[site]]\nslug = \"a\"\nlabel = \"\"\nhome = \"/b\"\nproject = \"q\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"\"\nhome = \"/a\"\nproject = \"p\"\n\
             [[site]]\nslug = \"b\"\nlabel = \"\"\nhome = \"/b\"\nproject = \"p\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"\"\nhome = \"/a\"\nproject = \"p\"\n\
             [[site]]\nslug = \"b\"\nlabel = \"\"\nhome = \"/a/\"\nproject = \"q\"\n",
        ];
        for text in cases {
            std::fs::write(registry_path(uh), text).unwrap();
            assert!(Registry::load(uh).is_err(), "accepted: {text}");
            // Adoption must refuse too, without overwriting the file.
            install_legacy(uh);
            assert!(adopt_legacy(uh).is_err(), "adopted over: {text}");
            assert_eq!(std::fs::read_to_string(registry_path(uh)).unwrap(), text);
        }
    }

    #[test]
    fn save_refuses_an_invalid_registry() {
        let th = TempHome::new();
        let uh = th.path();
        let mut dup = site(uh, "blog");
        dup.slug = "shop".to_string();
        dup.home = site_home(uh, "shop");
        let r = Registry {
            active: None,
            sites: vec![site(uh, "blog"), dup],
        };
        assert!(r.save(uh).is_err(), "two sites sharing a project");
        assert!(!registry_path(uh).exists());
    }

    #[test]
    fn slugs_and_projects() {
        for ok in ["default", "blog", "a", "site-2", "9lives"] {
            assert!(is_valid_slug(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "Blog",
            "a b",
            "a_b",
            "a.b",
            "../x",
            &"x".repeat(41),
        ] {
            assert!(!is_valid_slug(bad), "{bad}");
        }
        assert_eq!(project_for_slug("blog"), "atelier-blog");
        let uh = Path::new("/home/u");
        assert_eq!(
            site_home(uh, "blog"),
            PathBuf::from("/home/u/.atelier/sites/blog")
        );
        assert_eq!(
            registry_path(uh),
            PathBuf::from("/home/u/.atelier/sites.toml")
        );
    }

    fn two_site_registry(uh: &Path) -> Registry {
        Registry {
            active: Some("blog".to_string()),
            sites: vec![legacy_site(uh), site(uh, "blog"), site(uh, "shop")],
        }
    }

    #[test]
    fn resolution_flag_beats_everything() {
        let uh = Path::new("/home/u");
        let r = two_site_registry(uh);
        let got = resolve(
            Some("shop"),
            Some(Path::new("/tmp/hand-rolled")),
            Some(&r),
            Some(uh),
        )
        .unwrap();
        assert_eq!(got.source, Source::Flag);
        assert_eq!(got.home, site_home(uh, "shop"));
        assert_eq!(got.project(), "atelier-shop");

        assert!(resolve(Some("nope"), None, Some(&r), Some(uh)).is_err());
        assert!(resolve(Some("shop"), None, None, Some(uh)).is_err());
    }

    #[test]
    fn resolution_env_beats_the_registry() {
        let uh = Path::new("/home/u");
        let r = two_site_registry(uh);

        // Hand-rolled: unregistered, today's derived project.
        let got = resolve(
            None,
            Some(Path::new("/tmp/atelier-staging")),
            Some(&r),
            Some(uh),
        )
        .unwrap();
        assert_eq!(got.source, Source::Env);
        assert_eq!(got.home, PathBuf::from("/tmp/atelier-staging"));
        assert_eq!(got.site, None);
        assert_eq!(got.project(), "atelier-staging");

        // A registered home reached by path keeps its PINNED project — the
        // derived one would be `shop`, a different Compose project entirely.
        let shop = site_home(uh, "shop");
        let got = resolve(None, Some(&shop), Some(&r), Some(uh)).unwrap();
        assert_eq!(got.source, Source::Env);
        assert_eq!(got.project(), "atelier-shop");

        // Works without a user home, as `Stack::locate` always has.
        let got = resolve(None, Some(Path::new("/x/y")), None, None).unwrap();
        assert_eq!(got.home, PathBuf::from("/x/y"));
    }

    #[test]
    fn resolution_active_beats_the_default() {
        let uh = Path::new("/home/u");
        let r = two_site_registry(uh);
        let got = resolve(None, None, Some(&r), Some(uh)).unwrap();
        assert_eq!(got.source, Source::Active);
        assert_eq!(got.home, site_home(uh, "blog"));
        assert_eq!(got.project(), "atelier-blog");
    }

    #[test]
    fn resolution_falls_back_to_the_default_home() {
        let uh = Path::new("/home/u");
        // No registry: exactly today's behaviour.
        let got = resolve(None, None, None, Some(uh)).unwrap();
        assert_eq!(got.source, Source::Default);
        assert_eq!(got.home, PathBuf::from("/home/u/.atelier"));
        assert_eq!(got.site, None);
        assert_eq!(got.project(), "atelier");

        // A registry with no active site: still ~/.atelier, with its entry.
        let mut r = two_site_registry(uh);
        r.active = None;
        let got = resolve(None, None, Some(&r), Some(uh)).unwrap();
        assert_eq!(got.source, Source::Default);
        assert_eq!(got.site, Some(legacy_site(uh)));
        assert_eq!(got.project(), "atelier");

        assert!(resolve(None, None, None, None).is_err());
    }

    #[test]
    fn port_allocation_skips_used_and_busy_ports() {
        let none = BTreeSet::new();
        assert_eq!(next_free_port(&none, |_| true), Some(DEFAULT_PORT));

        let used: BTreeSet<u16> = [DEFAULT_PORT, DEFAULT_PORT + 1, DEFAULT_PORT + 3].into();
        let busy = [DEFAULT_PORT + 2];
        assert_eq!(
            next_free_port(&used, |p| !busy.contains(&p)),
            Some(DEFAULT_PORT + 4)
        );
        // A used port is never even probed.
        let mut probed = Vec::new();
        next_free_port(&used, |p| {
            probed.push(p);
            true
        });
        assert_eq!(probed, vec![DEFAULT_PORT + 2]);

        assert_eq!(next_free_port(&none, |_| false), None);
    }

    #[test]
    fn used_ports_come_from_installed_sites_env() {
        let th = TempHome::new();
        let uh = th.path();
        let r = two_site_registry(uh);
        install_legacy(uh); // default port
        let blog = r.get("blog").unwrap().stack();
        blog.ensure_scaffold(&crate::stack::InstallOptions {
            http_port: Some(41250),
            ..Default::default()
        })
        .unwrap();
        // `shop` is registered but never installed: claims nothing.
        assert_eq!(r.used_ports(), [DEFAULT_PORT, 41250].into());
        assert_eq!(
            next_free_port(&r.used_ports(), |_| true),
            Some(DEFAULT_PORT + 1)
        );
    }

    #[test]
    fn adoption_registers_legacy_in_place_with_project_atelier() {
        let th = TempHome::new();
        let uh = th.path();
        let legacy = install_legacy(uh);

        let adopted = adopt_legacy(uh).unwrap().expect("adopted");
        assert_eq!(adopted, legacy_site(uh));
        assert_eq!(adopted.home, legacy.home);
        assert_eq!(adopted.project, legacy.project_name());

        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some(DEFAULT_SLUG));
        assert_eq!(r.sites, vec![legacy_site(uh)]);

        // Idempotent: a second run changes nothing, byte for byte.
        let before = std::fs::read(registry_path(uh)).unwrap();
        assert_eq!(adopt_legacy(uh).unwrap(), None);
        assert_eq!(std::fs::read(registry_path(uh)).unwrap(), before);

        // And the default resolution still lands on the same dir and project.
        let got = resolve(None, None, Some(&r), Some(uh)).unwrap();
        assert_eq!(got.home, legacy.home);
        assert_eq!(got.project(), "atelier");
    }

    #[test]
    fn adoption_leaves_the_stack_directory_untouched() {
        let th = TempHome::new();
        let uh = th.path();
        let legacy = install_legacy(uh);
        let before = snapshot(&legacy.home);

        adopt_legacy(uh).unwrap().expect("adopted");

        let mut after = snapshot(&legacy.home);
        assert!(after.remove(Path::new(SITES_FILE)).is_some());
        assert_eq!(after, before, "adoption may only add sites.toml");
        assert!(!default_home(uh).join(SITES_DIR).exists());
    }

    #[test]
    fn adoption_without_a_legacy_install_does_nothing() {
        let th = TempHome::new();
        let uh = th.path();
        assert_eq!(adopt_legacy(uh).unwrap(), None);
        assert!(!default_home(uh).exists(), "must not create ~/.atelier");
    }

    #[test]
    fn adoption_joins_an_existing_registry_without_stealing_active() {
        let th = TempHome::new();
        let uh = th.path();
        let r = Registry {
            active: Some("blog".to_string()),
            sites: vec![site(uh, "blog")],
        };
        r.save(uh).unwrap();
        install_legacy(uh);

        adopt_legacy(uh).unwrap().expect("adopted");
        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some("blog"));
        assert_eq!(r.sites, vec![legacy_site(uh), site(uh, "blog")]);
    }

    // --- `atelier sites …` ---------------------------------------------------

    fn all_free(_: u16) -> bool {
        true
    }

    #[test]
    fn add_creates_a_pinned_site_on_a_free_port_and_adopts_legacy_first() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh); // on DEFAULT_PORT, unregistered
        let busy = [DEFAULT_PORT + 1];

        let added = add_site(uh, "blog", Some("  The Blog "), None, |p| {
            !busy.contains(&p)
        })
        .unwrap();
        assert_eq!(added.site.slug, "blog");
        assert_eq!(added.site.label, "The Blog");
        assert_eq!(added.site.home, site_home(uh, "blog"));
        assert_eq!(added.site.project, "atelier-blog");
        // DEFAULT_PORT is the legacy site's, +1 is busy on the host.
        assert_eq!(added.port, DEFAULT_PORT + 2);
        assert!(!added.reregistered);
        assert!(!added.activated, "adoption made `default` active first");

        let stack = added.site.stack();
        assert!(stack.exists());
        assert_eq!(stack.http_port(), DEFAULT_PORT + 2);
        assert_eq!(stack.project_name(), "atelier-blog");

        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some(DEFAULT_SLUG));
        assert_eq!(r.sites[0], legacy_site(uh), "legacy adopted in place");
        assert_eq!(r.sites[1], added.site);

        // A second site gets the next port, and the label defaults to the slug.
        let shop = add_site(uh, "shop", None, None, all_free).unwrap();
        assert_eq!(shop.port, DEFAULT_PORT + 1);
        assert_eq!(shop.site.label, "shop");
    }

    #[test]
    fn add_on_a_fresh_machine_activates_the_first_site() {
        let th = TempHome::new();
        let uh = th.path();
        let added = add_site(uh, "blog", None, None, all_free).unwrap();
        assert!(added.activated);
        assert_eq!(added.port, DEFAULT_PORT);
        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some("blog"));
        assert_eq!(r.sites, vec![added.site]);
        // ~/.atelier itself is NOT a stack — only the registry lives there.
        assert!(!Stack::at(default_home(uh)).exists());
    }

    #[test]
    fn add_refuses_bad_input_without_touching_disk() {
        let th = TempHome::new();
        let uh = th.path();
        assert!(add_site(uh, "Bad Name", None, None, all_free).is_err());
        assert!(!default_home(uh).exists());

        add_site(uh, "blog", None, Some(41500), all_free).unwrap();
        let before = std::fs::read(registry_path(uh)).unwrap();
        // Same slug twice.
        assert!(add_site(uh, "blog", None, None, all_free).is_err());
        // An explicit port another site claims, or the host has taken.
        assert!(add_site(uh, "shop", None, Some(41500), all_free).is_err());
        assert!(add_site(uh, "shop", None, Some(41501), |_| false).is_err());
        assert!(!site_home(uh, "shop").exists());
        // A non-empty directory that isn't a stack is left alone.
        let stray = site_home(uh, "notes");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("todo.txt"), b"mine").unwrap();
        assert!(add_site(uh, "notes", None, None, all_free).is_err());
        assert_eq!(std::fs::read(stray.join("todo.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read_dir(&stray).unwrap().count(), 1);

        assert_eq!(std::fs::read(registry_path(uh)).unwrap(), before);
    }

    #[test]
    fn use_sets_the_active_site() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        add_site(uh, "blog", None, None, all_free).unwrap();

        let got = use_site(uh, "blog").unwrap();
        assert_eq!(got.slug, "blog");
        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some("blog"));

        assert!(use_site(uh, "nope").is_err());
        let r2 = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r2, r);
    }

    #[test]
    fn use_adopts_legacy_before_its_first_write() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        // No sites.toml yet: `default` is only a preview…
        assert!(!registry_path(uh).exists());
        // …and using it writes the adopted entry, in place.
        use_site(uh, DEFAULT_SLUG).unwrap();
        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.sites, vec![legacy_site(uh)]);
        assert_eq!(r.active.as_deref(), Some(DEFAULT_SLUG));
    }

    #[test]
    fn rename_changes_only_the_label() {
        let th = TempHome::new();
        let uh = th.path();
        let added = add_site(uh, "blog", Some("Blog"), None, all_free).unwrap();
        let before = snapshot(&added.site.home);

        let renamed = rename_site(uh, "blog", "The Blog").unwrap();
        assert_eq!(renamed.label, "The Blog");
        assert_eq!(renamed.slug, "blog");
        assert_eq!(renamed.home, added.site.home, "home must not move");
        assert_eq!(renamed.project, "atelier-blog", "project must stay pinned");

        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.get("blog"), Some(&renamed));
        assert_eq!(
            r.get("blog").unwrap().stack().project_name(),
            "atelier-blog"
        );
        assert_eq!(snapshot(&added.site.home), before, "stack files untouched");
        assert!(site_home(uh, "blog").is_dir());

        assert!(rename_site(uh, "blog", "   ").is_err());
        assert!(rename_site(uh, "nope", "X").is_err());
    }

    #[test]
    fn remove_unregisters_and_leaves_the_directory_byte_identical() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        let added = add_site(uh, "blog", None, None, all_free).unwrap();
        use_site(uh, "blog").unwrap();
        let home = added.site.home.clone();
        std::fs::create_dir_all(home.join("backups")).unwrap();
        std::fs::write(home.join("backups/aincient-1.tar.gz"), b"precious").unwrap();
        let before = snapshot(&home);
        let legacy_before = {
            let mut s = snapshot(&default_home(uh));
            s.retain(|p, _| !p.starts_with(SITES_DIR) && p != Path::new(SITES_FILE));
            s
        };

        let removed = remove_site(uh, "blog").unwrap();
        assert_eq!(removed.site, added.site);
        assert!(removed.was_active);

        assert_eq!(snapshot(&home), before, "remove must not touch the stack");
        let mut legacy_after = snapshot(&default_home(uh));
        legacy_after.retain(|p, _| !p.starts_with(SITES_DIR) && p != Path::new(SITES_FILE));
        assert_eq!(legacy_after, legacy_before);

        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.get("blog"), None);
        assert_eq!(r.active, None);
        // Nothing active: resolution falls back to ~/.atelier.
        let got = resolve(None, None, Some(&r), Some(uh)).unwrap();
        assert_eq!(got.home, default_home(uh));

        // The site at ~/.atelier can't be removed (adoption would bring it back).
        assert!(remove_site(uh, DEFAULT_SLUG).is_err());
        assert!(remove_site(uh, "blog").is_err(), "already gone");
    }

    #[test]
    fn a_removed_site_is_re_added_in_place_with_its_port() {
        let th = TempHome::new();
        let uh = th.path();
        let first = add_site(uh, "blog", None, Some(41290), all_free).unwrap();
        let salt = first.site.stack().env_get("HASH_SALT").unwrap();
        remove_site(uh, "blog").unwrap();

        // Even with the host port taken (its own container could hold it).
        let again = add_site(uh, "blog", None, None, |_| false).unwrap();
        assert!(again.reregistered);
        assert_eq!(again.port, 41290);
        assert_eq!(again.site, first.site);
        assert_eq!(again.site.stack().env_get("HASH_SALT").unwrap(), salt);
    }

    #[test]
    fn list_shows_sites_with_ports_and_the_unadopted_legacy() {
        let th = TempHome::new();
        let uh = th.path();
        assert_eq!(list_sites(uh).unwrap(), Vec::new());

        install_legacy(uh);
        let rows = list_sites(uh).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, DEFAULT_SLUG);
        assert_eq!(rows[0].project, DEFAULT_PROJECT);
        assert_eq!(rows[0].port, Some(DEFAULT_PORT));
        assert!(rows[0].installed && rows[0].active);
        assert!(!registry_path(uh).exists(), "list must not write");

        add_site(uh, "blog", Some("Blog"), None, all_free).unwrap();
        let rows = list_sites(uh).unwrap();
        let slugs: Vec<_> = rows.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(slugs, ["default", "blog"]);
        assert_eq!(rows[1].label, "Blog");
        assert_eq!(rows[1].port, Some(DEFAULT_PORT + 1));
        assert!(rows[1].image.is_some());
        assert!(!rows[1].active);
    }

    /// The Manager GUI's path (DECISIONS 0475 · M1): it picks its site once with
    /// `locate_in(None, …)`, then resolves every command by that slug. On a
    /// single legacy install both must land on today's stack and project, and
    /// nothing may write `sites.toml` or touch the site.
    #[test]
    fn gui_selection_of_a_single_legacy_site_is_todays_stack_and_writes_nothing() {
        let th = TempHome::new();
        let uh = th.path();
        let legacy = install_legacy(uh);
        let before = snapshot(&default_home(uh));

        let started = locate_in(None, None, Some(uh)).unwrap();
        let slug = started.site.as_ref().map(|s| s.slug.clone());
        assert_eq!(slug.as_deref(), Some(DEFAULT_SLUG));
        let per_command = locate_in(slug.as_deref(), None, Some(uh))
            .unwrap()
            .into_stack();

        assert_eq!(per_command.home, legacy.home);
        assert_eq!(per_command.project_name(), legacy.project_name());
        assert_eq!(per_command.project_name(), DEFAULT_PROJECT);
        assert!(!registry_path(uh).exists(), "selection must not write");
        assert_eq!(snapshot(&default_home(uh)), before);
    }

    #[test]
    fn site_flag_precedence_end_to_end() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        // Before any write, `--site default` already resolves (preview).
        let got = locate_in(Some(DEFAULT_SLUG), None, Some(uh)).unwrap();
        assert_eq!(
            (got.source, got.home.clone()),
            (Source::Flag, default_home(uh))
        );
        assert_eq!(got.project(), "atelier");

        add_site(uh, "blog", None, None, all_free).unwrap();
        add_site(uh, "shop", None, None, all_free).unwrap();
        use_site(uh, "shop").unwrap();
        let hand_rolled = uh.join("elsewhere");

        // --site beats ATELIER_HOME and the active site.
        let got = locate_in(Some("blog"), Some(&hand_rolled), Some(uh)).unwrap();
        assert_eq!(got.source, Source::Flag);
        assert_eq!(got.home, site_home(uh, "blog"));
        let stack = got.into_stack();
        assert_eq!(stack.project_name(), "atelier-blog");
        assert_eq!(stack.slug.as_deref(), Some("blog"));
        assert_eq!(stack.http_port(), DEFAULT_PORT + 1);

        // ATELIER_HOME beats the active site; an unregistered one stays derived.
        let got = locate_in(None, Some(&hand_rolled), Some(uh)).unwrap();
        assert_eq!((got.source, got.site.clone()), (Source::Env, None));
        assert_eq!(got.project(), "elsewhere");

        // The active site beats the ~/.atelier default.
        let got = locate_in(None, None, Some(uh)).unwrap();
        assert_eq!(got.source, Source::Active);
        assert_eq!(got.project(), "atelier-shop");

        // An unknown --site is an error, never a fall-through.
        assert!(locate_in(Some("nope"), None, Some(uh)).is_err());
        assert!(locate_in(Some("nope"), Some(&hand_rolled), Some(uh)).is_err());
    }

    #[test]
    fn install_allocates_only_for_an_unconfigured_registry_site() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh); // claims DEFAULT_PORT
        adopt_legacy(uh).unwrap();
        let mut r = Registry::load(uh).unwrap().unwrap();
        // A registered site with no .env yet (e.g. registry edited by hand).
        r.sites.push(site(uh, "blog"));
        r.save(uh).unwrap();

        let blog = r.get("blog").unwrap().stack();
        assert_eq!(
            port_for_install(&blog, None, Some(&r), Some(uh), all_free),
            Some(DEFAULT_PORT + 1)
        );
        assert_eq!(
            port_for_install(&blog, Some(41400), Some(&r), Some(uh), all_free),
            Some(41400)
        );
        // ~/.atelier keeps meaning the default; a configured stack keeps its port.
        let legacy = r.get(DEFAULT_SLUG).unwrap().stack();
        assert_eq!(
            port_for_install(&legacy, None, Some(&r), Some(uh), all_free),
            None
        );
        blog.ensure_scaffold(&crate::stack::InstallOptions {
            http_port: Some(41250),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            port_for_install(&blog, None, Some(&r), Some(uh), all_free),
            None
        );
        // An unregistered hand-rolled home is today's behaviour.
        let hand = Stack::at(uh.join("hand"));
        assert_eq!(
            port_for_install(&hand, None, Some(&r), Some(uh), all_free),
            None
        );
    }

    #[test]
    fn port_claimant_names_another_site_on_the_same_port() {
        let th = TempHome::new();
        let uh = th.path();
        let blog = add_site(uh, "blog", None, Some(41260), all_free).unwrap();
        let r = Registry::load(uh).unwrap().unwrap();
        let shop = site_home(uh, "shop");
        assert_eq!(
            r.port_claimant(41260, &shop).map(|s| s.slug.as_str()),
            Some("blog")
        );
        assert_eq!(r.port_claimant(41260, &blog.site.home), None, "not itself");
        assert_eq!(r.port_claimant(41261, &shop), None);
    }

    fn remote(uh: &Path, slug: &str, host: &str) -> Site {
        Site::new_remote(uh, slug, None, host, None, KeyMode::Ssh)
    }

    #[test]
    fn a_local_only_file_round_trips_byte_for_byte() {
        // What the M1 CLI writes: no `host` anywhere. Adding the remote fields
        // must not change a single byte of it (D-06).
        let th = TempHome::new();
        let uh = th.path();
        let text = format!(
            "{FILE_HEADER}active = \"blog\"\n\n\
             [[site]]\nslug = \"default\"\nlabel = \"My Site\"\nhome = \"~/.atelier\"\nproject = \"atelier\"\n\n\
             [[site]]\nslug = \"blog\"\nlabel = \"Blog\"\nhome = \"~/.atelier/sites/blog\"\nproject = \"atelier-blog\"\n"
        );
        std::fs::create_dir_all(default_home(uh)).unwrap();
        std::fs::write(registry_path(uh), &text).unwrap();
        let r = Registry::load(uh).unwrap().unwrap();
        assert!(r.sites.iter().all(|s| s.remote.is_none()));
        assert_eq!(r.to_toml(uh).unwrap(), text);
        r.save(uh).unwrap();
        assert_eq!(std::fs::read_to_string(registry_path(uh)).unwrap(), text);
    }

    #[test]
    fn a_remote_site_round_trips_without_home_or_project() {
        let uh = Path::new("/Users/someone");
        let text = r#"
[[site]]
slug = "default"
label = "My Site"
home = "~/.atelier"
project = "atelier"

[[site]]
slug = "prod"
label = "Production"
host = "deploy@example.com"
remote_site = "default"
key = "guided"

[[site]]
slug = "stage"
label = "Stage"
host = "stage-alias"
"#;
        let r = Registry::parse(text, uh).unwrap();
        let prod = r.get("prod").unwrap();
        assert_eq!(
            prod.remote,
            Some(Remote {
                host: "deploy@example.com".to_string(),
                slug: "default".to_string(),
                key: KeyMode::Guided,
                domain: None,
            })
        );
        assert_eq!(prod.project, "");
        assert_eq!(prod.home, remote_placeholder_home(uh, "prod"));
        // remote_site defaults to the local slug, key to the user's ssh setup.
        let stage = r.get("stage").unwrap().remote.clone().unwrap();
        assert_eq!((stage.slug.as_str(), stage.key), ("stage", KeyMode::Ssh));

        let out = r.to_toml(uh).unwrap();
        assert!(
            out.contains(
                "host = \"deploy@example.com\"\nremote_site = \"default\"\nkey = \"guided\""
            ),
            "{out}"
        );
        assert!(
            !out.contains("remote/prod"),
            "no placeholder home is written: {out}"
        );
        assert_eq!(Registry::parse(&out, uh).unwrap(), r);
    }

    #[test]
    fn remote_entries_are_validated() {
        let uh = Path::new("/Users/someone");
        let bad = [
            // remote with a local home
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\nhome = \"~/x\"\n",
            // remote fields without a host
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhome = \"~/x\"\nproject = \"p\"\nremote_site = \"b\"\n",
            // a host ssh would read as an option
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"-oProxyCommand=evil\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h; rm -rf /\"\n",
            // bad slug on the server
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\nremote_site = \"No Way\"\n",
            // unknown key mode
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\nkey = \"password\"\n",
            // two entries for the same site on the same server
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\n\n[[site]]\nslug = \"b\"\nlabel = \"B\"\nhost = \"h\"\nremote_site = \"a\"\n",
            // a local site still needs both
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhome = \"~/x\"\n",
            // a domain on a local site, or one that is not a bare base
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhome = \"~/x\"\nproject = \"p\"\ndomain = \"x.com\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\ndomain = \"x.com/atelier\"\n",
            "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h\"\ndomain = \"https://u:p@x.com\"\n",
        ];
        for text in bad {
            assert!(Registry::parse(text, uh).is_err(), "accepted:\n{text}");
        }
        // The same remote slug on two servers is fine.
        let ok = "[[site]]\nslug = \"a\"\nlabel = \"A\"\nhost = \"h1\"\nremote_site = \"x\"\n\n\
                  [[site]]\nslug = \"b\"\nlabel = \"B\"\nhost = \"h2\"\nremote_site = \"x\"\n";
        Registry::parse(ok, uh).unwrap();
    }

    #[test]
    fn a_recorded_domain_round_trips() {
        let uh = Path::new("/Users/someone");
        let text = "[[site]]\nslug = \"prod\"\nlabel = \"Prod\"\nhost = \"h\"\n\
                    domain = \"cms.example.com\"\n";
        let r = Registry::parse(text, uh).unwrap();
        let remote = r.get("prod").unwrap().remote.clone().unwrap();
        assert_eq!(remote.domain.as_deref(), Some("cms.example.com"));
        let out = r.to_toml(uh).unwrap();
        assert!(out.contains("domain = \"cms.example.com\""), "{out}");
        assert_eq!(Registry::parse(&out, uh).unwrap(), r);
        for ok in [
            "cms.example.com",
            "https://cms.example.com",
            "http://10.0.0.5:8080/",
        ] {
            assert!(is_valid_domain(ok), "{ok}");
        }
    }

    #[test]
    fn tunnel_ports_never_go_to_a_local_site() {
        // Every port below the tunnel range taken: the next local site skips
        // the whole range rather than landing in it.
        let used: BTreeSet<u16> = (DEFAULT_PORT..*TUNNEL_PORTS.start()).collect();
        assert_eq!(
            next_free_port(&used, |_| true),
            Some(TUNNEL_PORTS.end() + 1)
        );
        // Nothing the local allocator can return is a tunnel port, whatever is busy.
        for busy_below in [0u16, 10, 79] {
            let used: BTreeSet<u16> = (DEFAULT_PORT..DEFAULT_PORT + busy_below).collect();
            let port = next_free_port(&used, |p| p % 3 != 0).unwrap();
            assert!(!TUNNEL_PORTS.contains(&port), "{port}");
        }
        // Tunnels pick the first free port of their own range.
        assert_eq!(next_tunnel_port(|_| true), Some(*TUNNEL_PORTS.start()));
        assert_eq!(
            next_tunnel_port(|p| p > TUNNEL_PORTS.start() + 1),
            Some(TUNNEL_PORTS.start() + 2)
        );
        assert_eq!(next_tunnel_port(|_| false), None);
        assert!(!TUNNEL_PORTS.contains(&DEFAULT_PORT));
    }

    #[test]
    fn sites_add_refuses_a_tunnel_port() {
        let th = TempHome::new();
        let err = add_site(th.path(), "blog", None, Some(41300), |_| true).unwrap_err();
        assert!(err.to_string().contains("SSH tunnels"), "{err}");
    }

    #[test]
    fn remote_sites_claim_no_home_or_port() {
        let th = TempHome::new();
        let uh = th.path();
        let r = Registry {
            active: None,
            sites: vec![remote(uh, "prod", "example.com")],
        };
        assert!(r.used_ports().is_empty());
        assert_eq!(r.port_claimant(DEFAULT_PORT, &default_home(uh)), None);
        assert_eq!(r.find_by_home(&remote_placeholder_home(uh, "prod")), None);
        assert_eq!(r.local_sites().count(), 0);
    }

    #[test]
    fn add_remote_writes_nothing_when_verification_fails() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        let before = snapshot(uh);
        let err = add_remote_site(uh, remote(uh, "prod", "example.com"), |_| {
            bail!("example.com did not answer")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("did not answer"));
        assert_eq!(snapshot(uh), before, "not even legacy adoption happened");
        assert!(!registry_path(uh).exists());
    }

    #[test]
    fn add_remote_registers_after_verification_and_adopts_legacy() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        let mut seen = None;
        let added = add_remote_site(uh, remote(uh, "prod", "example.com"), |s| {
            seen = Some(s.slug.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.as_deref(), Some("prod"));
        assert!(
            !added.activated,
            "legacy was adopted first and stays active"
        );
        let r = Registry::load(uh).unwrap().unwrap();
        assert_eq!(r.active.as_deref(), Some(DEFAULT_SLUG));
        assert_eq!(r.sites.len(), 2);
        assert!(r.get("prod").unwrap().is_remote());
        assert!(!remote_placeholder_home(uh, "prod").exists());

        let rows = list_sites(uh).unwrap();
        let prod = rows.iter().find(|r| r.slug == "prod").unwrap();
        assert_eq!(prod.host.as_deref(), Some("example.com"));
        assert_eq!(prod.remote_site.as_deref(), Some("prod"));
        assert!(!prod.installed);
        assert_eq!(prod.port, None);

        // Taken slug, and a duplicate server target, are refused before verify.
        let dup = add_remote_site(uh, remote(uh, "prod", "other"), |_| unreachable!());
        assert!(dup.is_err());
        let same_target =
            Site::new_remote(uh, "prod2", None, "example.com", Some("prod"), KeyMode::Ssh);
        assert!(add_remote_site(uh, same_target, |_| unreachable!()).is_err());

        // Removing it touches nothing on disk but sites.toml.
        remove_site(uh, "prod").unwrap();
        assert!(Registry::load(uh).unwrap().unwrap().get("prod").is_none());
    }

    #[test]
    fn set_domain_records_and_clears_a_remote_sites_domain_only() {
        let th = TempHome::new();
        let uh = th.path();
        install_legacy(uh);
        add_remote_site(uh, remote(uh, "prod", "example.com"), |_| Ok(())).unwrap();

        let site = set_domain(uh, "prod", Some("cms.example.com")).unwrap();
        assert_eq!(
            site.remote.unwrap().domain.as_deref(),
            Some("cms.example.com")
        );
        let saved = Registry::load(uh).unwrap().unwrap();
        let remote = saved.get("prod").unwrap().remote.clone().unwrap();
        assert_eq!(remote.domain.as_deref(), Some("cms.example.com"));

        assert!(set_domain(uh, "prod", Some("not a domain")).is_err());
        assert!(set_domain(uh, DEFAULT_SLUG, Some("x.example.com")).is_err());
        assert!(set_domain(uh, "nope", None).is_err());

        set_domain(uh, "prod", None).unwrap();
        let saved = Registry::load(uh).unwrap().unwrap();
        assert_eq!(
            saved.get("prod").unwrap().remote.as_ref().unwrap().domain,
            None
        );
    }
}
