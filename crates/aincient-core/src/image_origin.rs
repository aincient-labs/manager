//! Whether an image on this computer came from a registry (and can be
//! downloaded again), read correctly under both of Docker's image stores.
//!
//! The **classic** store (`overlay2`; OrbStack, older Docker Engine) gives an
//! image a `RepoDigests` entry only when it was pulled from (or pushed to) a
//! registry: a local build has none. That made `RepoDigests` the Manager's
//! "can this be downloaded again" signal.
//!
//! The **containerd** store (`io.containerd.snapshotter.v1`; the default for
//! fresh Docker Engine 29 installs on Linux and for Docker Desktop) lists a
//! `RepoDigests` entry for every name an image has, local builds included
//! (`atelier-cms@sha256:<its own ID>`), and an image ID is the digest of the
//! image's index or manifest. Checked on Docker 29.9 (containerd) and 29.4
//! (classic). There the honest signal is `Identity` (API 1.53, Docker 29):
//! `Identity.Pull` names each repository the image was pulled from,
//! `Identity.Build` marks a local build. A `docker load`ed image has no
//! `Identity`, and neither has any image on an older Docker using the
//! containerd store: there the origin is unknown, and callers keep what they
//! can't prove downloadable.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::docker;

/// The parts of `docker image inspect` (JSON) this module reads. Read as
/// JSON, never with a `--format` template: a template naming a field an older
/// Docker lacks (`Identity`) is an error there, not an empty value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Inspected {
    #[serde(rename = "Id", default)]
    pub id: String,
    #[serde(rename = "RepoTags", default)]
    pub repo_tags: Option<Vec<String>>,
    #[serde(rename = "RepoDigests", default)]
    pub repo_digests: Option<Vec<String>>,
    #[serde(rename = "Config", default)]
    config: Option<Config>,
    /// Present only on the containerd store (API 1.48+).
    #[serde(rename = "Descriptor", default)]
    descriptor: Option<serde_json::Value>,
    /// Docker 29+ (API 1.53); filled in on the containerd store.
    #[serde(rename = "Identity", default)]
    identity: Option<Identity>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Config {
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Identity {
    #[serde(rename = "Pull", default)]
    pull: Option<Vec<PullIdentity>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PullIdentity {
    #[serde(rename = "Repository", default)]
    repository: String,
}

/// Which image store Docker keeps images in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Classic,
    Containerd,
}

/// The image store, from `docker info`'s driver status (`driver-type
/// io.containerd.snapshotter.v1` on the containerd store, on every Docker
/// version that has it). `None` when Docker doesn't answer.
pub fn store() -> Option<Store> {
    let mut c = docker::docker();
    c.args(["info", "--format", "{{json .DriverStatus}}"]);
    docker::try_capture(c).map(|s| store_from_driver_status(&s))
}

fn store_from_driver_status(json: &str) -> Store {
    if json.contains("io.containerd.snapshotter") {
        Store::Containerd
    } else {
        Store::Classic
    }
}

/// `docker image inspect <image>`, parsed; `None` when the image isn't here
/// (or Docker doesn't answer).
pub fn inspect(image: &str) -> Option<Inspected> {
    let mut c = docker::docker();
    c.args(["image", "inspect", image]);
    parse(&docker::try_capture(c)?).ok()?.into_iter().next()
}

/// `docker image inspect`'s JSON array.
pub fn parse(json: &str) -> serde_json::Result<Vec<Inspected>> {
    serde_json::from_str(json)
}

impl Inspected {
    /// On the containerd store: the image says so itself (`Descriptor`), or
    /// `store` does (a Docker older than API 1.48 has no `Descriptor`).
    pub fn containerd(&self, store: Option<Store>) -> bool {
        self.descriptor.is_some() || store == Some(Store::Containerd)
    }

    /// The repositories this image was pulled from, as Docker names them
    /// (`nginx`, `docker.io/library/nginx` or `ghcr.io/…`: compare with
    /// [`same_repository`]); empty for an image never pulled (a local build, a
    /// `docker load`). `None` when that can't be told: the containerd store on
    /// a Docker without `Identity`.
    pub fn pulled_from(&self, store: Option<Store>) -> Option<Vec<String>> {
        if self.containerd(store) {
            let identity = self.identity.as_ref()?;
            return Some(
                identity
                    .pull
                    .iter()
                    .flatten()
                    .map(|p| p.repository.clone())
                    .filter(|r| !r.is_empty())
                    .collect(),
            );
        }
        Some(
            self.repo_digests
                .iter()
                .flatten()
                .filter_map(|d| d.split_once('@').map(|(name, _)| name.to_string()))
                .collect(),
        )
    }

    /// Whether this image can be downloaded again: `Some(true)` when it was
    /// pulled from a registry, `Some(false)` for a build only this computer
    /// has, `None` when that can't be told. Anything that would delete the
    /// image's last copy must treat `None` as "no".
    pub fn downloadable(&self, store: Option<Store>) -> Option<bool> {
        self.pulled_from(store).map(|r| !r.is_empty())
    }

    /// The registry digest (`sha256:…`) this image was pulled as, comparable
    /// with what the registry reports for its tag; `None` for a local build.
    /// Classic store: the first `RepoDigests` entry. Containerd store: the
    /// image ID, which there is the digest of what was pulled, once
    /// `Identity` shows it was pulled (`RepoDigests` there names every tag,
    /// local builds too, and is empty on a pulled image whose tag moved on).
    /// On a containerd store without `Identity`, the first `RepoDigests`
    /// entry, as before.
    pub fn registry_digest(&self, store: Option<Store>) -> Option<String> {
        let first_repo_digest = || {
            self.repo_digests
                .iter()
                .flatten()
                .find_map(|d| d.split_once('@').map(|(_, digest)| digest.to_string()))
        };
        if !self.containerd(store) {
            return first_repo_digest();
        }
        match self.downloadable(store) {
            Some(true) => self.id.starts_with("sha256:").then(|| self.id.clone()),
            Some(false) => None,
            None => first_repo_digest(),
        }
    }

    /// The image's label `key`, if set.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.config
            .as_ref()?
            .labels
            .as_ref()?
            .get(key)
            .map(String::as_str)
    }

    /// The `repo:tag` names the image has.
    pub fn tags(&self) -> &[String] {
        self.repo_tags.as_deref().unwrap_or_default()
    }
}

