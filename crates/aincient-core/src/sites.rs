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
//! ```
//!
//! **No code path here moves, renames or deletes a stack directory.** A stack's
//! Compose project is derived from its directory name, so relocating one orphans
//! its `db-data` volume — the site comes back empty while the bytes sit on disk
//! under a name nothing reads. Legacy adoption therefore registers `~/.atelier`
//! *where it is*; the only file this module ever writes is `sites.toml` itself,
//! via temp file + rename in the same directory.
//!
//! Resolution order — explicit beats implicit:
//! `--site <slug>` > `ATELIER_HOME` > `sites.toml` `active` > `~/.atelier`.
//! [`resolve`] is a pure function of those inputs; [`locate`] gathers them from
//! the process.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::stack::{Stack, DEFAULT_PORT};

/// The registry's file name, inside the default stack directory.
pub const SITES_FILE: &str = "sites.toml";
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
}

impl Site {
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteEntry {
    slug: String,
    label: String,
    home: String,
    project: String,
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
            let home = expand_home(&entry.home, user_home).with_context(|| {
                format!(
                    "site `{}`: home `{}` is not absolute",
                    entry.slug, entry.home
                )
            })?;
            sites.push(Site {
                slug: entry.slug,
                label: entry.label,
                home,
                project: entry.project,
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
        for site in &self.sites {
            if !is_valid_slug(&site.slug) {
                bail!("`{}` is not a valid site slug (a-z, 0-9, -)", site.slug);
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
                .map(|s| SiteEntry {
                    slug: s.slug.clone(),
                    label: s.label.clone(),
                    home: contract_home(&s.home, user_home),
                    project: s.project.clone(),
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
        self.sites.iter().find(|s| normalize(&s.home) == want)
    }

    /// The active site, if one is set.
    pub fn active_site(&self) -> Option<&Site> {
        self.active.as_deref().and_then(|slug| self.get(slug))
    }

    /// Every console port a registered site has claimed in its `.env`. A site
    /// with no `.env` yet (never installed) claims nothing; one whose `.env`
    /// lacks `HTTP_PORT` runs on — and so claims — [`DEFAULT_PORT`].
    pub fn used_ports(&self) -> BTreeSet<u16> {
        self.sites
            .iter()
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
    let registry = match (&user_home, site_flag, &atelier_home) {
        (None, _, _) => None,
        (Some(uh), None, Some(_)) => Registry::load(uh).ok().flatten(),
        (Some(uh), _, _) => Registry::load(uh)?,
    };
    resolve(
        site_flag,
        atelier_home.as_deref().map(Path::new),
        registry.as_ref(),
        user_home.as_deref(),
    )
}

/// The first port from [`DEFAULT_PORT`] upward that no registered site claims
/// (`used`) and that `is_free` reports bindable on this host. Deterministic in
/// its inputs; `None` only if every port up to 65535 is taken.
///
/// Pair with [`Registry::used_ports`] and [`host_port_is_free`]. Run legacy
/// adoption first, so an installed `~/.atelier`'s port counts as used even
/// while it is stopped.
pub fn next_free_port(used: &BTreeSet<u16>, mut is_free: impl FnMut(u16) -> bool) -> Option<u16> {
    (DEFAULT_PORT..=u16::MAX).find(|port| !used.contains(port) && is_free(*port))
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
    };
    registry.sites.insert(0, site.clone());
    if registry.active.is_none() {
        registry.active = Some(site.slug.clone());
    }
    registry.save(user_home)?;
    Ok(Some(site))
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
        }
    }

    fn legacy_site(user_home: &Path) -> Site {
        Site {
            slug: DEFAULT_SLUG.to_string(),
            label: DEFAULT_LABEL.to_string(),
            home: default_home(user_home),
            project: DEFAULT_PROJECT.to_string(),
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
}
