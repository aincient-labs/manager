//! The component-pack developer loop (plans/byo-components.md Phase 4).
//!
//! `atelier pack new` scaffolds a pack — an ordinary Drupal module carrying
//! `thirdPartySettings.atelier` — from the embedded templates (the published
//! `atelier-pack-template` repo is GENERATED from this same set, so the two
//! never drift). `atelier pack dev` brings up the pinned appliance image with
//! the pack mounted plus the dev overlays (opcache revalidation, Twig
//! auto-reload, a Tailwind watcher, the AINCIENT_DEV endpoints), and
//! `atelier pack validate` runs the appliance's own admission gate.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

use crate::docker::{self, compose, run_capture, run_inherited};
use crate::stack::Stack;

/// A pack checkout on disk: the directory and the module machine name (taken
/// from the `<module>.info.yml` at its root — the one Drupal itself trusts).
pub struct Pack {
    pub dir: PathBuf,
    pub module: String,
}

impl Pack {
    /// Locate the pack whose root is `dir` (typically the working directory).
    pub fn locate(dir: &Path) -> Result<Pack> {
        let dir = dir
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", dir.display()))?;
        let mut infos: Vec<String> = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(module) = name.strip_suffix(".info.yml") {
                infos.push(module.to_string());
            }
        }
        match infos.len() {
            0 => bail!(
                "no <module>.info.yml here — run this from a pack's root (or start one with `atelier pack new <name>`)"
            ),
            1 => Ok(Pack { module: infos.remove(0), dir }),
            _ => bail!("more than one .info.yml here ({}) — a pack root carries exactly one", infos.join(", ")),
        }
    }
}

/// A machine name Drupal will accept — also our sole path-safety guarantee for
/// everything derived from it, so enforce it before any filesystem write.
pub fn valid_module_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 50
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The scaffold: every file `atelier pack new` lays down, as
/// (relative path, template) pairs after placeholder substitution.
/// Public so the template-repo generator and tests can enumerate it.
pub fn scaffold_files(module: &str) -> Vec<(String, String)> {
    let label = {
        let mut words = module.replace('_', " ");
        if let Some(first) = words.get_mut(0..1) {
            let upper = first.to_uppercase();
            words.replace_range(0..1, &upper);
        }
        words
    };
    let render = |tpl: &str| tpl.replace("__MODULE__", module).replace("__LABEL__", &label);
    vec![
        (format!("{module}.info.yml"), render(include_str!("../templates/pack/module.info.yml.tpl"))),
        ("atelier.pack.yml".into(), render(include_str!("../templates/pack/atelier.pack.yml.tpl"))),
        ("components/showcase/showcase.component.yml".into(), render(include_str!("../templates/pack/showcase.component.yml.tpl"))),
        ("components/showcase/showcase.twig".into(), render(include_str!("../templates/pack/showcase.twig.tpl"))),
        // The SOURCE rules (imported by input.css) and the committed OUTPUT the
        // appliance links — seeded identical; the dev watcher rebuilds the output.
        ("build/pack.css".into(), render(include_str!("../templates/pack/pack.css.tpl"))),
        (format!("assets/{module}.css"), render(include_str!("../templates/pack/pack.css.tpl"))),
        ("build/input.css".into(), render(include_str!("../templates/pack/input.css.tpl"))),
        ("build/atelier/tokens.generated.css".into(), render(include_str!("../templates/pack/preset-placeholder.css.tpl"))),
        ("build/atelier/tw-palette.generated.css".into(), render(include_str!("../templates/pack/preset-placeholder.css.tpl"))),
        ("compose.dev.yaml".into(), render(include_str!("../templates/pack/compose.dev.yaml.tpl"))),
        ("compose.ci.yaml".into(), render(include_str!("../templates/pack/compose.ci.yaml.tpl"))),
        ("dev/pack.yml".into(), render(include_str!("../templates/pack/packsd.yml.tpl"))),
        ("dev/zz-dev.ini".into(), render(include_str!("../templates/pack/zz-dev.ini.tpl"))),
        ("dev/services.dev.yml".into(), render(include_str!("../templates/pack/services.dev.yml.tpl"))),
        ("Dockerfile".into(), render(include_str!("../templates/pack/Dockerfile.tpl"))),
        (".dockerignore".into(), render(include_str!("../templates/pack/dockerignore.tpl"))),
        (".gitignore".into(), render(include_str!("../templates/pack/gitignore.tpl"))),
        (".github/workflows/build.yml".into(), render(include_str!("../templates/pack/workflow.yml.tpl"))),
        ("README.md".into(), render(include_str!("../templates/pack/README.md.tpl"))),
    ]
}

