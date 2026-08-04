# Changelog

All notable changes to the Atelier CLI + Manager GUI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Every desktop bundle now ships a `.sha256` checksum sibling.** The `.dmg`, `.msi`, `.exe`,
  `.AppImage`, `.deb` and `.rpm` had none — only cargo-dist's CLI archives did — so a download
  page could not publish a verifiable hash for the app most people actually install. The hash is
  computed on the runner that built the bundle (a checksum derived by re-downloading the asset
  attests nothing but "GitHub served me these bytes") and uses cargo-dist's exact format, so one
  `shasum -a 256 -c <file>.sha256` verifies artifacts from either release lane.

## [0.4.0] - 2026-08-04

### Added
- **`app update` steps through intermediate versions when one hop can't get there.** A release
  can be unable to migrate arbitrarily old state: the update that uninstalls a module has to run
  while that module is still present, so the release that finally drops it can only migrate sites
  that already ran it. The appliance now declares the oldest version it can migrate from and
  refuses anything older before touching the database — which is safe, but on its own leaves you
  to work the route out by hand.
  - `atelier app update` reads those declarations **from the registry, pulling nothing** (the OCI
    label `dev.atelier.upgrade.min-from`), walks them backwards from the target until it reaches a
    version your install satisfies, and shows the route — `from 0.1.1 → 0.3.0 → 0.4.0`, with the
    reason each waypoint is there — before anything happens. Confirm and it walks the whole thing;
    `-y` skips the prompt.
  - A **full snapshot** is taken before the first hop, and each hop pulls, migrates and is
    health-checked before the next one starts (it has to be: the floor is checked against the
    version the site recorded on its last successful converge). A hop that doesn't come up stops
    the route and says where it stopped, rather than pressing on into a refusal.
  - `app check-update` reports the same route, so a five-minute upgrade is visible before you
    start it rather than during. Also in the GUI: **Update** asks first when the route has more
    than one step, and Settings → Updates says how many.
  - New `app update --to X.Y.Z` stops at a specific version. It leaves the install **pinned** to
    that version — said out loud, with the command to resume following releases.
  - Reading either the registry or your installed image can fail, and neither is treated as a
    reason to block an update: the plan falls back to one direct hop and explains that the route
    wasn't verified. The appliance's own refusal is the safety net, and it costs no data.
