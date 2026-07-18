# Atelier manager

The installer/manager for an Atelier CMS appliance. One shared Rust engine, two
front-ends:

- **`atelier` CLI** (`crates/aincient-cli`) — the developer/power-user front door,
  distributed via Homebrew and cross-platform installers.
- **Tauri GUI** (`app/`) — a double-click manager for non-technical users. Plain
  HTML/CSS/JS frontend (no bundler) over the same core, via Tauri commands.

Both link **`aincient-core`** (`crates/aincient-core`) directly, so there is exactly one
implementation of every operation and no duplicated logic.

## Why this exists

Install friction is the biggest leak in the evaluator funnel. The real prerequisite floor is
**Docker** (a CMS needs PHP + a database + storage), not the terminal — so the manager is a
*lifecycle manager over the existing appliance*, not a way to skip prerequisites. Every
operation maps to a lower-level primitive the appliance already provides:

Commands are grouped into noun namespaces so the surface stays maintainable as it grows
(`doctor` is the one flat, universal preflight):

| Command                  | What it does                                                              |
| ------------------------ | ------------------------------------------------------------------------- |
| `app install`            | Lay down `~/.atelier/{compose.yaml,.env}`, pull, `up -d`. Idempotent.     |
| `app update`             | `pull` + `up -d` — `converge.sh` migrates in place and auto-rolls-back.    |
| `app check-update`       | Compare the local image digest against the registry tag. (alias `app check`) |
| `app reinstall`          | Wipe volumes and install fresh (destructive, confirmed).                  |
| `app status`             | Read-only health probe. (Docker readiness is the flat `doctor`.)          |
| `app start`/`stop`/`down`/`logs`/`open`/`password` | Everyday stack management.              |
| `site export`            | Export the public site to static HTML — the deploy-anywhere artifact.      |
| `data backup`            | Portable `.tar.gz` snapshot (DB dump + uploaded files + manifest) → `~/.atelier/backups`. (alias `data export`) |
| `data restore <file>`    | Restore a `.tar.gz` snapshot (DB + files, re-chowned) or a legacy `.sql`/`.sql.gz` dump (DB only). (alias `data import`) |
| `data list`              | List snapshots on this host. (alias `data backups`)                       |
| `ai model list`/`set`    | Inspect or bind the AI model per Atelier role.                            |

`export`/`import` alias `data backup`/`restore` so either mental model works; `export` is
never a bare top-level verb (it means one of three things — static site, db+files, db-only —
so it's always qualified by its namespace). The stack directory defaults to `~/.atelier`
(override with `ATELIER_HOME`) and holds the same `compose.yaml` + `.env` the
`docker/install.sh` bootstrapper writes.

> **v0.2.0 renamed the flat commands into these namespaces** (BC break vs v0.1.0): `install` →
> `app install`, `backup` → `data backup`, `model set` → `ai model set`, and so on. `doctor`
> is unchanged.

## Develop

```bash
cargo build            # build the workspace
cargo test             # run unit tests
cargo clippy           # lint
cargo run -p atelier -- doctor        # run the CLI
cargo run -p atelier-manager          # launch the GUI
```

The built CLI binary is `target/debug/atelier` (or `target/release/atelier`); the GUI
binary is `target/debug/atelier-manager`. Packaging the GUI into a signed `.dmg`/`.msi`/
AppImage (via `cargo tauri build`) and code-signing/notarization are deferred.

## Release / distribution

`dist-workspace.toml` drives cargo-dist: a tagged `v*` release builds the `atelier` CLI for
every target and publishes a Homebrew formula (`Formula/atelier.rb`) to the
`aincient-labs/homebrew-tap` repo, so `brew install aincient-labs/tap/atelier` works.

**Prerequisite:** the cross-repo publish needs a `HOMEBREW_TAP_TOKEN` Actions secret on this
repo — a token with `Contents: read and write` on `aincient-labs/homebrew-tap` (the built-in
`GITHUB_TOKEN` can't push to another repo). Without it the `publish-homebrew-formula` job fails.

### Desktop GUI

The Tauri GUI ships on its own lane (`.github/workflows/release-gui.yml`, via
`tauri-apps/tauri-action`) — cargo-dist only distributes the raw CLI binary, so the GUI needs
Tauri's own bundler for `.dmg`/`.msi`/`.AppImage`. Push a **`gui-v*`** tag (a namespace kept
separate from the CLI's `v*` tags) to build all three and attach them to a **draft** GitHub
Release:

```bash
git tag gui-v0.1.0 && git push origin gui-v0.1.0
```

The bundles are currently **unsigned** — macOS Gatekeeper / Windows SmartScreen warn on first
launch. Code-signing + notarization (Apple Developer ID + Windows Authenticode, supplied as
Actions secrets) is a deliberate fast-follow before the GUI is promoted to non-technical users.