/// `atelier pack new <module>`: scaffold into `<parent>/<module>`.
/// Refuses to touch a directory that already exists — never overwrites work.
pub fn scaffold(parent: &Path, module: &str) -> Result<PathBuf> {
    if !valid_module_name(module) {
        bail!("\"{module}\" is not a valid module machine name (lowercase letters, digits and _, starting with a letter)");
    }
    let dest = parent.join(module);
    if dest.exists() {
        bail!("{} already exists — refusing to overwrite it", dest.display());
    }
    for (rel, content) in scaffold_files(module) {
        let path = dest.join(&rel);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(dest)
}

/// The `docker compose` invocation for a dev stack: the stack's own
/// compose.yaml PLUS the pack's committed `compose.dev.yaml` overlay, with the
/// `${PACK_DIR}`/`${PACK_MODULE}` interpolations the overlay expects.
fn dev_compose(stack: &Stack, pack: &Pack) -> std::process::Command {
    let mut c = compose(stack);
    c.args(["-f".as_ref(), stack.home.join("compose.yaml").as_os_str()])
        .args(["-f".as_ref(), pack.dir.join("compose.dev.yaml").as_os_str()])
        .env("PACK_DIR", &pack.dir)
        .env("PACK_MODULE", &pack.module);
    c
}

/// Bring the dev stack up (recreating the app container so the mounts and
/// AINCIENT_DEV take effect), then sync the token preset into the pack.
pub fn dev_up(stack: &Stack, pack: &Pack) -> Result<()> {
    docker::preflight().require()?;
    let overlay = pack.dir.join("compose.dev.yaml");
    if !overlay.is_file() {
        bail!("{} is missing — is this a pack scaffolded by `atelier pack new`?", overlay.display());
    }
    let mut c = dev_compose(stack, pack);
    c.args(["up", "-d", "--wait"]);
    run_inherited(c, "bring the pack dev stack up")?;
    Ok(())
}

/// Tear the dev overlay down: back to (or off) the plain stack is the
/// caller's choice; this stops the whole compose project.
pub fn dev_down(stack: &Stack, pack: &Pack) -> Result<()> {
    let mut c = dev_compose(stack, pack);
    c.args(["down"]);
    run_inherited(c, "stop the pack dev stack")
}

/// Copy the appliance's committed token/utility preset out of the RUNNING dev
/// container into `build/atelier/` — pinning the pack's CSS build to the exact
/// image version it develops against. Committed; CI builds against it.
pub fn sync_preset(stack: &Stack, pack: &Pack) -> Result<()> {
    const SRC: &str = "/opt/drupal/web/modules/custom/aincient_pages/build";
    let dest = pack.dir.join("build/atelier");
    fs::create_dir_all(&dest)?;
    for file in ["tokens.generated.css", "tw-palette.generated.css"] {
        let mut c = compose(stack);
        c.args(["exec", "-T", "app", "cat", &format!("{SRC}/{file}")]);
        let css = run_capture(c, &format!("read the {file} preset from the appliance"))?;
        fs::write(dest.join(file), css)?;
    }
    Ok(())
}

/// One drush invocation inside the app container, output inherited.
fn drush_inherited(stack: &Stack, args: &[&str], action: &str) -> Result<()> {
    let mut c = compose(stack);
    c.args(["exec", "-T", "app", "/opt/drupal/vendor/bin/drush", "--root=/opt/drupal/web"])
        .args(args);
    run_inherited(c, action)
}

/// `atelier pack validate`: the appliance's own gate — drush
/// atelier:pack-validate scoped to this pack. Exit code carries the verdict.
pub fn validate(stack: &Stack, module: &str) -> Result<()> {
    drush_inherited(
        stack,
        &["atelier:pack-validate", module],
        "run the admission gate (is the dev stack up? try `atelier pack dev`)",
    )
}

/// A cache rebuild — what a `.component.yml` edit needs before discovery sees
/// the change (Twig/PHP/CSS edits need nothing: the dev overlays cover those).
pub fn cache_rebuild(stack: &Stack) -> Result<()> {
    drush_inherited(stack, &["cache:rebuild"], "rebuild caches after a component.yml change")
}

/// The dev watch loop: poll the pack for `*.component.yml` / `*.info.yml`
/// changes (std-only, no watcher dependency — a 2s poll is plenty for a save)
/// and run `drush cr` + the gate on each change, until interrupted.
/// `report` receives human-readable progress lines.
pub fn watch(stack: &Stack, pack: &Pack, mut report: impl FnMut(&str)) -> Result<()> {
    let mut seen = schema_mtimes(&pack.dir);
    report("watching *.component.yml — save one and the catalog rebuilds (Twig/CSS edits need no rebuild; Ctrl+C to stop)");
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let now = schema_mtimes(&pack.dir);
        if now != seen {
            seen = now;
            report("component schema changed — rebuilding caches");
            if let Err(e) = cache_rebuild(stack) {
                report(&format!("cache rebuild failed: {e:#}"));
                continue;
            }
            if validate(stack, &pack.module).is_err() {
                report("the admission gate REJECTED the change — see the table above");
            } else {
                report("gate clean — refresh the gallery to see it");
            }
        }
    }
}

