# Changelog

All notable changes to the Atelier CLI + Manager GUI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The progress window shows each step as a group of blocks that fills as the step runs: Prep,
  Download, Start and Boot for an install, update or start; Check, Database, Files and Save for a
  backup; Copy, Unpack, Database, Files and Migrate for a restore; Prepare, Render and Save for a
  static export. Actions without steps (stop, freeze, password) show one slow-moving row and how
  long they've been running. Image layers and the log are behind **Details**.
- A backup now dumps the database and packs the files as two separate steps, and a snapshot
  restore unpacks, restores the database and restores the files as three, so an error names the
  part that failed.
- The terminal numbers each step of these actions, for example `[2/4] Dumping the database…`.

### Added

- When an action fails, the progress window stays open on the step that stopped (marked red),
  opens the log, and offers **Copy report**: what ran, on which site and Atelier version, how
  long each step took, the error and the end of the log.

### Fixed

- Two backups started at the same moment (from the app and the terminal, say) no longer write
  the same file: each gets its own name, and neither packs the other's half-finished files. The
  same holds for two restores or two static exports running at once.
- A backup is only reported as saved once the archive has been read back and holds the database
  and the manifest. A truncated or empty one fails the backup instead.
- A restore checks the backup's database dump before it touches anything: a damaged file now stops
  the restore with your current database untouched.
- A backup, restore or export that fails no longer leaves its temporary files inside the
  container.

## [0.15.1] - 2026-11-10

### Fixed

- Restoring a snapshot made on an older Atelier version no longer leaves the console blank. `atelier data
  restore` now finishes, for both a `.tar.gz` snapshot and a legacy `.sql` dump, by running the
  appliance's own update step (database updates, config import, cache rebuild behind the health
  check), the same one `atelier doctor` runs, instead of only clearing caches.

### Changed

- Dependency refresh within current versions: clap 4.6.7, anyhow 1.0.104, serde_json 1.0.154,
  owo-colors 4.4.0, socket2 0.6.5, libc 0.2.191. No behaviour change.

## [0.15.0] - 2026-11-08

### Security

