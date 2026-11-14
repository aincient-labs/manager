//! Free disk space Atelier used for things it can download again (DECISIONS 0527).
//!
//! Owner, 2026-11-14: "it is ok to keep user data. but anything that can be
//! downloaded later, user should be able to clean up". Every update leaves the
//! previous `atelier-cms` image behind as an untagged leftover (~420 MB), and a
//! database or web-server move leaves the old image; nothing removed them.
//!
//! Only Atelier's own images are candidates, and only those no container (in
//! any state) uses. Never `docker image prune`: it also takes other tools'
//! images. Data volumes and backups are never touched.

use std::collections::HashSet;

use serde::Serialize;

use crate::docker;
use crate::engine::ENGINE_IMAGE_REPO;
use crate::stack::{
    edge_nginx_upgrade, DB_IMAGE, EDGE_IMAGE, IMAGE_REPO, LEGACY_DB_IMAGE, LEGACY_IMAGE_REPO,
};

/// How much to free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// After a successful update: old untagged Atelier versions and the database
    /// and web-server images Atelier has moved away from. Nothing a stopped site
    /// would have to download again to start.
    AfterUpdate,
    /// On request: every unused Atelier image. A site whose image goes downloads
    /// it again at its next start.
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

/// One `docker image ls` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: String,
    repo: String,
    tag: String,
}

fn is_atelier_repo(repo: &str) -> bool {
    repo == IMAGE_REPO || repo == LEGACY_IMAGE_REPO || repo == ENGINE_IMAGE_REPO
}

/// A database or web-server image Atelier used before and has moved its sites off.
fn is_superseded_base(name: &str) -> bool {
    name == LEGACY_DB_IMAGE || edge_nginx_upgrade(name).is_some()
}

/// The images `scope` may remove, grouped by image ID (an image with two tags is
/// one entry). A row qualifies when its image is unused and, for an ID with
/// several names, every name qualifies.
fn select(
    rows: &[Row],
    in_use: &HashSet<String>,
    scope: Scope,
) -> Vec<(String, String, Vec<String>)> {
    let qualifies = |r: &Row| -> bool {
        let untagged = r.tag.is_empty() || r.tag == "<none>";
        let name = format!("{}:{}", r.repo, r.tag);
        if is_atelier_repo(&r.repo) {
            return match scope {
                Scope::AfterUpdate => untagged,
                Scope::Everything => true,
            };
        }
        if untagged {
            return false;
        }
        is_superseded_base(&name)
            || (scope == Scope::Everything && (name == DB_IMAGE || name == EDGE_IMAGE))
    };
    let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    ids.dedup();
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) || in_use.contains(id) {
            continue;
        }
        let same: Vec<&Row> = rows.iter().filter(|r| r.id == id).collect();
        if !same.iter().all(|r| qualifies(r)) {
            continue;
        }
        let names = same
            .iter()
            .filter(|r| !(r.tag.is_empty() || r.tag == "<none>"))
            .map(|r| format!("{}:{}", r.repo, r.tag))
            .collect();
        out.push((id.to_string(), same[0].repo.clone(), names));
    }
    out
}

fn parse_rows(out: &str) -> Vec<Row> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (id, repo, tag) = (f.next()?.trim(), f.next()?.trim(), f.next()?.trim());
            (!id.is_empty()).then(|| Row {
                id: id.to_string(),
                repo: repo.to_string(),
                tag: tag.to_string(),
            })
        })
        .collect()
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

