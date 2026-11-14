//! Free disk space Atelier used for things it can download again (DECISIONS 0527).
//!
//! Owner, 2026-11-14: "it is ok to keep user data. but anything that can be
//! downloaded later, user should be able to clean up". Every update leaves the
//! previous `atelier-cms` image behind as an untagged leftover (~420 MB), and a
//! database or web-server move leaves the old image; nothing removed them.
//!
//! Only Atelier's own images (and the database / web-server images it moved
//! off) are candidates, and only those no container (in any state) uses, no
//! registered site needs to start, and that can be downloaded again. Never
//! `docker image prune`: it also takes other tools' images. Data volumes and
//! backups are never touched.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::docker;
use crate::engine::{pinned_tag, ENGINE_IMAGE_REPO, IMAGE_ENV};
use crate::stack::{edge_nginx_upgrade, Stack, IMAGE_REPO, LEGACY_DB_IMAGE, LEGACY_IMAGE_REPO};

/// How much to free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// After a successful update: old untagged Atelier versions and the database
    /// and web-server images Atelier has moved away from.
    AfterUpdate,
    /// On request: every unused Atelier image no registered site names, and the
    /// database and web-server images Atelier has moved away from.
    Everything,
}

/// One image that can go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reclaimable {
    pub id: String,
    /// `repo:tag` names, or empty for an untagged leftover.
    pub names: Vec<String>,
    /// The repository, for a readable label when there's no tag.
    pub repo: String,
    pub size_bytes: u64,
}

/// What a clean-up found or removed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub images: Vec<Reclaimable>,
    /// Sum of the images' sizes. Images share layers, so the space actually
    /// freed can be less.
    pub total_bytes: u64,
    /// Images that wouldn't go (in use since the list was made, Docker refused).
    pub failed: Vec<String>,
}

/// One `docker image ls --digests` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: String,
    repo: String,
    tag: String,
    /// `sha256:…`, or empty / `<none>` for an image never pulled from a
    /// registry under this repository.
    digest: String,
}

fn is_none(s: &str) -> bool {
    s.is_empty() || s == "<none>"
}

impl Row {
    fn untagged(&self) -> bool {
        is_none(&self.tag) || is_none(&self.repo)
    }
    fn name(&self) -> String {
        format!("{}:{}", self.repo, self.tag)
    }
    fn by_digest(&self) -> Option<String> {
        (!is_none(&self.digest) && !is_none(&self.repo))
            .then(|| format!("{}@{}", self.repo, self.digest))
    }
}

fn is_atelier_repo(repo: &str) -> bool {
    repo == IMAGE_REPO || repo == LEGACY_IMAGE_REPO || repo == ENGINE_IMAGE_REPO
}

/// A database or web-server image Atelier used before and has moved its sites off.
fn is_superseded_base(name: &str) -> bool {
    name == LEGACY_DB_IMAGE || edge_nginx_upgrade(name).is_some()
}

/// The images `scope` may remove, grouped by image ID (an image with two tags is
/// one entry). An image qualifies when:
///
/// - no container uses it (`in_use`) and no registered site needs it
///   (`needed`: by ID, `repo:tag` or `repo@digest`, see [`needed_images`]);
/// - it can be downloaded again: at least one row has a registry digest. A
///   local or fork build has none, and removing it would lose it for good;
/// - every name it has qualifies: an Atelier image (only an untagged one after
///   an update), or a database / web-server image Atelier has moved its sites
///   off ([`is_superseded_base`]). The current `nginx` and `pgvector` images
///   are generic and other projects may use them, so they always stay.
fn select(
    rows: &[Row],
    in_use: &HashSet<String>,
    needed: &HashSet<String>,
    scope: Scope,
) -> Vec<(String, String, Vec<String>)> {
    let qualifies = |r: &Row| -> bool {
        if is_atelier_repo(&r.repo) {
            return match scope {
                Scope::AfterUpdate => r.untagged(),
                Scope::Everything => true,
            };
        }
        !r.untagged() && is_superseded_base(&r.name())
    };
    let is_needed = |r: &Row| -> bool {
        needed.contains(&r.id)
            || (!r.untagged() && needed.contains(&r.name()))
            || r.by_digest().is_some_and(|d| needed.contains(&d))
    };
    let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut seen = HashSet::new();
    for id in rows.iter().map(|r| r.id.as_str()) {
        if !seen.insert(id) || in_use.contains(id) {
            continue;
        }
        let same: Vec<&Row> = rows.iter().filter(|r| r.id == id).collect();
        if same.iter().any(|r| is_needed(r))
            || !same.iter().any(|r| r.by_digest().is_some())
            || !same.iter().all(|r| qualifies(r))
        {
            continue;
        }
        let mut names: Vec<String> = same
            .iter()
            .filter(|r| !r.untagged())
            .map(|r| r.name())
            .collect();
        names.dedup();
        out.push((id.to_string(), same[0].repo.clone(), names));
    }
    out
}