- **New sites get their own random database password** instead of the shared default. It is
  written to the new site's `.env` (`DB_PASSWORD`) only when Docker confirms the site has no
  database yet; if Docker can't be asked, the site keeps the default, as before. Existing sites
  keep the password their database was created with: nothing is rotated or added to an existing
  `.env`. The site's `.env` is the only source of the password: the manager ignores a
  `DB_PASSWORD` exported in your shell (for another project, say), so it can no longer override
  the site's own. If a new `.env` is written for a site whose database already exists, the
  manager says so and points at how to restore or reset the password
  (<https://aincient-labs.com/docs/hardening#if-you-lose-env>); databases from older versions
  keep working.
- **`atelier doctor` flags the shared default database password.** Sites made before per-site
  passwords (no `DB_PASSWORD` in `.env`, or the default `aincient`) now show a warning: the database
  isn't reachable from outside the stack, but a future update will rotate it. Advisory only: it
  never fails `doctor`, and `--fix` does nothing for it.
- **The edge sends baseline security headers** (`X-Content-Type-Options`, `Referrer-Policy`,
  `X-Frame-Options: SAMEORIGIN`, `Permissions-Policy`, and HSTS when the request arrived over TLS),
  including on the engine's resting page (which keeps `X-Frame-Options: DENY`) and its "engine
  unavailable" answer. A site's untouched `edge.conf` is upgraded to the new rule silently, and a
  running edge reloads it on `atelier app install` as well as on update; a hand-edited one keeps
  doctor's existing "written by an older manager" note.
- **Releases are gated on RustSec advisories** (`cargo deny check advisories`, in CI and before
  every release), and an OpenVEX file, `vex.openvex.json`, is published at the repository root.
- **quick-xml 0.42** (from 0.39.4, via plist 1.10.3): fixes RUSTSEC-2026-0194 and -0195, a
  quadratic attribute check and an unbounded namespace allocation in the XML reader the desktop app
  uses for property lists. **anyhow 1.0.103** fixes RUSTSEC-2026-0190.

## [0.14.1] - 2026-11-03

### Security

- **The web server in front of your site moves to nginx's slim image** (`nginx:1.30-alpine-slim`).
  The full image carries curl, expat and brotli, which nginx never uses but which collect most of
  its security advisories; the slim one is the same nginx without them. New installs start on it,
  and `atelier app update` moves existing ones (your `compose.yaml` edits are kept), even when
  there is no new Atelier release. `atelier doctor` warns until then.
- **The database server moves to the Debian trixie build** (`pgvector/pgvector:pg16-trixie`): the
  same PostgreSQL 16 and pgvector, on system libraries without the old image's Critical and High
  advisories. New installs start on it. `atelier app update` moves an existing site once it runs
  an Atelier version that rebuilds its database indexes for the new libraries (text sorts
  slightly differently under them); the data stays, and the first start takes a moment longer.
  `atelier doctor` warns until then.

## [0.14.0] - 2026-11-03

### Changed

- **The Manager window wears the new brand**, matching aincient-labs.com and the Atelier
  console: paper and ink, 1px rules instead of cards and shadows, square corners, IBM Plex Sans
  (bundled, still fully offline), and the new Atelier CMS mark in the window and as the app icon.
  Highlights are ink; red only means something is wrong. A site that is starting up now shows
  the amber "waiting" square instead of the green "running" one.
- **A calmer, more consistent layout.** The site you're working on now sits at the top of the
  sidebar instead of in the title bar, which holds only the Atelier CMS mark, "Manager" and the
  version. Sections are separated by lines instead of boxes, every screen opens with the same
  kind of title, and buttons, fields, dropdowns and checkboxes share one look and size.
- **The "Atelier is starting" page wears the new brand**: what your browser shows while a site
  starts or upgrades now has the Atelier CMS mark and follows your light or dark setting.
  Existing sites get it on their next update, with no doctor warning: the Manager recognises the
  starting page it shipped before and swaps it in place. An `edge.conf` you edited by hand is
  never touched.
- **The Mac app icon is a proper macOS icon**: the Atelier CMS mark on Apple's rounded-square
  tile with its soft shadow, so it sits right in the Dock and the app switcher.

## [0.13.1] - 2026-11-01

### Fixed

- `atelier sites list` says **resting** or **locked down** for a site the experimental engine
  stopped, instead of stopped, as the window's site list does.

## [0.13.0] - 2026-10-31

### Added

- **Site switcher in the Manager window.** The top bar names the site the window works on: its
  name, where it runs and whether it's running. Its menu lists every site in `sites.toml`,
  switches between them, adds a site (name + short name, next free port, then its set-up screen)
  and renames one. Every action runs against the site shown, and with more than one site each
  confirmation and progress panel names it. Switching also makes it the active site for the CLI;
  an `atelier sites use` in a terminal doesn't move an open window.
- **Sites on a server, over SSH.** `atelier sites add <slug> --host <ssh target>` (or **Add a
  site… → On a server** in the window) attaches a site that already runs on a server, using an
  SSH alias you have or a per-server key the Manager makes and keeps in the OS keychain. The
  Manager runs the server's own `atelier` CLI, copying a matching Linux build to
  `~/.local/bin/atelier` when the server has none. Status, update, backup, restore, start/stop,
  export, freeze/live, snapshots and doctor then work with `--site` or from the switcher; doctor
  says which machine each check ran on. The console opens at the address set with `--domain` /
  `atelier sites set-domain`, else through an SSH tunnel on 127.0.0.1 (ports 41300–41399).
- **Server backups come home.** A backup made on a server is copied to
  `~/.atelier/remote/<slug>/backups` and checked against the server's size and SHA-256
  (`--no-copy` keeps it on the server). A backup on this computer restores onto the server by
  upload; `--on-server` takes a path on the server. Both show each step as it happens: the server's own steps
  (dumping, packing, loading, rebuilding caches) and the Manager's (uploading or copying with
  the size, the SHA-256 check), so a minute-long backup is never a blank progress box.
