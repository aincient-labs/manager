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
binary is `target/debug/atelier-manager`. The GUI is packaged into `.dmg`/`.msi`/AppImage
by `cargo tauri build` (in CI, `release-gui.yml`); the macOS bundle is Developer ID signed
and notarized when the Apple secrets are configured (see "macOS code signing" below).

## Release / distribution

**One tag ships everything.** CLI and GUI share the workspace version and ride the **same `v*`
tag**, so they can never drift apart:

```bash
git tag v0.2.2 && git push origin v0.2.2
```

That fans out to two workflows on the one tag:

- **`release.yml`** (cargo-dist, driven by `dist-workspace.toml`) builds the `atelier` CLI for
  every target, creates the GitHub Release, and publishes the Homebrew formula
  (`Formula/atelier.rb`) to `aincient-labs/homebrew-tap` so `brew install aincient-labs/tap/atelier`
  works.
- **`release-gui.yml`** (`tauri-apps/tauri-action`) builds the desktop bundles
  (`.dmg`/`.msi`/`.AppImage`/`.deb`/`.rpm`) — which cargo-dist can't produce — and uploads them
  onto the *same* release cargo-dist creates for that tag.

Keep `app/tauri.conf.json`'s `version` in lockstep with the workspace `Cargo.toml` version when
bumping (the GUI release name is derived from it).

**Homebrew-tap prerequisite:** the cross-repo formula push needs a `HOMEBREW_TAP_TOKEN` Actions
secret — a token with `Contents: read and write` on `aincient-labs/homebrew-tap` (the built-in
`GITHUB_TOKEN` can't push to another repo). Note the **`aincient-labs` org rejects fine-grained
PATs whose lifetime exceeds 366 days** — if `publish-homebrew-formula` fails at the tap checkout,
rotate the secret to a ≤366-day token. (Recovery when it fails: the formula is also attached to
the release as `atelier.rb`, so it can be pushed to the tap by hand.)

### macOS code signing

The macOS bundle is signed with a **Developer ID Application** certificate and **notarized** via the
App Store Connect API. `release-gui.yml` does this automatically **when the secrets below are set**;
with them absent the build falls back to an unsigned bundle (Gatekeeper warns on first launch), so the
workflow is safe to run either way. Windows Authenticode is still a fast-follow.

One-time Apple-side setup (all from [developer.apple.com](https://developer.apple.com/account)):

1. **Certificate** — create a *Developer ID Application* certificate, then export it from Keychain
   Access as a `.p12` (with a password). Base64 it for the secret:
   `base64 -i cert.p12 | pbcopy`.
2. **Signing identity** — the cert's full name, e.g.
   `Developer ID Application: Your Name (TEAMID)` (copy from Keychain Access or `security find-identity -v -p codesigning`).
3. **Notarization key** — App Store Connect → Users and Access → Integrations → App Store Connect API
   → generate a key with the **Developer** role. Download the `.p8` (one-time download), and note the
   **Key ID** and **Issuer ID**. Base64 it for the secret: `base64 -i AuthKey_XXXX.p8 | pbcopy`.

Then add these repo Actions secrets (Settings → Secrets and variables → Actions):

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | base64 of the `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password you set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: … (TEAMID)` |
| `APPLE_API_ISSUER` | Issuer ID (UUID) from App Store Connect |
| `APPLE_API_KEY` | Key ID (e.g. `XXXXXXXXXX`) |
| `APPLE_API_KEY_BASE64` | base64 of the `.p8` key (workflow decodes it to a file) |
| `KEYCHAIN_PASSWORD` | *(optional)* any string; auto-generated if unset |
