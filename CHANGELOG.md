# Changelog

All notable changes to the Atelier CLI + Manager GUI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/aincient-labs/manager/compare/v0.2.7...HEAD
[0.2.7]: https://github.com/aincient-labs/manager/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/aincient-labs/manager/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/aincient-labs/manager/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/aincient-labs/manager/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/aincient-labs/manager/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/aincient-labs/manager/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/aincient-labs/manager/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/aincient-labs/manager/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aincient-labs/manager/releases/tag/v0.1.0