/// Whether two repository names are the same repository: Docker Hub's implied
/// `docker.io/` and `library/` don't count.
pub fn same_repository(a: &str, b: &str) -> bool {
    fn norm(r: &str) -> &str {
        let r = r
            .strip_prefix("docker.io/")
            .or_else(|| r.strip_prefix("index.docker.io/"))
            .unwrap_or(r);
        r.strip_prefix("library/").unwrap_or(r)
    }
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `docker image inspect` on Docker 29.9.3, containerd store (trimmed to
    // the fields read): a pulled image, a local build, a `docker load`ed
    // image, and a pulled image whose tag moved on while a container used it.
    const CD_PULLED: &str = r#"[{"Id":"sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec","RepoTags":["busybox:1.37"],"RepoDigests":["busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"],"Descriptor":{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec","size":9535},"Identity":{"Pull":[{"Repository":"docker.io/library/busybox"}]}}]"#;
    const CD_BUILT: &str = r#"[{"Id":"sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1","RepoTags":["atelier-scn-app:x-old"],"RepoDigests":["atelier-scn-app@sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1"],"Comment":"buildkit.dockerfile.v0","Config":{"Labels":{"org.example":"1"}},"Descriptor":{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1","size":855},"Identity":{"Build":[{"Ref":"nwzy71ql8sdg08butp5eig1lw","CreatedAt":"2026-11-15T11:44:46.225289827Z"}]}}]"#;
    const CD_LOADED: &str = r#"[{"Id":"sha256:059f18976e9e5c1408093377fcb157e2c137152df2c93ebabd7af8dfe3367bf8","RepoTags":["atelier-scn-app:y"],"RepoDigests":["atelier-scn-app@sha256:059f18976e9e5c1408093377fcb157e2c137152df2c93ebabd7af8dfe3367bf8"],"Descriptor":{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:059f18976e9e5c1408093377fcb157e2c137152df2c93ebabd7af8dfe3367bf8","size":855},"Identity":null}]"#;
    const CD_PULLED_UNTAGGED: &str = r#"[{"Id":"sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec","RepoTags":[],"RepoDigests":[],"Descriptor":{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec","size":9535},"Identity":{"Pull":[{"Repository":"docker.io/library/busybox"}]}}]"#;
    // Docker 29.4.0, classic store: a pull and a local build.
    const CLASSIC_PULLED: &str = r#"[{"Id":"sha256:1111111111111111111111111111111111111111111111111111111111111111","RepoTags":["busybox:1.37"],"RepoDigests":["busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"],"Descriptor":null,"Identity":null}]"#;
    const CLASSIC_BUILT: &str = r#"[{"Id":"sha256:fd3e722c434033cde19e692506dbfc443a86d74ddf0a2d3b7c97e5ae86ac74fb","RepoTags":["atelier-scn-probe:1"],"RepoDigests":[],"Descriptor":null,"Identity":null}]"#;
    // A containerd store on a Docker older than 29: no `Identity`.
    const CD_OLD_DOCKER: &str = r#"[{"Id":"sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1","RepoTags":["atelier-scn-app:x"],"RepoDigests":["atelier-scn-app@sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1"],"Descriptor":{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"sha256:4f7e751fef6b0e8b5e97e05cb2003b6bef01aaeb73753df7517e12c40674cea1","size":855}}]"#;

    fn one(json: &str) -> Inspected {
        parse(json).unwrap().remove(0)
    }

    #[test]
    fn the_store_is_read_off_the_driver_status() {
        assert_eq!(
            store_from_driver_status(r#"[["driver-type","io.containerd.snapshotter.v1"]]"#),
            Store::Containerd
        );
        assert_eq!(
            store_from_driver_status(
                r#"[["Backing Filesystem","btrfs"],["Supports d_type","true"]]"#
            ),
            Store::Classic
        );
    }

    #[test]
    fn a_local_build_is_never_downloadable_under_either_store() {
        assert_eq!(
            one(CLASSIC_BUILT).downloadable(Some(Store::Classic)),
            Some(false)
        );
        // Containerd gives the build a RepoDigests entry under its own name.
        assert_eq!(one(CD_BUILT).downloadable(None), Some(false));
        assert_eq!(
            one(CD_BUILT).downloadable(Some(Store::Containerd)),
            Some(false)
        );
        // A `docker load`ed image has no `Identity` at all: not provably
        // downloadable, so kept, the same as on a Docker without `Identity`.
        assert_eq!(one(CD_LOADED).downloadable(None), None);
        assert_eq!(one(CD_BUILT).registry_digest(None), None);
        assert_eq!(
            one(CLASSIC_BUILT).registry_digest(Some(Store::Classic)),
            None
        );
        assert_eq!(one(CD_BUILT).pulled_from(None), Some(vec![]));
    }

    #[test]
    fn a_pulled_image_is_downloadable_under_either_store() {
        let classic = one(CLASSIC_PULLED);
        assert_eq!(classic.downloadable(Some(Store::Classic)), Some(true));
        assert_eq!(classic.pulled_from(None), Some(vec!["busybox".to_string()]));
        assert_eq!(
            classic.registry_digest(None).as_deref(),
            Some("sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec")
        );
        let cd = one(CD_PULLED);
        assert_eq!(cd.downloadable(None), Some(true));
        assert_eq!(
            cd.pulled_from(None),
            Some(vec!["docker.io/library/busybox".to_string()])
        );
        assert_eq!(cd.registry_digest(None), Some(cd.id.clone()));
        // Its tag moved on: no RepoDigests left, still pulled, still its digest.
        let untagged = one(CD_PULLED_UNTAGGED);
        assert_eq!(untagged.downloadable(None), Some(true));
        assert_eq!(untagged.registry_digest(None), Some(untagged.id.clone()));
    }

    #[test]
    fn a_containerd_store_without_identity_cant_tell() {
        let old = one(CD_OLD_DOCKER);
        assert_eq!(old.downloadable(None), None);
        assert_eq!(old.pulled_from(None), None);
        // Informational reads keep the old RepoDigests reading.
        assert_eq!(old.registry_digest(None), Some(old.id.clone()));
        // A Docker older than API 1.48 has no Descriptor either: the store says.
        let mut older = old.clone();
        older.descriptor = None;
        assert_eq!(older.downloadable(Some(Store::Containerd)), None);
    }

    #[test]
    fn labels_and_repositories() {
        assert_eq!(one(CD_BUILT).label("org.example"), Some("1"));
        assert_eq!(one(CD_PULLED).label("org.example"), None);
        assert!(same_repository("docker.io/library/busybox", "busybox"));
        assert!(same_repository("docker.io/someone/fork", "someone/fork"));
        assert!(!same_repository(
            "ghcr.io/aincient-labs/atelier-cms",
            "atelier-cms"
        ));
    }
}