/// What [`free`] would remove for `scope`. `None` when Docker can't be asked.
pub fn find(scope: Scope) -> Option<Report> {
    let mut ls = docker::docker();
    ls.args([
        "image",
        "ls",
        "--no-trunc",
        "--format",
        "{{.ID}}\t{{.Repository}}\t{{.Tag}}",
    ]);
    let rows = parse_rows(&docker::try_capture_empty_ok(ls)?);
    // Without knowing what containers use, nothing is safe to remove.
    let in_use = images_in_use()?;
    let images: Vec<Reclaimable> = select(&rows, &in_use, scope)
        .into_iter()
        .map(|(id, repo, names)| Reclaimable {
            size_bytes: image_size(&id),
            id,
            names,
            repo,
        })
        .collect();
    let total_bytes = images.iter().map(|i| i.size_bytes).sum();
    Some(Report {
        images,
        total_bytes,
        failed: Vec::new(),
    })
}

/// Remove what [`find`] lists for `scope`. Each removal is by name (or by ID for
/// an untagged leftover) and without force, so Docker itself refuses an image a
/// container started using in the meantime.
pub fn free(scope: Scope) -> Option<Report> {
    let found = find(scope)?;
    let mut report = Report::default();
    for image in found.images {
        let targets = if image.names.is_empty() {
            vec![image.id.clone()]
        } else {
            image.names.clone()
        };
        let mut ok = true;
        for t in &targets {
            let mut c = docker::docker();
            c.args(["image", "rm", t]);
            if docker::try_capture(c).is_none() {
                ok = false;
                report.failed.push(t.clone());
            }
        }
        if ok {
            report.total_bytes += image.size_bytes;
            report.images.push(image);
        }
    }
    Some(report)
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

    fn row(id: &str, repo: &str, tag: &str) -> Row {
        Row {
            id: id.into(),
            repo: repo.into(),
            tag: tag.into(),
        }
    }

    fn ids(v: &[(String, String, Vec<String>)]) -> Vec<&str> {
        v.iter().map(|(id, _, _)| id.as_str()).collect()
    }

    #[test]
    fn only_unused_atelier_images_go_and_after_an_update_only_old_ones() {
        let rows = vec![
            row("cms-old", IMAGE_REPO, "<none>"),
            row("cms-now", IMAGE_REPO, "latest"),
            row("cms-stopped-site", IMAGE_REPO, "0.27.0"),
            row("engine-pin", ENGINE_IMAGE_REPO, "pinned-atelier-x"),
            row("pg-old", "pgvector/pgvector", "pg16"),
            row("pg-now", "pgvector/pgvector", "pg16-trixie"),
            row("nginx-old", "nginx", "1.28-alpine"),
            row("nginx-now", "nginx", "1.30-alpine-slim"),
            // Not Atelier's: never, whatever the scope.
            row("other", "node", "22"),
            row("other-untagged", "<none>", "<none>"),
            row("redis", "redis", "7"),
        ];
        let in_use: HashSet<String> = ["cms-now".to_string()].into();

        let after = select(&rows, &in_use, Scope::AfterUpdate);
        assert_eq!(ids(&after), ["cms-old", "pg-old", "nginx-old"]);

        let all = select(&rows, &in_use, Scope::Everything);
        assert_eq!(
            ids(&all),
            [
                "cms-old",
                "cms-stopped-site",
                "engine-pin",
                "pg-old",
                "pg-now",
                "nginx-old",
                "nginx-now"
            ]
        );
    }

    #[test]
    fn an_image_shared_with_a_name_that_isnt_atelier_s_stays() {
        // One image ID tagged both by Atelier and by the owner for something else.
        let rows = vec![
            row("shared", "pgvector/pgvector", "pg16"),
            row("shared", "my-db", "backup"),
        ];
        assert!(select(&rows, &HashSet::new(), Scope::Everything).is_empty());
    }

    #[test]
    fn rows_parse_and_sizes_read_well() {
        let rows = parse_rows("sha256:a\tghcr.io/aincient-labs/atelier-cms\t<none>\n\nsha256:b\tnginx\t1.30-alpine-slim\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].tag, "1.30-alpine-slim");
        assert_eq!(human_bytes(420_000_000), "420 MB");
        assert_eq!(human_bytes(1_234_000_000), "1.2 GB");
        assert_eq!(human_bytes(0), "0 MB");
    }
}