/// Every schema-ish file's mtime, keyed by path — the poll set for [`watch`].
fn schema_mtimes(dir: &Path) -> BTreeMap<PathBuf, SystemTime> {
    let mut map = BTreeMap::new();
    collect_mtimes(dir, &mut map, 0);
    map
}

fn collect_mtimes(dir: &Path, map: &mut BTreeMap<PathBuf, SystemTime>, depth: u8) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.starts_with('.') && name != "node_modules" {
                collect_mtimes(&path, map, depth + 1);
            }
        } else if name.ends_with(".component.yml") || name.ends_with(".info.yml") || name == "atelier.pack.yml" {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    map.insert(path, mtime);
                }
            }
        }
    }
}

/// A minimal HTTP/1.0 GET against the local appliance — used by the MCP
/// server to proxy the AINCIENT_DEV endpoints. Deliberately dependency-free
/// (localhost, no TLS), same posture as `ops::http_ready`.
pub fn http_get(port: u16, path_and_query: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("nothing is listening on 127.0.0.1:{port} — is the dev stack up? (`atelier pack dev`)"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // HTTP/1.0: the server answers whole and closes — no chunked encoding to
    // parse. Read to EOF, split head from body.
    write!(stream, "GET {path_and_query} HTTP/1.0\r\nHost: localhost\r\nAccept: */*\r\n\r\n")?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP response from the appliance")?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_names_are_validated() {
        assert!(valid_module_name("acme_pack"));
        assert!(valid_module_name("a1"));
        assert!(!valid_module_name(""));
        assert!(!valid_module_name("1acme"));
        assert!(!valid_module_name("Acme"));
        assert!(!valid_module_name("acme-pack"));
        assert!(!valid_module_name("acme pack"));
        assert!(!valid_module_name("../evil"));
    }

    #[test]
    fn scaffold_substitutes_and_lays_down_the_contract() {
        let files = scaffold_files("acme_pack");
        let by_name: BTreeMap<_, _> = files.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        // The four load-bearing files exist and carry the module name.
        assert!(by_name.contains_key("acme_pack.info.yml"));
        assert!(by_name["atelier.pack.yml"].contains("name: acme_pack"));
        assert!(by_name["dev/pack.yml"].contains("module: acme_pack"));
        assert!(by_name["Dockerfile"].contains("modules/custom/acme_pack"));
        // The client image re-registers itself in the extensions label the
        // manager's pre-pull diff reads — losing this breaks client updates.
        assert!(by_name["Dockerfile"].contains("LABEL dev.atelier.extensions"));
        assert!(by_name[".github/workflows/build.yml"].contains("ATELIER_EXTENSIONS"));
        assert!(by_name["compose.ci.yaml"].contains("acme_pack:ci"));
        // The component declares the pack stylesheet it ships.
        assert!(by_name["components/showcase/showcase.component.yml"].contains("stylesheet: assets/acme_pack.css"));
        assert!(by_name.contains_key("assets/acme_pack.css"));
        // No placeholder survives substitution anywhere.
        for (name, content) in &files {
            assert!(!content.contains("__MODULE__"), "{name} kept __MODULE__");
            assert!(!content.contains("__LABEL__"), "{name} kept __LABEL__");
        }
    }

    #[test]
    fn scaffold_refuses_an_existing_directory() {
        let tmp = std::env::temp_dir().join(format!("atelier-pack-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let dest = scaffold(&tmp, "acme_pack").unwrap();
        assert!(dest.join("acme_pack.info.yml").is_file());
        assert!(dest.join("compose.dev.yaml").is_file());
        assert!(scaffold(&tmp, "acme_pack").is_err(), "second scaffold must refuse");
        assert!(scaffold(&tmp, "in valid").is_err());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pack_locate_reads_the_info_yml() {
        let tmp = std::env::temp_dir().join(format!("atelier-locate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        assert!(Pack::locate(&tmp).is_err(), "no info.yml → error");
        fs::write(tmp.join("acme_pack.info.yml"), "name: Acme\n").unwrap();
        let pack = Pack::locate(&tmp).unwrap();
        assert_eq!(pack.module, "acme_pack");
        fs::write(tmp.join("other.info.yml"), "name: Other\n").unwrap();
        assert!(Pack::locate(&tmp).is_err(), "two info.yml → error");
        let _ = fs::remove_dir_all(&tmp);
    }
}