- **Update channels: new installs follow released versions, and you can choose.** The
  appliance image publishes two moving tags — `:latest`, retagged on every release, and
  `:edge`, rebuilt on every merge to main — but the manager only ever pointed at `:edge`,
  because when it was written there were no releases to point at. That is no longer true, so
  the default is now **stable** (`:latest`): released versions only.
  - `atelier app channel` says which channel you're on and what the alternative is;
    `atelier app channel stable|edge` switches, and `--now` pulls and converges immediately
    instead of waiting for your next update.
  - `atelier app install --channel stable|edge` picks one at install time. `--image` still
    pins one exact image (and is now reported as such: a pinned image can never receive an
    update, which `check-update`'s cheerful "you're on the latest" used to hide).
  - `app status` and `doctor` both name the channel — a pasted report now answers "why hasn't
    this updated" and "why did this change under me" without a second command.
  - In the GUI: **Settings → Your installation → Updates**.
- **Existing installs are moved onto released versions, once.** An install still carrying the
  old default `:edge` never *chose* unreleased builds, so `install`/`update` move it to
  `:latest`, say so, and tell you how to go back. It takes a full snapshot (database + files)
  first: edge can be ahead of the newest release and a Drupal database only migrates forward,
  so this direction is the risky one. A deliberate `--channel edge`, an `--image` pin, or a
  channel you picked yourself is recorded in `.env` (`AINCIENT_CHANNEL`) and left alone — the
  move can only ever happen to an install that never expressed a preference, and only once.

### Fixed
- **Installs made before the appliance's image was renamed can find updates again.** The public
  image moved from `ghcr.io/aincient-labs/atelier` to `…/atelier-cms` when the repository behind
  it was renamed, and the old name does not merely go stale — it stops answering entirely. Anything
  installed by manager ≤ v0.2.0 (or the installer of that era) has the retired name written into
  its `.env`, and nothing re-reads the default, so **every update check on those installs failed
  permanently** while the site itself carried on running fine. Reported as
  [#1](https://github.com/aincient-labs/manager/issues/1); the earlier fix there made the failure
  legible ("Couldn't reach the registry… 403 Forbidden") but not survivable.
  - `install` and `update` now rewrite the retired name to the current one, keeping whatever tag
    or digest it named, and say so. Unlike the channel move this is not guarded by your recorded
    channel: a repository that no longer answers is a dead pointer, not a preference. An install
    that also predates channels is then moved onto stable by the existing migration, in the same
    run.
  - `check-update` asks the registry about the name an update would actually come from, while
    still asking your machine about the image it is actually running — so the check works before
    any repair has been applied, rather than reporting "install Atelier first" to someone whose
    site is up.
  - `doctor` reports the retired name as a warning (the site runs, which is why this goes
    unnoticed) and `doctor --fix` repairs it without an update having to be attempted first.

## [0.3.0] - 2026-08-04

### Added
- **`atelier doctor` now diagnoses the whole appliance, and repairs it with `--fix`.**
  It used to check three things about your machine (Docker installed, running, Compose
  plugin) and stop — everything that could actually go wrong with a *site* was invisible
  to it. It now checks three tiers in dependency order and reports each with the fix
  right underneath:
  - **Your computer** — Docker (CLI **and** engine version, which routinely differ under a
    remote context, OrbStack or colima), the Compose plugin, the `buildx` plugin (previously
    only discoverable by watching an update check fail), whether another program has taken the
    console's port, and free disk space. Each of the four tools reports **its version**, not
    just a tick — "Docker installed ✓" is the same line on every machine and tells a bug report
    nothing, whereas the version is what separates a host where something works from one where
    it doesn't.
  - **Atelier itself** — the stack files are present and `compose.yaml` parses, `.env`
    still has its `HASH_SALT`, both containers exist, and neither is stuck in a restart
    loop (a crash-looping container reports "running" at any single glance).
  - **Your site** — Drupal boots, no database updates are left pending, the uploaded-files
    directory is still owned by the web server, the appliance's own health check passes,
    and a model is bound.

  A tier whose prerequisite failed reports as *skipped*, never as failed — "we couldn't
  look" and "we looked and it's broken" are different answers.

  `atelier doctor --fix` climbs a safe repair ladder, re-checking after each rung and
  stopping the moment the appliance is healthy — so a site that only needed a cache
  rebuild never gets a full self-heal run: restore the stack files → start the containers
  → `drush cache:rebuild` → run pending database updates → repair file ownership → re-run
  the appliance's `converge.sh` (which snapshots the database first and rolls back if the
  repair makes things worse). **Nothing in the ladder deletes data.** Removing, reinstalling
  and restoring stay where they were, behind their own confirmations; doctor only names
  them as the next step.
- **`atelier doctor --json`** — the whole report, machine-readable, with the verdict and
  the manager version already computed. Paste it into a bug report instead of prose.
- **A Troubleshoot panel in the Manager GUI** (System → Troubleshoot): the same checkup
  and the same repairs, for the people who can't run a CLI. Repairs stream their progress
  through the existing overlay, since a self-heal run can take minutes.
- Doctor exits non-zero when something is still failing, so it works as a gate in a
  script. Advisories (no model connected yet on a fresh install) never fail the exit code.
- **The Manager GUI shows its own version in the window chrome**, beside the "Manager" label —
  so it lands in *any* screenshot, including ones taken from the Docker-not-ready and first-run
  screens, where a bug report is most likely to start. It's compile-time constant, needs neither
  Docker nor an installed stack, and is stamped before anything else can fail. Also listed in
  **System → Your installation**, where the existing "Version" row is now labelled **Atelier** —
  with two versions on screen, neither could stay named just "Version": the Atelier row is what
  your *site* runs, the Manager row is this app, and a useful bug report usually names both.

## [0.2.10] - 2026-08-04

### Fixed
- **"Check for updates" now says what it found, and why it couldn't.** It used to
  answer *"Couldn't check right now. Make sure your site is running and you're
  online."* for every inconclusive outcome, which named two things that aren't the
  cause (the check never needs the site running, and the image is public — the CLI's
  equivalent hint asked whether you were logged in). Both digest probes ran through a
  helper that discarded stderr and the exit code, so the failure was unattributable
  even from a bug report. The four real causes are now told apart and each phrased with
  its own fix: Docker unavailable, the image not pulled yet, **Docker's `buildx` plugin
  missing** (the likely one — it reads the registry, ships with Docker Desktop but is a
  separate package on Linux, and nothing else in the manager needs it), and the registry
  unreachable (carrying the underlying error). Reported as
  aincient-labs/atelier-cms#7.

### Added
- **The update check names versions, not just digests.** It now reads the image's
  `org.opencontainers.image.version` label — locally via `docker image inspect` and
  from the registry via `imagetools`, neither of which starts a container — so both
  front-ends can say "Version v0.1.2 is available (you're on v0.1.1)" instead of a bare
  "an update is available". On a rolling tag like `:edge` the digest changes on every
  build, which made the old digest-only message unreadable. This is the first consumer
  of the image stamp added in the appliance.

### Changed
- **Fresher, warmer GUI that reads as the same shop as the marketing site.** The
  manager now shares the homepage's exact warm-paper palette (a bright `#FDFCFA`
  ground with pure-white raised cards, in place of the greyer ground it used to
  wear), bundles **Schibsted Grotesk** as the body/UI font (it previously fell
  back to bare `system-ui`, which made the app feel flat next to the site), and
  adopts the site's two-layer "light falls" diffuse shadows so cards lift off the
  paper. Light and dark tokens are taken verbatim from
  `apps/website/site/assets/css/styles.css`; the brand laws are unchanged (one
  cinnabar pigment, Fraunces display, the 4/7/10/14 radius family).