fn parse_rows(out: &str) -> Vec<Row> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (id, repo, tag) = (f.next()?.trim(), f.next()?.trim(), f.next()?.trim());
            let digest = f.next().unwrap_or("").trim();
            (!id.is_empty()).then(|| Row {
                id: id.to_string(),
                repo: repo.to_string(),
                tag: tag.to_string(),
                digest: digest.to_string(),
            })
        })
        .collect()
}

/// The names `docker image ls` shows for an image reference, so a site's
/// reference and a listed row compare equal: Docker Hub's implied host and
/// `library/` dropped, the implied `:latest` added. `repo:tag@digest` gives
/// both forms; a bare image ID (`sha256:…`) stays as it is.
fn ref_names(reference: &str) -> Vec<String> {
    let r = reference.trim().trim_matches(|c| c == '"' || c == '\'');
    if r.is_empty() {
        return Vec::new();
    }
    if r.starts_with("sha256:") {
        return vec![r.to_string()];
    }
    let (name, digest) = match r.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (r, None),
    };
    let name = name
        .strip_prefix("docker.io/")
        .or_else(|| name.strip_prefix("index.docker.io/"))
        .unwrap_or(name);
    let name = name.strip_prefix("library/").unwrap_or(name);
    let last = name.rsplit('/').next().unwrap_or(name);
    let (repo, tag) = match last.rfind(':') {
        Some(i) => {
            let at = name.len() - last.len() + i;
            (&name[..at], Some(&name[at + 1..]))
        }
        None => (name, None),
    };
    let mut out = Vec::new();
    if let Some(d) = digest {
        out.push(format!("{repo}@{d}"));
    }
    match tag {
        Some(t) => out.push(format!("{repo}:{t}")),
        None if digest.is_none() => out.push(format!("{repo}:latest")),
        None => {}
    }
    out
}

/// A compose value with `${VAR}`, `${VAR:-default}`, `${VAR-default}` and
/// `${VAR:?message}` filled in from `.env`, as Compose fills them. `None` when a
/// variable is unset and has no default.
fn interpolate(value: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let inner = &after[..end];
        let var_len = inner
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(inner.len());
        let (var, op) = inner.split_at(var_len);
        let set = env.get(var).filter(|v| !v.is_empty());
        let unset_but_present = env.get(var).is_some_and(|v| v.is_empty());
        let filled = if let Some(d) = op.strip_prefix(":-") {
            set.cloned().unwrap_or_else(|| d.to_string())
        } else if let Some(d) = op.strip_prefix('-') {
            if unset_but_present {
                String::new()
            } else {
                set.cloned().unwrap_or_else(|| d.to_string())
            }
        } else {
            set.cloned()?
        };
        out.push_str(&filled);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The image references a compose file's services run, with `.env` filled in.
fn compose_images(text: &str, env: &BTreeMap<String, String>) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("image:"))
        .map(|v| v.split(" #").next().unwrap_or(v).trim())
        .map(|v| v.trim_matches(|c| c == '"' || c == '\''))
        .filter_map(|v| interpolate(v, env))
        .filter(|v| !v.is_empty())
        .collect()
}

