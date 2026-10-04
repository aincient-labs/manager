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
use crate::stack::{InstallOptions, Stack};

/// A pack checkout on disk: the directory and the module machine name (taken
/// from the `<module>.info.yml` at its root — the one Drupal itself trusts).
pub struct Pack {
    pub dir: PathBuf,
    pub module: String,
}

impl Pack {
    /// Locate the pack that contains `start`: walk up from `start` to the
    /// nearest directory holding an `atelier.pack.yml` (stopping at the
    /// filesystem root), then [`Pack::locate`] it. MCP clients don't guarantee
    /// the launch directory, so `atelier mcp` must not assume the pack root.
    pub fn locate_upward(start: &Path) -> Result<Pack> {
        let from = start
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", start.display()))?;
        for dir in from.ancestors() {
            if dir.join("atelier.pack.yml").is_file() {
                return Pack::locate(dir);
            }
        }
        bail!(
            "no atelier.pack.yml found in {} or any parent directory — run `atelier mcp` from inside a pack (the directory with atelier.pack.yml), or start one with `atelier pack new <name>`",
            from.display()
        )
    }

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
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The scaffold: every file `atelier pack new` lays down, as
/// (relative path, template) pairs after placeholder substitution.
/// Public so the template-repo generator and tests can enumerate it.
///
/// `studio` adds an EXPERIMENTAL hello-world console studio (DECISIONS 0448):
/// `<module>.studios.yml`, a plain-JS `studio/studio.js` + its CSS, and
/// `studio/atelier-studio.d.ts` — the mount contract, generated from the
/// console's `contract.ts` by the umbrella's `bin/sync-pack-studio-dts`. It is
/// opt-in because the mount boundary needs Atelier 0.16, and a pack that ships
/// a studio puts a rail in every console that installs it.
pub fn scaffold_files(module: &str, studio: bool) -> Vec<(String, String)> {
    let label = {
        let mut words = module.replace('_', " ");
        if let Some(first) = words.get_mut(0..1) {
            let upper = first.to_uppercase();
            words.replace_range(0..1, &upper);
        }
        words
    };
    let (provides, requires) = if studio {
        ("components, studios", "^0.16")
    } else {
        ("components", "^0.10")
    };
    let render = |tpl: &str| {
        tpl.replace("__MODULE__", module)
            .replace("__LABEL__", &label)
            .replace("__PROVIDES__", provides)
            .replace("__REQUIRES__", requires)
    };
    let mut files = vec![
        (
            format!("{module}.info.yml"),
            render(include_str!("../templates/pack/module.info.yml.tpl")),
        ),
        (
            "atelier.pack.yml".into(),
            render(include_str!("../templates/pack/atelier.pack.yml.tpl")),
        ),
        (
            "components/showcase/showcase.component.yml".into(),
            render(include_str!("../templates/pack/showcase.component.yml.tpl")),
        ),
        (
            "components/showcase/showcase.twig".into(),
            render(include_str!("../templates/pack/showcase.twig.tpl")),
        ),
        // The SOURCE rules (imported by input.css) and the committed OUTPUT the
        // appliance links — seeded identical; the dev watcher rebuilds the output.
        (
            "build/pack.css".into(),
            render(include_str!("../templates/pack/pack.css.tpl")),
        ),
        (
            format!("assets/{module}.css"),
            render(include_str!("../templates/pack/pack.css.tpl")),
        ),
        (
            "build/input.css".into(),
            render(include_str!("../templates/pack/input.css.tpl")),
        ),
        (
            "build/atelier/tokens.generated.css".into(),
            render(include_str!("../templates/pack/preset-placeholder.css.tpl")),
        ),
        (
            "build/atelier/tw-palette.generated.css".into(),
            render(include_str!("../templates/pack/preset-placeholder.css.tpl")),
        ),
        (
            "compose.dev.yaml".into(),
            render(include_str!("../templates/pack/compose.dev.yaml.tpl")),
        ),
        (
            "compose.ci.yaml".into(),
            render(include_str!("../templates/pack/compose.ci.yaml.tpl")),
        ),
        (
            "dev/pack.yml".into(),
            render(include_str!("../templates/pack/packsd.yml.tpl")),
        ),
        (
            "dev/zz-dev.ini".into(),
            render(include_str!("../templates/pack/zz-dev.ini.tpl")),
        ),
        (
            "dev/services.dev.yml".into(),
            render(include_str!("../templates/pack/services.dev.yml.tpl")),
        ),
        (
            "Dockerfile".into(),
            render(include_str!("../templates/pack/Dockerfile.tpl")),
        ),
        (
            ".dockerignore".into(),
            render(include_str!("../templates/pack/dockerignore.tpl")),
        ),
        (
            ".gitignore".into(),
            render(include_str!("../templates/pack/gitignore.tpl")),
        ),
        (
            ".github/workflows/build.yml".into(),
            render(include_str!("../templates/pack/workflow.yml.tpl")),
        ),
        // MCP has no discovery: this is how an agent opened in the pack finds
        // `atelier mcp`.
        (
            ".mcp.json".into(),
            render(include_str!("../templates/pack/mcp.json.tpl")),
        ),
        (
            "README.md".into(),
            render(include_str!("../templates/pack/README.md.tpl")),
        ),
    ];
    if studio {
        files.extend([
            (
                format!("{module}.studios.yml"),
                render(include_str!("../templates/pack/studios.yml.tpl")),
            ),
            (
                "studio/studio.js".into(),
                render(include_str!("../templates/pack/studio.js.tpl")),
            ),
            (
                "studio/studio.css".into(),
                render(include_str!("../templates/pack/studio.css.tpl")),
            ),
            (
                "studio/atelier-studio.d.ts".into(),
                include_str!("../templates/pack/atelier-studio.d.ts.tpl").to_string(),
            ),
        ]);
    }
    files
}

/// `atelier pack new <module>`: scaffold into `<parent>/<module>`.
/// Refuses to touch a directory that already exists — never overwrites work.
pub fn scaffold(parent: &Path, module: &str, studio: bool) -> Result<PathBuf> {
    if !valid_module_name(module) {
        bail!("\"{module}\" is not a valid module machine name (lowercase letters, digits and _, starting with a letter)");
    }
    let dest = parent.join(module);
    if dest.exists() {
        bail!(
            "{} already exists — refusing to overwrite it",
            dest.display()
        );
    }
    for (rel, content) in scaffold_files(module, studio) {
        let path = dest.join(&rel);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(dest)
}

/// Where the pack's ISOLATED dev appliance lives: a dot-directory inside the
/// pack itself, one per pack. The basename `.atelier-pack-<module>` sanitizes
/// (via [`Stack::project_name`]) to compose project `atelier-pack-<module>`,
/// so the dev stack's containers and volumes can never collide with the
/// machine's real appliance (`atelier`) or with another pack's.
pub fn dev_home(pack: &Pack) -> PathBuf {
    pack.dir.join(format!(".atelier-pack-{}", pack.module))
}

/// The pack's isolated dev stack — a plain [`Stack`], so every compose/env
/// helper works on it unchanged.
pub fn dev_stack(pack: &Pack) -> Stack {
    Stack::at(dev_home(pack))
}

/// The stack this pack's dev loop is actually running against: the isolated
/// one once it has been laid down, else the real appliance (an `--attach`
/// run, or a stack from before isolation existed). down/validate/watch/mcp
/// resolve through this so they hit whichever stack `pack dev` started,
/// never blindly the real one.
pub fn resolve_stack(pack: &Pack, real: &Stack) -> Stack {
    let dev = dev_stack(pack);
    if dev.compose_path().exists() {
        dev
    } else {
        real.clone()
    }
}

/// The appliance image the pack's Dockerfile pins (`ARG ATELIER_IMAGE=…`) —
/// the dev stack should run the exact image the pack deploys against, not
/// whatever the machine's real appliance happens to follow.
fn pack_pinned_image(pack: &Pack) -> Option<String> {
    let text = fs::read_to_string(pack.dir.join("Dockerfile")).ok()?;
    let value = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("ARG ATELIER_IMAGE="))?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// First bindable localhost port in 41300..41400 that isn't in `avoid` (the
/// real appliance's port — colliding with it would be a trap the moment both
/// stacks run, bound right now or not). Falls back to 41300 if the whole
/// range is busy: compose then fails visibly rather than us picking somewhere
/// surprising.
fn pick_free_port(avoid: &[u16]) -> u16 {
    (41300..41400)
        .find(|p| !avoid.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .unwrap_or(41300)
}

/// Lay down (or adopt) the pack's isolated dev stack. A first run scaffolds
/// it on the image the Dockerfile pins and a free port clear of the real
/// appliance; a re-run passes no overrides so the stored port/image (and the
/// HASH_SALT) stick — [`Stack::ensure_scaffold`] never clobbers an existing
/// `.env`.
pub fn ensure_dev_stack(pack: &Pack, real: &Stack) -> Result<Stack> {
    let stack = dev_stack(pack);
    let opts = if stack.env_path().is_file() {
        InstallOptions::default()
    } else {
        InstallOptions {
            image: pack_pinned_image(pack),
            http_port: Some(pick_free_port(&[real.http_port()])),
        }
    };
    stack.ensure_scaffold(&opts)?;
    Ok(stack)
}

/// Poll the dev appliance until Drupal actually serves, or `timeout` elapses.
/// Returns whether it became ready; `report` receives human-readable progress
/// lines. Probes `/user/login`, NOT `/` — the front page is anonymously
/// page-cacheable, so it can stay warm while every real route 500s (the same
/// rationale as `ops::http_ready`); a form-bearing route proves the container
/// rendered something.
pub fn wait_ready(stack: &Stack, timeout: Duration, report: &mut impl FnMut(&str)) -> bool {
    report("waiting for the dev appliance — a first boot installs a throwaway demo site, which can take a few minutes");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok((status, _)) = http_get(stack.http_port(), "/user/login") {
            if status < 500 {
                report("the dev appliance is up.");
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            report("timed out waiting for the dev appliance — check its logs with `docker compose logs` in the stack directory");
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
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
        bail!(
            "{} is missing — is this a pack scaffolded by `atelier pack new`?",
            overlay.display()
        );
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

/// [`dev_down`] plus `-v`: also delete the compose project's volumes — the
/// throwaway demo DB and files. The reset for the ISOLATED dev site; callers
/// must never point it at the real appliance, whose volumes are the site.
pub fn dev_down_purge(stack: &Stack, pack: &Pack) -> Result<()> {
    let mut c = dev_compose(stack, pack);
    c.args(["down", "-v"]);
    run_inherited(c, "stop the pack dev stack and delete its volumes")
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
    c.args([
        "exec",
        "-T",
        "app",
        "/opt/drupal/vendor/bin/drush",
        "--root=/opt/drupal/web",
    ])
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
    drush_inherited(
        stack,
        &["cache:rebuild"],
        "rebuild caches after a component.yml change",
    )
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
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.starts_with('.') && name != "node_modules" {
                collect_mtimes(&path, map, depth + 1);
            }
        } else if name.ends_with(".component.yml")
            || name.ends_with(".info.yml")
            || name == "atelier.pack.yml"
        {
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
    let mut stream = TcpStream::connect(("127.0.0.1", port)).with_context(|| {
        format!(
            "nothing is listening on 127.0.0.1:{port} — is the dev stack up? (`atelier pack dev`)"
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // HTTP/1.0: the server answers whole and closes — no chunked encoding to
    // parse. Read to EOF, split head from body.
    write!(
        stream,
        "GET {path_and_query} HTTP/1.0\r\nHost: localhost\r\nAccept: */*\r\n\r\n"
    )?;
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
        let files = scaffold_files("acme_pack", false);
        let by_name: BTreeMap<_, _> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
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
        // The isolated dev-stack home never enters git or the image build
        // context — it holds a .env with a HASH_SALT.
        assert!(by_name[".gitignore"].contains(".atelier-pack-*/"));
        assert!(by_name[".dockerignore"].contains(".atelier-pack-*"));
        // The component declares the pack stylesheet it ships.
        assert!(by_name["components/showcase/showcase.component.yml"]
            .contains("stylesheet: assets/acme_pack.css"));
        assert!(by_name.contains_key("assets/acme_pack.css"));
        // A plain pack ships no studio and declares none.
        assert!(by_name["atelier.pack.yml"].contains("provides: [components]\n"));
        assert!(!files
            .iter()
            .any(|(k, _)| k.starts_with("studio/") || k.ends_with(".studios.yml")));
        // No placeholder survives substitution anywhere.
        for (name, content) in &files {
            assert!(!content.contains("__MODULE__"), "{name} kept __MODULE__");
            assert!(!content.contains("__LABEL__"), "{name} kept __LABEL__");
        }
    }

    #[test]
    fn studio_scaffold_lays_down_the_mount_contract() {
        let files = scaffold_files("acme_pack", true);
        let by_name: BTreeMap<_, _> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // Declared, so pack-validate does not warn "shipped but undeclared",
        // and pinned to the release that first mounts a pack studio.
        assert!(by_name["atelier.pack.yml"].contains("provides: [components, studios]"));
        assert!(by_name["atelier.pack.yml"].contains("atelier: '^0.16'"));
        // The id is the module name — pack-validate rejects an unprefixed one.
        let manifest = by_name["acme_pack.studios.yml"];
        assert!(manifest.contains("\nacme_pack:\n"));
        assert!(manifest.contains("script: studio/studio.js"));
        assert!(manifest.contains("style: studio/studio.css"));
        // The module speaks the contract version the .d.ts describes.
        let dts = by_name["studio/atelier-studio.d.ts"];
        assert!(dts.contains("STUDIO_MOUNT_API_VERSION = 1;"));
        assert!(dts.contains("export type StudioMountContext"));
        let js = by_name["studio/studio.js"];
        assert!(js.contains("export const apiVersion = 1;"));
        assert!(js.contains("export function mount(el, ctx)"));
        assert!(js.contains(r#"import("./atelier-studio")"#));
        for (name, content) in &files {
            assert!(!content.contains("__MODULE__"), "{name} kept __MODULE__");
            assert!(
                !content.contains("__PROVIDES__"),
                "{name} kept __PROVIDES__"
            );
        }
    }

    #[test]
    fn scaffold_refuses_an_existing_directory() {
        let tmp = std::env::temp_dir().join(format!("atelier-pack-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let dest = scaffold(&tmp, "acme_pack", false).unwrap();
        assert!(dest.join("acme_pack.info.yml").is_file());
        assert!(dest.join("compose.dev.yaml").is_file());
        assert!(
            scaffold(&tmp, "acme_pack", false).is_err(),
            "second scaffold must refuse"
        );
        assert!(scaffold(&tmp, "in valid", false).is_err());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dev_home_yields_the_isolated_compose_project() {
        let pack = Pack {
            dir: PathBuf::from("/somewhere/acme_pack"),
            module: "acme_pack".into(),
        };
        assert_eq!(
            dev_home(&pack),
            PathBuf::from("/somewhere/acme_pack/.atelier-pack-acme_pack")
        );
        // The load-bearing fact: the basename sanitizes to a project name that
        // can never collide with the real appliance's `atelier`.
        assert_eq!(dev_stack(&pack).project_name(), "atelier-pack-acme_pack");
    }

    #[test]
    fn pack_pinned_image_reads_the_dockerfile_arg() {
        let tmp = std::env::temp_dir().join(format!("atelier-pin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let pack = Pack {
            dir: tmp.clone(),
            module: "acme_pack".into(),
        };
        assert_eq!(pack_pinned_image(&pack), None, "no Dockerfile → no pin");

        fs::write(
            tmp.join("Dockerfile"),
            "# a comment\nARG ATELIER_IMAGE=ghcr.io/aincient-labs/atelier-cms:v1.2.3\nFROM ${ATELIER_IMAGE}\n",
        )
        .unwrap();
        assert_eq!(
            pack_pinned_image(&pack).as_deref(),
            Some("ghcr.io/aincient-labs/atelier-cms:v1.2.3")
        );

        fs::write(tmp.join("Dockerfile"), "ARG ATELIER_IMAGE=\nFROM scratch\n").unwrap();
        assert_eq!(pack_pinned_image(&pack), None, "an empty pin is no pin");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_stack_falls_back_to_the_real_appliance() {
        let tmp = std::env::temp_dir().join(format!("atelier-resolve-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let pack = Pack {
            dir: tmp.clone(),
            module: "acme_pack".into(),
        };
        let real = Stack::at(PathBuf::from("/somewhere/.atelier"));

        // No dev stack laid down yet → the real appliance (an --attach run).
        assert_eq!(resolve_stack(&pack, &real).home, real.home);

        // Once `pack dev` has scaffolded the isolated stack, everything
        // (down/validate/watch/mcp) must hit it, not the real appliance.
        let dev = dev_stack(&pack);
        fs::create_dir_all(&dev.home).unwrap();
        fs::write(dev.compose_path(), "name: x\n").unwrap();
        assert_eq!(resolve_stack(&pack, &real).home, dev.home);
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

    #[test]
    fn locate_upward_finds_pack_from_cwd_and_subdirs_or_errors_clearly() {
        let tmp = std::env::temp_dir().join(format!("atelier-upward-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let pack_dir = tmp.join("mypack");
        let nested = pack_dir.join("components/banner/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(pack_dir.join("atelier.pack.yml"), "name: x\n").unwrap();
        fs::write(pack_dir.join("acme_pack.info.yml"), "name: Acme\n").unwrap();

        let canon = pack_dir.canonicalize().unwrap();
        let here = Pack::locate_upward(&pack_dir).unwrap();
        assert_eq!(here.dir, canon);
        assert_eq!(here.module, "acme_pack");
        let deep = Pack::locate_upward(&nested).unwrap();
        assert_eq!(deep.dir, canon);

        // A sibling dir outside any pack: actionable error naming file + origin.
        let outside = tmp.join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        let err = Pack::locate_upward(&outside).err().unwrap().to_string();
        assert!(err.contains("atelier.pack.yml"), "{err}");
        assert!(err.contains("elsewhere"), "{err}");
        assert!(err.contains("atelier pack new"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }

    /// Mirror of the appliance's advisory CSS lint (cms `StylesheetLint`):
    /// hardcoded colours and fractional opacity, comments stripped, `var(…)`
    /// fallbacks allowed. Returns the offending lines.
    fn css_lint(css: &str) -> Vec<String> {
        let mut css = css.to_string();
        while let Some(a) = css.find("/*") {
            let end = css[a..].find("*/").map_or(css.len(), |e| a + e + 2);
            css.replace_range(a..end, "");
        }
        let mut issues = vec![];
        for (i, line) in css.lines().enumerate() {
            let mut bare = String::new();
            let mut rest = line;
            while let Some(a) = rest.find("var(") {
                bare.push_str(&rest[..a]);
                rest = rest[a..].find(')').map_or("", |e| &rest[a + e + 1..]);
            }
            bare.push_str(rest);
            let hex = bare.match_indices('#').any(|(a, _)| {
                let n = bare[a + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .count();
                (3..=8).contains(&n)
            });
            let fn_colour = ["rgb(", "rgba(", "hsl(", "hsla(", "oklch(", "oklab("]
                .iter()
                .any(|f| bare.contains(f));
            let opacity = bare.find("opacity").is_some_and(|a| {
                let v = bare[a + 7..].trim_start().trim_start_matches(':').trim();
                v.starts_with('.') || v.starts_with("0.")
            });
            if hex || fn_colour || opacity {
                issues.push(format!("line {}: {}", i + 1, line.trim()));
            }
        }
        issues
    }

    #[test]
    fn scaffold_writes_the_mcp_json() {
        let tmp = std::env::temp_dir().join(format!("atelier-mcpjson-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let dest = scaffold(&tmp, "acme_pack", false).unwrap();
        let raw = fs::read_to_string(dest.join(".mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["mcpServers"]["atelier"]["command"], "atelier");
        assert_eq!(
            v["mcpServers"]["atelier"]["args"],
            serde_json::json!(["mcp"])
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pristine_scaffold_authored_css_is_lint_clean() {
        // The lint targets the AUTHORED stylesheet (build/pack.css); the
        // seeded committed output is identical, so it is clean too. Build
        // output (compiled Tailwind preflight) is a cms-side concern (W10d).
        let tmp = std::env::temp_dir().join(format!("atelier-lint-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for studio in [false, true] {
            let name = if studio { "acme_studio" } else { "acme_pack" };
            let dest = scaffold(&tmp, name, studio).unwrap();
            let authored = fs::read_to_string(dest.join("build/pack.css")).unwrap();
            let seeded = fs::read_to_string(dest.join(format!("assets/{name}.css"))).unwrap();
            assert!(css_lint(&authored).is_empty(), "{:?}", css_lint(&authored));
            assert!(css_lint(&seeded).is_empty(), "{:?}", css_lint(&seeded));
            // The linter itself still bites.
            assert!(!css_lint(".a { color: #fff; }").is_empty());
            assert!(!css_lint(".a { opacity: .5; }").is_empty());
            assert!(css_lint(".a { color: var(--x, #fff); } /* #000 */").is_empty());
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scaffold_prefills_prop_vocab_for_every_custom_prop() {
        // Props the appliance's locked vocabulary already covers.
        const LOCKED: [&str; 6] = [
            "variant",
            "tone",
            "eyebrow",
            "heading",
            "cta_label",
            "cta_url",
        ];
        let tmp = std::env::temp_dir().join(format!("atelier-vocab-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let dest = scaffold(&tmp, "acme_pack", false).unwrap();
        let yml =
            fs::read_to_string(dest.join("components/showcase/showcase.component.yml")).unwrap();
        let indent = |l: &str| l.len() - l.trim_start().len();
        let section = |header: &str, ind: usize| -> Vec<String> {
            let mut it = yml.lines().skip_while(|l| l.trim_end() != header);
            let head = it.next().unwrap_or_else(|| panic!("no {header}"));
            assert_eq!(indent(head), ind);
            it.take_while(|l| l.trim().is_empty() || indent(l) > ind)
                .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                .map(|l| l.trim().split(':').next().unwrap().to_string())
                .collect()
        };
        let props: Vec<String> = section("  properties:", 2)
            .into_iter()
            .filter(|k| {
                !k.starts_with('-') && k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            })
            .collect();
        assert!(props.contains(&"claim".to_string()), "{props:?}");
        let vocab = section("    prop_vocab:", 4);
        for p in props.iter().filter(|p| !LOCKED.contains(&p.as_str())) {
            // Only the 2-deep property names count; nested keys (type, enum…)
            // are not props.
            if ["type", "enum", "default"].contains(&p.as_str()) {
                continue;
            }
            assert!(
                vocab.contains(p),
                "custom prop {p} has no prop_vocab: {vocab:?}"
            );
        }
        assert!(yml.contains("THE RULE"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