### Added
- **`SECURITY.md`** — a security policy with a private vulnerability-reporting channel
  (GitHub Security Advisories), supported-version and disclosure statements, and signing/
  verification notes. Part of Phase 0 launch-readiness.

### Fixed
- **Confirmation dialogs were completely non-functional in the GUI** — the
  "Reset & remove" actions (Reinstall from scratch, Remove the containers) and
  Restore-backup all pop a confirm modal that could neither be confirmed nor
  cancelled, and the underlying operation never ran (no feedback, nothing reset).
  The modal's Cancel button carried a `data-action` but no `id`, while
  `confirmModal()` wires both buttons by `id` (`$("confirm-no")`), so setup threw
  a `TypeError` on `null` inside the Promise executor and the promise rejected
  before any click could resolve it. Cancel now has `id="confirm-no"` (and the
  dead `data-action` attributes, which had no handlers, were removed). Regression
  from the v0.2.6 type-to-confirm redesign.

## [0.2.8] - 2026-07-19

### Added
- **"Edit my site" now signs you in.** A fresh appliance mints a random admin
  password that the manager never shows, so the old "Open my website" button
  (which pointed at the `/atelier` console) landed a logged-out operator on an
  access-denied page. The Home screen now has two doors: **Edit my site** mints
  a one-time login link (`drush user:login`, via a new `open_console_authed`
  op / Tauri command) and drops you into the console authenticated, and **View
  my site** opens the public front page (anonymous-viewable, no login). The
  address shown is now the public site root. CLI parity: `atelier app open`
  opens the console signed in; new `atelier app view` opens the public site.

