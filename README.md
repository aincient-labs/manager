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
(`doctor` is the one flat, universal command):

| Command                  | What it does                                                              |
| ------------------------ | ------------------------------------------------------------------------- |
| `app install`            | Lay down `~/.atelier/{compose.yaml,.env}`, pull, `up -d`. Idempotent.     |
| `app update`             | `pull` + `up -d` — `converge.sh` migrates in place and auto-rolls-back. Steps through intermediate versions when one hop can't get there (`--to X.Y.Z`, `-y`). |
| `app check-update`       | Compare the local image digest against the registry tag, and show the route the update would take. (alias `app check`) |
| `app channel [stable\|edge]` | Show or switch which images the install follows. `--now` pulls immediately. |
| `app reinstall`          | Wipe volumes and install fresh (destructive, confirmed).                  |
| `app status`             | Read-only health probe. (The full checkup is the flat `doctor`.)          |
| `app start`/`stop`/`down`/`logs`/`open`/`password` | Everyday stack management.              |
| `site export`            | Export the public site to static HTML — the deploy-anywhere artifact.      |
| `data backup`            | Portable `.tar.gz` snapshot (DB dump + uploaded files + manifest) → `~/.atelier/backups`. (alias `data export`) |
| `data restore <file>`    | Restore a `.tar.gz` snapshot (DB + files, re-chowned) or a legacy `.sql`/`.sql.gz` dump (DB only). (alias `data import`) |
| `data list`              | List snapshots on this host. (alias `data backups`)                       |
| `ai model list`/`set`    | Inspect or bind the AI model per Atelier role.                            |
| `doctor`                 | Diagnose host + stack + site. `--fix` repairs safely, `--json` for bug reports. |

`export`/`import` alias `data backup`/`restore` so either mental model works; `export` is
never a bare top-level verb (it means one of three things — static site, db+files, db-only —
so it's always qualified by its namespace). The stack directory defaults to `~/.atelier`
(override with `ATELIER_HOME`) and holds the same `compose.yaml` + `.env` the
`docker/install.sh` bootstrapper writes.

### Channels

`AINCIENT_IMAGE` decides everything Docker does, so a "channel" is just a policy about which tag
it names: **stable** = `:latest` (retagged on every release, the default), **edge** = `:edge`
(rebuilt on every merge to main), and anything else — a `:vX.Y.Z` tag, a digest, a local build —
is **pinned** and never moves, so no update can arrive through it.

`.env` also carries `AINCIENT_CHANNEL`, which records the channel the operator *chose* rather than
the one they're on. It exists for exactly one reason: installs made before there were releases
have `:edge` baked into their `.env` because edge was all there was, and they need to be told apart
from an operator who asked for unreleased builds. `install`/`update` move the former onto stable
once (snapshotting first — edge can be *ahead* of the newest release, and Drupal only migrates
forward), announce it, and write the marker so it can never fire twice.

### Stepped upgrades

A release can be unable to migrate arbitrarily old state. The concrete case: the update
that uninstalls a module must run while that module is still in the image, so the release
that finally drops it from `composer.json` can only migrate sites that already ran it.
Every appliance image therefore declares the oldest version it can migrate from, as the
OCI label `dev.atelier.upgrade.min-from` (see `docker/upgrade-floor` in the cms repo).

`app update` reads those labels **straight from the registry, pulling nothing**, walks them
backwards from the target until it reaches a version the install satisfies, and shows the
route before doing anything:

```
$ atelier app update
This upgrade takes more than one step

  from 0.1.1
  1. 0.3.0   (0.4.0 can't migrate a site this old directly)
  2. 0.4.0

Each step pulls its image, migrates, and is health-checked before the next one starts;
a step that fails rolls its own database back. A full snapshot is taken first.

Walk the whole route now? [y/N]:
```

Each hop must finish converging before the next begins — the floor is checked against the
version the site *recorded* on its last successful converge, so running ahead of one would
present the next image with two-versions-ago state and earn its refusal. A hop that doesn't
come up stops the route and says so; the site is left on the last version that converged,
which is a state the appliance guarantees (converge rolls its own database back on failure).
`app check-update` reports the same route, so a long upgrade is visible before it's started.

The manager is the ergonomics, not the safety: the appliance refuses an impossible migration
itself, before touching the database, so a hand-run `docker compose pull` fails safely with a
message naming the version to go through. That also means an unreadable registry or an
unstamped local image costs the *route*, not the safety — the plan falls back to one direct
hop and says why it wasn't verified.

`--to X.Y.Z` stops at a specific version (which leaves the install **pinned** to it —
`app channel stable` resumes following releases). `-y` skips the confirmation.

### `doctor`

`atelier doctor` is read-only and checks three tiers in dependency order, so a failure never
masquerades as the tier below it (a check whose prerequisite failed reports *skipped*, not
*failed*):

| Tier | Checks |
| ---- | ------ |
| **host**  | Docker installed/running, Compose plugin, `buildx` (only the update check needs it) — each reported **with its version**, the CLI and the engine separately since they can differ — plus whether another program has taken the console port, and free disk space. |
| **stack** | `compose.yaml` present and parseable, `.env` still has its `HASH_SALT`, both containers exist, neither is restart-looping or unhealthy. |
| **site**  | Drupal bootstraps, no pending `updatedb`, uploaded-files tree still owned by `www-data`, the image's own `healthcheck.sh` passes, a model role is bound. |

`atelier doctor --fix` climbs a **safe repair ladder**, re-diagnosing after each rung and stopping
the moment nothing is failing — so a site that only needed a cache rebuild never gets a full
self-heal run:

1. restore the stack files (a replaced `compose.yaml` is moved aside, never deleted)
2. `docker compose up -d`, then wait for the console to actually serve
3. `drush cache:rebuild`
4. `drush updatedb -y`
5. `chown -R www-data:www-data` on the files tree
6. re-run the appliance's `converge.sh` — which snapshots the database first and rolls back if
   the repair makes things worse

Every rung's worst case is lost time. **`down -v`, reinstall and restore are excluded at any
flag** — doctor names them as the next step and stops. Exit code is non-zero while anything is
still failing (advisories, like "no model connected yet" on a fresh install, don't count), so it
works as a gate in a script. `--json` emits the whole report plus the verdict and manager version,
for pasting into an issue.

> **v0.2.0 renamed the flat commands into these namespaces** (BC break vs v0.1.0): `install` →
> `app install`, `backup` → `data backup`, `model set` → `ai model set`, and so on. `doctor`
> is unchanged.

### Network calls the Manager makes on its own

On launch, the GUI asks GitHub (`api.github.com`) whether a newer Manager release exists, so it
can show a quiet "get the update" link when this app itself is stale. Nothing about you or your
install is sent — no ID, no version, no OS details, just the request itself — and **we receive no
data**: GitHub doesn't share API request logs with repository owners, so this can't even count
installs. Offline or rate-limited, the check silently does nothing. The Manager never updates
itself; the appliance update banner is about your site, this link is about the app. (The
appliance's own `check-update` talks to the image registry the same way — a version lookup,
nothing more.)

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