- **Backups know their site.** Every new backup records the site it came from, and restore
  refuses another site's backup unless you pass `--other-site` (the window asks). Older backups
  restore with a warning. `data backup --json` prints the archive's path, size and checksum.
- **The experimental engine ships as a published image.** Each release builds
  `ghcr.io/aincient-labs/atelier-engine:<version>` (linux/amd64 and linux/arm64) from the same tag,
  and the CLI and the desktop app carry its digest: `atelier engine enable --experimental` pulls
  the image and pins it by that digest, never by a tag. Building the image on the server
  (`engine/build.sh`) remains for developer builds of the Manager, which say so when they enable.
- **On a site with the experimental engine, Start wakes it and Stop puts it to rest.** Resting
  saves a fresh snapshot first and visitors keep reading it; the window's button reads **Rest** and
  asks first, and the site list says Resting or Locked down instead of Stopped. A locked-down site
  can't be started until it is unlocked. Update, backup, restore and freeze work as before; sites
  without the engine are unchanged.

### Changed

- Every destructive confirmation names where it acts ("on this computer" or the server). On a
  server site that is running, frozen or not, you type the site's short name; `--yes` alone doesn't skip it, so
  scripts pass `--yes --confirm-slug <slug>`. `app stop` and `site freeze|use|live|prune` take
  `--yes`.
- Sites on a server are kept in `~/.atelier/sites.remote.toml`; `sites.toml` holds only the sites on
  this computer, in the shape Manager 0.12.1 reads, so going back to 0.12.1 keeps every local site.
  A `sites.toml` with server sites in it (from a pre-release build) is split the first time it is read.

- The GUI's set-up screen installs a newly added site on the port it was registered with, the
  same rule as `atelier app install`, instead of always offering 41221.
- The Publish form remembers its folder and website address per site.

### Fixed

- A background update check for one site can no longer answer for another: the single-flight
  guard is now per site.

## [0.12.1] - 2026-10-29

### Security

- **The web server in front of your site moves to nginx 1.30** (ATELIER-SA-2026-002). Every site
  installed so far runs its `edge` web server on nginx 1.28, which no longer receives security
  fixes. `atelier app update` now moves it to `nginx:1.30-alpine`, even when there is no new
  Atelier version, by changing only that one line in `compose.yaml`. Your own edits to the file
  stay. Only the web server container restarts. New installs start on 1.30.