/// Every image `stack` needs to start without a download: its `.env`'s
/// `AINCIENT_IMAGE` and engine pin, the images its compose files name, and the
/// local name that keeps its engine pin ([`pinned_tag`]).
fn stack_needs(stack: &Stack) -> Vec<String> {
    let env = stack.read_env();
    let mut refs = vec![stack.image()];
    refs.extend(env.get(IMAGE_ENV).filter(|v| !v.is_empty()).cloned());
    for file in [
        stack.compose_path(),
        crate::engine::compose_path(stack),
        stack.home.join("compose.override.yaml"),
        stack.home.join("compose.override.yml"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&file) {
            refs.extend(compose_images(&text, &env));
        }
    }
    refs.push(pinned_tag(&stack.project_name()));
    refs
}

/// The images every site registered on this computer needs (the registry plus
/// an unadopted `~/.atelier`), as the names `docker image ls` shows them. A
/// stopped or removed site, a digest-pinned one (listed as `<none>`) and an
/// engine pin are all covered: clean-up must never make a site download its
/// own image to start, or lose an image it can't download at all.
pub fn needed_images(user_home: &Path) -> Result<HashSet<String>> {
    let registry = crate::sites::load_effective(user_home)
        .context("couldn't read the list of sites, so nothing is safe to remove")?;
    let mut needed = HashSet::new();
    for site in registry.iter().flat_map(|r| &r.sites) {
        if site.is_remote() {
            continue;
        }
        for r in stack_needs(&site.stack()) {
            needed.extend(ref_names(&r));
        }
    }
    Ok(needed)
}

/// Image IDs every container on this machine uses, in any state.
fn images_in_use() -> Option<HashSet<String>> {
    let mut ps = docker::docker();
    ps.args(["ps", "-aq", "--no-trunc"]);
    let ids = docker::try_capture_empty_ok(ps)?;
    let ids: Vec<&str> = ids
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if ids.is_empty() {
        return Some(HashSet::new());
    }
    let mut inspect = docker::docker();
    inspect.args(["inspect", "-f", "{{.Image}}"]).args(&ids);
    Some(
        docker::try_capture(inspect)?
            .lines()
            .map(|l| l.trim().to_string())
            .collect(),
    )
}