### Changed
- GitHub release notes now lead with the **desktop app** download (macOS `.dmg`
  first, then Linux/Windows bundles), mirroring the install order on
  aincient-labs.com. cargo-dist's CLI/Homebrew instructions move into a
  "Prefer the terminal?" section below. Automated by a new `lead-with-desktop-app`
  job in `release-gui.yml` that builds the links from the release's real assets.

## [0.2.7] - 2026-07-19

### Fixed
- GUI docs/guide links now open in the browser. The Tauri WebView blocks
  `target=_blank`, so links are routed through a new `open_url` command.
- Reinstall no longer resets a custom port or image back to defaults — the
  existing tunables are preserved when the stack is re-scaffolded.
- `ATELIER_HOME` now isolates stacks. The Compose project name is derived from
  the stack directory instead of a hardcoded `name: atelier`, so multiple homes
  no longer share containers/volumes (last-install-wins collision).

## [0.2.6] - 2026-07-19

### Added
- GUI static site export, activity/logs, and remove/teardown — closing the
  GUI-vs-CLI capability gap (the engine already lived in `aincient-core`).

### Changed
- GUI redesigned around first-time website owners: full-width
  "Atelier · by AIncient Labs" header lockup and a left sidebar
  (Home · Publish · Backups · System) replacing the button wall.
- AI setup is deliberately hidden in the GUI (done inside Atelier onboarding);
  the CLI `ai model` command stays.
- Publish address is promoted and remembered across sessions.

## [0.2.5] - 2026-07-19

### Fixed
- GUI detects OrbStack / Docker Desktop when launched from Finder or the Dock.
  A `.app` inherits launchd's minimal `PATH`, hiding Docker installs; all docker
  invocations now route through one builder that appends the well-known install
  dirs (`~/.orbstack/bin`, `/usr/local/bin`, `/opt/homebrew/bin`, …).

## [0.2.4] - 2026-07-19

### Fixed
- macOS GUI bundle upload in the release workflow (bash 3.2 `mapfile`, skips the
  `.app` directory, per-file upload with loud diagnostics).

## [0.2.3] - 2026-07-19

### Fixed
- GUI bundle upload on macOS and Windows in the release workflow.

## [0.2.2] - 2026-07-19

### Added
- macOS GUI bundle is now signed with a Developer ID Application certificate and
  notarized via the App Store Connect API key — installs with no "unidentified
  developer" wall.

## [0.2.1] - 2026-07-18

### Fixed
- Post-0.2.0 fixes and release-pipeline adjustments.

## [0.2.0] - 2026-07-18

### Added
- Namespaced CLI surface (`app` / `site` / `data` / `ai`).
- `atelier site` static export — publish the running appliance as a portable
  static site.

## [0.1.0] - 2026-07-17

### Added
- First stable release of the Atelier CLI + Manager over the shared
  `aincient-core` Rust core: install / update / backup / restore lifecycle for
  the Docker appliance, plus the Tauri GUI.
- Distributed via `cargo-dist` → GitHub Releases and the
  `aincient-labs/homebrew-tap` (`brew install aincient-labs/tap/atelier`).

[Unreleased]: https://github.com/aincient-labs/manager/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/aincient-labs/manager/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/aincient-labs/manager/compare/v0.2.10...v0.3.0
[0.2.10]: https://github.com/aincient-labs/manager/compare/v0.2.9...v0.2.10
[0.2.9]: https://github.com/aincient-labs/manager/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/aincient-labs/manager/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/aincient-labs/manager/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/aincient-labs/manager/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/aincient-labs/manager/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/aincient-labs/manager/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/aincient-labs/manager/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/aincient-labs/manager/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/aincient-labs/manager/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/aincient-labs/manager/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aincient-labs/manager/releases/tag/v0.1.0