- **`atelier doctor` warns about an unmaintained web server** ("Web server receives security
  fixes") and names the line it would move to.

## [0.12.0] - 2026-10-04

### Added

- **Several sites on one machine: `atelier sites`.** `atelier sites add blog` creates a second
  site in `~/.atelier/sites/blog` on the first free port (41222, 41223, …) and installs it. Each
  site has its own containers, database, uploaded files, port and backups. `atelier sites list`
  shows every site with its port, whether it's running and its version. `atelier sites use blog`
  picks which site commands use by default. `atelier sites rename blog "The Blog"` changes only the
  display name. `atelier sites remove blog` stops the site and takes it off the list but deletes
  nothing: its folder, database and files stay, and `atelier sites add blog` brings it back. The
  list is kept in `~/.atelier/sites.toml`. An existing `~/.atelier` joins it as the site `default`
  and stays exactly where it is, with its data.
- **`--site <SLUG>` on every command**, e.g. `atelier --site blog app status` or
  `atelier --site blog data backup`. Order of precedence: `--site`, then `ATELIER_HOME`, then the
  active site, then `~/.atelier`. Without a second site nothing changes.
- **`atelier app update --all`** updates every installed site one after the other, each along
  its own upgrade route. If one site fails, the rest still run, and the command reports which
  site failed.
- **`atelier doctor` names a port another site is set up on.** If two sites share a console port,
  only one can run at a time. Doctor now warns about this and gives the command that moves the
  site to another port. The port check also confirms it's this site's own container holding the
  port, not just any running Atelier.

- **`atelier pack new` writes a `.mcp.json`.** A fresh pack now carries an MCP config that
  points an agent opened in the pack at `atelier mcp`, so the agent finds the pack tools
  without you wiring anything up.
- **The starter component teaches the `prop_vocab` rule.** Its `prop_vocab` block now states
  the rule (every prop outside the locked vocabulary needs a one-line meaning) and has an
  entry for each custom prop the starter defines, so a pristine pack validates and you learn
  the rule by example rather than by rejection.
- The starter's own CSS is checked to be free of hardcoded colours and fractional opacity, so
  an untouched pack starts with no stylesheet warnings.

### Fixed

- **`atelier mcp` finds your pack from any folder inside it.** It now looks upward from where it
  was launched for the nearest `atelier.pack.yml`, so an editor or agent that starts it from a
  subfolder (or elsewhere under the pack) still scaffolds into the right pack. If there is no
  pack above, it says so: which file it looked for, from where, and to run it inside a pack or
  start one with `atelier pack new`.
- **`atelier app start` brings the appliance back after `atelier pack down`.** When the
  containers no longer exist, Start now creates them again (`compose up -d`) instead of failing
  with "no container found".

- **The update check can no longer hang on a flaky or missing connection** (manager#6). Before
  asking the registry anything, the Manager and the CLI (`atelier status`, `atelier update
  --check`) now check in about 2 seconds whether the registry is reachable at all. If it isn't,
  they report "You're offline — couldn't reach the registry." without starting a registry read.
  The registry read itself now stops after 10 seconds, and so does each step of an upgrade
  route's lookups.
- **Docker checks give up instead of freezing.** Every quick Docker check (`docker info`,
  `compose ps`, image and container inspects) now stops after 15 seconds, so a stalled Docker
  Desktop reads as "not running" instead of hanging. A check that times out is stopped along
  with any helper processes it started, and the message names the command and how long it ran.
  Long operations (pull, start, backup, restore, doctor's repairs) still run for as long as
  they need.
- **The Manager window no longer waits on the update check** (manager#6). The Home update
  banner now reads the last check result, saved in the stack folder (`update-check.json`). The
  Manager asks the registry again once shortly after the window opens, then only when that
  result is more than 6 hours old. Start, Stop, Backup, Restore and the other actions no longer
  start a new check. Only one check runs at a time, so a slow registry can't pile up background
  processes. **Update** on Home reuses a recent result. Otherwise it checks again with the same
  time limit, and if you're offline it says so instead of starting an update that can't download
  anything. Settings → Check for updates still checks live. Docker readiness checks and the
  backups list no longer run on the window's main thread, and the Manager's own new-version
  check gives up after 5 seconds.

## [0.11.1] - 2026-10-03

### Fixed

- **`pack new --studio` styles the studio with token names that exist.** The scaffolded
  `studio/studio.css` read the pre-0.16 names (`--ain-text`), which Atelier 0.17 no longer
  defines, plus `--ain-text-muted`, which never existed, so the studio's text lost its colour. It
  now reads `--ain-color-text` / `--ain-color-text-muted`; the contract and README examples match.

## [0.11.0] - 2026-10-03

### Added

- **`atelier pack new <name> --studio`** (experimental). Also scaffolds a hello-world console
  studio: `<name>.studios.yml`, a plain-JS `studio/studio.js` the console mounts through its
  `mount(el, ctx)` boundary, and `studio/atelier-studio.d.ts`, the typed mount contract, so an
  editor type-checks the studio out of the box. The manifest then declares `studios` and requires
  Atelier `^0.16`, the first release that mounts a pack studio. Without the flag a pack is
  unchanged.

## [0.10.0] - 2026-09-12

### Added

- **`atelier site freeze` / `snapshots` / `use` / `live` / `prune`.** Freeze & Live: the
  appliance can serve visitors a frozen snapshot of the published site (plain files, kept as
  versions) while logged-in users keep seeing the live site. These verbs are thin passthroughs
  onto the appliance's `drush aincient:freeze` family; switching is instant and nothing restarts.
  Needs an appliance image that carries the feature (cms `0.11.0` or later).
- **`edge` in the stack template.** A fresh install now lays down `compose.yaml` + `edge.conf` +
  `.env`: an nginx `edge` service is the only published port, serving a frozen snapshot to anonymous
  visitors without touching PHP and proxying everything else to `app` (cms Freeze & Live Phase 3).
  `atelier doctor` gains `stack.files_current`: a stack written by an older manager (no `edge.conf`,
  `app` publishing the port) is a warning, and `--fix` rewrites both files from the templates, keeping
  the previous ones aside; `docker compose up -d` then applies the new topology. Data volumes are
  untouched.

### Changed

- **`atelier data backup` no longer packs image-style derivatives** (`files/styles`). They are a
  cache Drupal rebuilds on first request, and they were 246 MB of a 261 MB archive on a small
  site. Restore is unchanged; the first visit to a page after a restore regenerates what it needs.
  Frozen snapshots stay in the bundle, derivatives included.

- **`data backup` bundles the frozen snapshots** (`private/frozen/`, symlink included) and
  `data restore` puts them back, so a restore also restores which snapshot visitors were seeing.

## [0.9.1] - 2026-09-03

### Fixed

- **Publish could delete the folder you picked.** The Manager's "Where to save
  it" picker passed the chosen folder straight to the exporter, which cleared
  it before copying the site in — pick `~/Downloads` and Downloads was gone.
  The site now goes into `aincient-site-export` *inside* the folder you pick,
  and the engine (GUI and `atelier site export --out`) refuses to write into
  any folder that is not empty or a previous Atelier export (identified by its
  `.aincient-export.json` marker). The new export is copied beside the old one
  and swapped in only once it is complete; nothing else on disk is ever removed.

### Changed

- `atelier pack dev` now runs an isolated per-pack appliance — its own compose
  project (`atelier-pack-<module>`), its own port, and a throwaway demo
  database — instead of switching your real site into dev mode. Pass
  `--attach` to restore the old behavior (mount the pack into your live
  appliance); `atelier pack down --purge` resets the isolated dev site.

## [0.9.0] - 2026-08-25

### Added

- The GUI now notices when a newer Manager is available and shows a quiet
  banner on Home (plus a hint in Settings) linking to the releases page. One
  GitHub API check per launch, after first render; offline or rate-limited it
  stays silent and behaves exactly as before. The Manager still never updates
  itself — the appliance banner remains about your site, this one is about the
  app.

## [0.8.0] - 2026-08-25

### Fixed
- A failed operation's error banner (e.g. an export stopped by broken links)
  now clears the moment a new operation starts, so a retry that succeeds — such
  as re-exporting with the link check skipped — no longer looks like it failed.

### Added
- The GUI's update/install log is now a real progress view: the image pull
  renders one progress bar per layer (phase, bytes downloaded, percent) plus a
  summary line, driven by Docker Compose's structured JSON progress instead of
  raw text. Working layers fill cinnabar, finished ones settle jade, warnings
  ochre, failures coral — and plain log lines (compose up, converge) are now
  colour-classified so errors and warnings stand out. Requires Compose v2.30+
  for the bars; older Compose falls back to the plain text feed. The CLI is
  unchanged and keeps Docker's native progress rendering.
- `atelier pack new|dev|down|validate` — the component-pack developer loop:
  scaffold a pack (an ordinary Drupal module carrying `thirdPartySettings.atelier`),
  run the pinned appliance image in dev mode with the pack mounted (Twig/PHP
  edits visible on refresh, a Tailwind watcher, the component gallery at
  `/atelier/packs/<module>/gallery`), and run the appliance's own admission
  gate against it. Requires an appliance image with pack support (> 0.9.2).
- `atelier mcp` — a stdio MCP server exposing the pack developer's
  ground-truth tools (catalog, gate, prompt manifest, rendered examples,
  design tokens, component scaffolding) to coding agents such as Claude Code
  and Cursor. Works against a running `atelier pack dev` stack.
- `agent_eval` MCP tool — asks the appliance's real page-agent model to compose
  against a kind's live palette and grades the answer with the live catalog
  (unknown components, prop lint, the kind's opener, an optional expected
  component). The direct answer to "the agent won't use my component". Needs an
  AI provider connected on the dev appliance, and an appliance image with the
  `/atelier/dev/agent-eval` endpoint (> 0.9.2).
- The pack scaffold's CI workflow is now the full client reference pipeline:
  CSS drift gate (arms once the committed asset is a Tailwind build), image
  build, boot against a throwaway database (`compose.ci.yaml`), the admission
  gate + kind-check + gallery smoke inside the booted appliance, and — on
  `main` — a push to `ghcr.io/<owner>/<repo>` with the `dev.atelier.extensions`
  label re-stamped from the validated image so the manager's pre-pull extension
  diff knows the pack ships. On an Atelier pin predating component packs the
  gate steps skip with an explicit notice instead of failing.

## [0.7.2] - 2026-08-10

### Fixed
- **`atelier app update --to <version>` now runs the version you asked for.** It planned the right
  image, but then pulled and started whatever the install was already configured to run — on a
  standard install, the newest release — and reported it had pinned you to *that*. So asking for a
  specific version could silently install a different one. The version you name is now written to
  the install before anything is pulled. If you used `--to` on 0.6.0–0.7.1, run `atelier app status`
  to check what you're actually on. (Multi-step upgrade routes and plain `atelier app update` were
  never affected.)

## [0.7.1] - 2026-08-08

### Fixed
- **The desktop app's installers carry the right version again.** The GUI bundles (`.dmg`, `.deb`,
  `.AppImage`, `.msi`, `.exe`, `.rpm`) had stopped tracking the release and were labelled with an
  older version than the one you downloaded — so the installed app reported the wrong number. The
  bundle version now follows the release automatically and can't drift apart from it again. (The CLI
  was always correct; this was the desktop build only.)

## [0.7.0] - 2026-08-08

### Added
- **`atelier app status` now shows the version your site is running** — the same `v0.6.1` the
  console reports, read straight off the running container. It's the line to quote in a bug report,
  and it means you no longer have to open the admin status page to find out what you're on.

### Changed
- **`atelier app update` tells you what it did.** It now says the version it's moving you from and
  to before it starts, and when there's nothing newer to fetch it says so and stops — instead of
  always reporting "Update complete." whether or not anything changed.

### Fixed
- **An update can no longer quietly move your site backwards.** Asking to update *to* an older
  version than the one installed used to run the older release against a database a newer one had
  already migrated — which breaks the site with no obvious cause and no way back. That move is now
  refused outright, pointing you at `atelier data restore` instead, which is the safe way to return
  to an earlier version. (A pinned fork or a locally built image, whose version can't be read, is
  left to the appliance's own migration refusal as before.)

## [0.6.1] - 2026-08-08

### Changed
- **Moving an old install onto released versions now asks first.** An install made before there
  were releases follows unreleased builds without ever having chosen to, and the Manager used to
  move it onto released versions the first time it adopted one — silently changing which build
  someone's existing site runs on, possibly by several versions at once. It now says what it
  found and waits for an answer, and declining costs nothing: the install stays exactly where it
  was and can be moved later with `atelier app channel stable`. A non-interactive run (a script,
  a pipe) declines rather than hanging on an answer that will never come.

### Fixed
- **Readiness is judged from a route that can actually fail.** "Your site is ready" was decided by
  asking for the front page, which is the one URL a site can serve from its page cache while every
  other page is erroring — so the console could be announced as up moments before every link on it
  returned an error. Readiness now asks for a page that cannot be cached, so it reflects whether
  the site is really rendering.

## [0.6.0] - 2026-08-07

### Changed
- **An update is now refused before it downloads if the new image is missing a module your site
  has installed.** Drupal cannot start without an installed module's code, so an update across a
  release that dropped one leaves the site unable to boot — and the fix (uninstalling the module)
  is only possible on the version you are still running. Images now publish the extensions they
  ship, which Atelier reads straight from the registry, so the check happens before the ~500 MB
  pull and names the modules to uninstall first. It only ever acts on a definite answer: an older
  image that publishes no list, or a site it cannot read, means the update proceeds exactly as
  before, and the appliance's own check at boot remains the backstop.
- **Edge is demoted to a testing channel.** It ships unreleased work off main and was one menu
  selection away in the desktop app, which is not where a site you care about should be able to
  end up. The Updates menu now offers released versions only; an install already on edge still
  shows edge, and can move back. `atelier app channel edge` keeps working, behind a confirmation
  that says what it costs. Nothing changes for installs already following either channel.

### Fixed
- **A backup now records the exact image it was taken from, not just the channel it was following.**
  The snapshot manifest stored the configured image reference — and on a moving channel (`edge`,
  `latest`) that names no particular build, so by the time you need the manifest it can point at a
  different release than the one your data ran on. It now records the image's **digest** and version
  alongside it, which identify one build permanently. Restoring a snapshot that came from a
  different image than the one installed prints a warning naming both, plus the
  `atelier app install --image …@sha256:…` that puts the data back on the build it is known to run
  on. It is a warning, not a refusal: restoring onto a different image is a legitimate recovery
  move. Backups made by earlier versions restore exactly as before, without the warning — they
  simply have no digest to compare.

## [0.5.0] - 2026-08-04

### Added
- **The Publish tab hands you to a host instead of stopping at a folder.** Exporting used to end
  with a path and a link to the docs — the last thing you see, at the moment you most want to be
  finished, was a directory. The result now carries the next step with it: Netlify Drop,
  Cloudflare Pages, and our deploy template each get a row, and each button reveals the export
  folder *and* opens the host in your browser, so the files are sitting there ready to drag. The
  export location gained a copy button, and a disclosure holds the ready-made
  `netlify deploy --prod --dir="…"` for people who'd rather type it. No accounts, tokens or
  credentials are involved — Atelier still stores nothing about your host.
- **An export made without a website address now says what that costs.** Leaving the address blank
  makes the exporter render against the address the appliance runs on locally, so page links,
  social previews and the sitemap all point at your own computer — silently, and only visible once
  the site was live. The result panel now flags it and points back at the field.
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

[Unreleased]: https://github.com/aincient-labs/manager/compare/v0.15.1...HEAD
[0.15.1]: https://github.com/aincient-labs/manager/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/aincient-labs/manager/compare/v0.14.1...v0.15.0
[0.14.1]: https://github.com/aincient-labs/manager/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/aincient-labs/manager/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/aincient-labs/manager/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/aincient-labs/manager/compare/v0.12.1...v0.13.0
[0.11.0]: https://github.com/aincient-labs/manager/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/aincient-labs/manager/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/aincient-labs/manager/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/aincient-labs/manager/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/aincient-labs/manager/compare/v0.7.2...v0.8.0
[0.6.0]: https://github.com/aincient-labs/manager/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/aincient-labs/manager/compare/v0.4.0...v0.5.0
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