fn image_size(id: &str) -> u64 {
    let mut c = docker::docker();
    c.args(["image", "inspect", "-f", "{{.Size}}", id]);
    docker::try_capture(c)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

const NO_DOCKER: &str = "couldn't ask Docker which images are in use; is Docker running?";

/// What [`free`] would remove for `scope`.
pub fn find(scope: Scope) -> Result<Report> {
    let user_home = crate::sites::user_home()?;
    let needed = needed_images(&user_home)?;
    let mut ls = docker::docker();
    ls.args([
        "image",
        "ls",
        "--no-trunc",
        "--digests",
        "--format",
        "{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.Digest}}",
    ]);
    let rows = parse_rows(&docker::try_capture_empty_ok(ls).context(NO_DOCKER)?);
    // Without knowing what containers use, nothing is safe to remove.
    let in_use = images_in_use().context(NO_DOCKER)?;
    let images: Vec<Reclaimable> = select(&rows, &in_use, &needed, scope)
        .into_iter()
        .map(|(id, repo, names)| Reclaimable {
            size_bytes: image_size(&id),
            id,
            names,
            repo,
        })
        .collect();
    let total_bytes = images.iter().map(|i| i.size_bytes).sum();
    Ok(Report {
        images,
        total_bytes,
        failed: Vec::new(),
    })
}

/// Remove what [`find`] lists for `scope` now; with `only`, just the images
/// among those IDs (what the owner was shown and agreed to). Each removal is by
/// name (or by ID for an untagged leftover) and without force.
pub fn free(scope: Scope, only: Option<&HashSet<String>>) -> Result<Report> {
    let found = find(scope)?;
    let images = found
        .images
        .into_iter()
        .filter(|i| only.is_none_or(|o| o.contains(&i.id)))
        .collect();
    Ok(remove_each(images, images_in_use, |target| {
        let mut c = docker::docker();
        c.args(["image", "rm", target]);
        docker::try_capture(c).is_some()
    }))
}

/// Remove `images`, asking what containers use right before every `image rm`.
/// Docker refuses to remove an image a container uses only at its *last* name:
/// removing any other name just untags it. So a container that started using an
/// image after [`find`] would otherwise lose the names before the refusal. An
/// image found in use (or when Docker can't say) keeps all its remaining names.
fn remove_each(
    images: Vec<Reclaimable>,
    mut in_use: impl FnMut() -> Option<HashSet<String>>,
    mut rm: impl FnMut(&str) -> bool,
) -> Report {
    let mut report = Report::default();
    for image in images {
        let targets = if image.names.is_empty() {
            vec![image.id.clone()]
        } else {
            image.names.clone()
        };
        let mut ok = true;
        for t in &targets {
            let busy = in_use().is_none_or(|u| u.contains(&image.id));
            if busy || !rm(t) {
                ok = false;
                report.failed.push(t.clone());
                break;
            }
        }
        if ok {
            report.total_bytes += image.size_bytes;
            report.images.push(image);
        }
    }
    report
}

/// Bytes as people read them: "420 MB", "1.2 GB".
pub fn human_bytes(n: u64) -> String {
    const MB: f64 = 1_000_000.0;
    let mb = n as f64 / MB;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{:.0} MB", mb.max(if n > 0 { 1.0 } else { 0.0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{COMPOSE_TEMPLATE, DB_IMAGE, EDGE_IMAGE};

    // Real `docker image ls --no-trunc --digests --format
    // '{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.Digest}}'` lines (Docker 28, 2026-11-14).
    const CMS_LATEST: &str = "sha256:5e54fbab3ec57b6fbc4650b43a2440f9104f6053fc10e4538cf76bab9e7ab041\tghcr.io/aincient-labs/atelier-cms\tlatest\tsha256:0f93c730bf54a025610ea0c4fafb1a80012773152cd22ce5227bd968e6e96bae";
    const ENGINE_PIN: &str = "sha256:bece2626a42ce7764ac03f94358cc1951fe89b5bf48890e51419a22631495110\tghcr.io/aincient-labs/atelier-engine\tpinned-atelier-engine-poc\tsha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f";
    const PG_NOW: &str = "sha256:1a12dd1d7b3f538339c6008b78f814c24f2d03b9211ae02d58b9e255ae0c5994\tpgvector/pgvector\tpg16-trixie\tsha256:c28c0b700f3f1b5eec14082b31ad6e7c8a566d14386323609c615315e7c0ffc8";
    const PG_OLD: &str = "sha256:6be9fa2171cbb8ad81463538d4e2e5f4c57fa5438fd845032b211cd9baad09d0\tpgvector/pgvector\tpg16\tsha256:0dd3149a06e4579a0d70d8fef9945636cd514005e7237deb27f9b67886f8dc4f";
    const NGINX_NOW: &str = "sha256:7e7544160523d3d40f70c5083bb54e9a6eef59a4660c49c67e391852a1ccfda4\tnginx\t1.30-alpine-slim\tsha256:e2355aa95529a7999dad94ddcb45a8768abf6fb0b204c93b1a3ffce477d7b893";
    // The full Alpine flavour edge ran before the slim move: superseded.
    const NGINX_FULL: &str = "sha256:ca1fa2353bc71f0a26fcd84a01385314fb5df4c61598bb57ccd577f7f2cbf460\tnginx\t1.30-alpine\tsha256:901e593c95dc85f5a729f5765516321366ef4ac401821431d408897e95287d0a";
    const NOT_OURS: &str = "sha256:2e7ed4ea03deedbb45c26c68bf2a7090dc68dd8518a9c75928c4b9b0f59bc411\tpostgres\t16\tsha256:63e83b5a3dc36a917dccb76fe1bd2ac51f1ec9c1deb47ff20d4f04233bf6eae9";
    const LOCAL_BUILD: &str = "sha256:e75719a4a4e9bb18c4f644cc8193e441d9a7630470736aefa3d2301d8578b9fd\taincient/cms\tsmoke\t<none>";
    // An earlier cms that lost its tag to a newer pull: Docker lists it as
    // `<none>` but keeps its registry digest.
    const CMS_OLD: &str = "sha256:0c2b9f0f8d1e4a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b\tghcr.io/aincient-labs/atelier-cms\t<none>\tsha256:9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b";

    fn rows(lines: &[&str]) -> Vec<Row> {
        parse_rows(&lines.join("\n"))
    }

    fn ids(v: &[(String, String, Vec<String>)]) -> Vec<String> {
        v.iter().map(|(id, _, _)| id[7..19].to_string()).collect()
    }

    fn short(line: &str) -> String {
        line[7..19].to_string()
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn only_unused_downloadable_atelier_images_and_superseded_bases_go() {
        let all = rows(&[
            CMS_OLD,
            CMS_LATEST,
            ENGINE_PIN,
            PG_NOW,
            PG_OLD,
            NGINX_NOW,
            NGINX_FULL,
            NOT_OURS,
            LOCAL_BUILD,
        ]);
        let after = select(&all, &none(), &none(), Scope::AfterUpdate);
        assert_eq!(
            ids(&after),
            [short(CMS_OLD), short(PG_OLD), short(NGINX_FULL)]
        );
        // On request, every Atelier image too, but never the current generic
        // bases: another project may run them (C3).
        let every = select(&all, &none(), &none(), Scope::Everything);
        assert_eq!(
            ids(&every),
            [
                short(CMS_OLD),
                short(CMS_LATEST),
                short(ENGINE_PIN),
                short(PG_OLD),
                short(NGINX_FULL)
            ]
        );
        // A container (any state) using it keeps it.
        let busy: HashSet<String> = [all[1].id.clone()].into();
        assert!(!ids(&select(&all, &busy, &none(), Scope::Everything)).contains(&short(CMS_LATEST)));
    }

    #[test]
    fn the_current_generic_bases_stay_even_when_unused() {
        let all = rows(&[PG_NOW, NGINX_NOW, NOT_OURS]);
        assert!(select(&all, &none(), &none(), Scope::Everything).is_empty());
    }

    #[test]
    fn an_image_that_cannot_be_downloaded_again_stays() {
        // A local / fork build of Atelier's own repository: no registry digest.
        let local = "sha256:13330c9d25f1fe75d49b49b1d7609abf4570b4082e4aaa2272960f2f167e0c89\tghcr.io/aincient-labs/atelier-cms\tdev\t<none>";
        let engine_dev = "sha256:b2b28d3776b84321c06f85f2caf84db1b119b0e9bab3f893ac69e9b0073b653f\tghcr.io/aincient-labs/atelier-engine\t<none>\t<none>";
        let all = rows(&[local, engine_dev, CMS_OLD]);
        assert_eq!(
            ids(&select(&all, &none(), &none(), Scope::Everything)),
            [short(CMS_OLD)]
        );
    }

    #[test]
    fn an_image_shared_with_a_name_that_isnt_atelier_s_stays() {
        // One image ID tagged both by Atelier and by the owner for something else.
        let shared = PG_OLD.replace("pgvector/pgvector\tpg16", "my-db\tbackup");
        let all = rows(&[PG_OLD, &shared]);
        assert!(select(&all, &none(), &none(), Scope::Everything).is_empty());
    }

    /// A user home with a registry of local sites, each laid down with the
    /// shipped `compose.yaml` and the given `.env`.
    struct TempHome(std::path::PathBuf);
    impl TempHome {
        fn new(sites: &[(&str, &str)]) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "atelier-cleanup-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let mut toml = String::new();
            for (slug, env) in sites {
                let home = crate::sites::site_home(&dir, slug);
                std::fs::create_dir_all(&home).unwrap();
                std::fs::write(home.join("compose.yaml"), COMPOSE_TEMPLATE).unwrap();
                std::fs::write(home.join(".env"), env).unwrap();
                toml.push_str(&format!(
                    "[[site]]\nslug = \"{slug}\"\nlabel = \"{slug}\"\nhome = \"~/.atelier/sites/{slug}\"\nproject = \"atelier-{slug}\"\n\n"
                ));
            }
            std::fs::write(crate::sites::registry_path(&dir), toml).unwrap();
            TempHome(dir)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nothing_a_registered_site_needs_is_taken() {
        // A stopped (or `down`) site on an old version, a digest-pinned site and
        // an engine site: no container runs any of their images.
        let old_tag = "sha256:4f3e2d1c0b9a88776655443322110ffeeddccbbaa99887766554433221100ffe\tghcr.io/aincient-labs/atelier-cms\t0.27.0\tsha256:5f4e3d2c1b0a99887766554433221100ffeeddccbbaa99887766554433221100";
        let digest_pinned = "sha256:8f1e2d3c4b5a69788796a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4\tghcr.io/aincient-labs/atelier-cms\t<none>\tsha256:77aa0f93c730bf54a025610ea0c4fafb1a80012773152cd22ce5227bd968e6e9";
        let local_engine = "sha256:a1d39d1c36521f28e0f455493d0b52db8cb4498dd1e2c186e7e2abb0e2044c7e\tghcr.io/aincient-labs/atelier-engine\t<none>\tsha256:1111111111111111111111111111111111111111111111111111111111111111";
        let th = TempHome::new(&[
            (
                "stopped",
                "HASH_SALT=x\nAINCIENT_IMAGE=ghcr.io/aincient-labs/atelier-cms:0.27.0\nHTTP_PORT=41230\n",
            ),
            (
                "pinned",
                "HASH_SALT=x\nAINCIENT_IMAGE=ghcr.io/aincient-labs/atelier-cms@sha256:77aa0f93c730bf54a025610ea0c4fafb1a80012773152cd22ce5227bd968e6e9\nHTTP_PORT=41231\n",
            ),
            (
                "devengine",
                "HASH_SALT=x\nHTTP_PORT=41232\nATELIER_ENGINE_IMAGE=sha256:a1d39d1c36521f28e0f455493d0b52db8cb4498dd1e2c186e7e2abb0e2044c7e\n",
            ),
        ]);
        let needed = needed_images(&th.0).unwrap();
        let all = rows(&[
            old_tag,
            digest_pinned,
            local_engine,
            ENGINE_PIN,
            CMS_LATEST,
            CMS_OLD,
        ]);
        let every = select(&all, &none(), &needed, Scope::Everything);
        // Only the engine POC's pin (no such site here) and the true leftover go;
        // the engine site's `:latest` cms (its compose default) stays.
        assert_eq!(ids(&every), [short(ENGINE_PIN), short(CMS_OLD)]);
        let after = select(&all, &none(), &needed, Scope::AfterUpdate);
        assert_eq!(ids(&after), [short(CMS_OLD)]);
    }

    #[test]
    fn an_engine_site_keeps_its_pinned_name_and_compose_images() {
        let th = TempHome::new(&[(
            "engine-poc",
            "HASH_SALT=x\nAINCIENT_IMAGE=ghcr.io/aincient-labs/atelier-cms:0.27.0\nATELIER_ENGINE_IMAGE=ghcr.io/aincient-labs/atelier-engine@sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f\n",
        )]);
        let needed = needed_images(&th.0).unwrap();
        for name in [
            "ghcr.io/aincient-labs/atelier-engine:pinned-atelier-engine-poc",
            "ghcr.io/aincient-labs/atelier-engine@sha256:435e2d228cb08ba64cf622149cc6fd03a01c5d01d4242d03e2d81115c78cd41f",
            "ghcr.io/aincient-labs/atelier-cms:0.27.0",
            DB_IMAGE,
            EDGE_IMAGE,
        ] {
            assert!(needed.contains(name), "{name} not in {needed:?}");
        }
        assert!(select(
            &rows(&[ENGINE_PIN, PG_NOW, NGINX_NOW]),
            &none(),
            &needed,
            Scope::Everything
        )
        .is_empty());
    }

    #[test]
    fn references_read_as_docker_lists_them() {
        assert_eq!(ref_names("nginx"), ["nginx:latest"]);
        assert_eq!(
            ref_names("docker.io/library/nginx:1.30-alpine-slim"),
            ["nginx:1.30-alpine-slim"]
        );
        assert_eq!(ref_names("localhost:5000/x"), ["localhost:5000/x:latest"]);
        assert_eq!(
            ref_names("ghcr.io/a/b:1@sha256:ab"),
            ["ghcr.io/a/b@sha256:ab", "ghcr.io/a/b:1"]
        );
        assert_eq!(ref_names("sha256:ab"), ["sha256:ab"]);
        let env: BTreeMap<String, String> = [("AINCIENT_IMAGE".to_string(), String::new())].into();
        assert_eq!(
            compose_images(COMPOSE_TEMPLATE, &env),
            [
                DB_IMAGE,
                "ghcr.io/aincient-labs/atelier-cms:latest",
                EDGE_IMAGE
            ]
        );
        assert_eq!(
            interpolate("${ATELIER_ENGINE_IMAGE:?run atelier engine enable}", &env),
            None
        );
    }

    fn image(id: &str, names: &[&str]) -> Reclaimable {
        Reclaimable {
            id: id.into(),
            names: names.iter().map(|n| n.to_string()).collect(),
            repo: IMAGE_REPO.into(),
            size_bytes: 10,
        }
    }

    #[test]
    fn what_containers_use_is_asked_again_before_every_removal() {
        // In use since `find`: Docker would only untag the first name, so no
        // name may be removed at all.
        let mut removed = Vec::new();
        let r = remove_each(
            vec![image("sha256:a", &["x:1", "x:2"])],
            || Some(["sha256:a".to_string()].into()),
            |t| {
                removed.push(t.to_string());
                true
            },
        );
        assert!(removed.is_empty());
        assert_eq!(r.failed, ["x:1"]);
        assert!(r.images.is_empty());

        // Becomes used between its two names: the second name stays.
        let mut calls = 0;
        let mut removed = Vec::new();
        let r = remove_each(
            vec![image("sha256:a", &["x:1", "x:2"]), image("sha256:b", &[])],
            || {
                calls += 1;
                Some(if calls == 2 {
                    ["sha256:a".to_string()].into()
                } else {
                    HashSet::new()
                })
            },
            |t| {
                removed.push(t.to_string());
                true
            },
        );
        assert_eq!(removed, ["x:1", "sha256:b"]);
        assert_eq!(r.failed, ["x:2"]);
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.total_bytes, 10);

        // Docker can't say: nothing goes.
        let r = remove_each(vec![image("sha256:a", &[])], || None, |_| true);
        assert!(r.images.is_empty());
    }

    #[test]
    fn rows_parse_and_sizes_read_well() {
        let rows = parse_rows(&format!("{CMS_OLD}\n\n{NGINX_NOW}\n"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].tag, "1.30-alpine-slim");
        assert!(rows[0].untagged());
        assert!(rows[0].by_digest().is_some());
        assert_eq!(parse_rows(LOCAL_BUILD)[0].by_digest(), None);
        assert_eq!(human_bytes(420_000_000), "420 MB");
        assert_eq!(human_bytes(1_234_000_000), "1.2 GB");
        assert_eq!(human_bytes(0), "0 MB");
    }
}
