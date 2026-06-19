# AIncient manager

The installer/manager for an AIncient CMS appliance. One shared Rust engine, two
front-ends:

- **`aincient` CLI** (`crates/aincient-cli`) — the developer/power-user front door,
  distributed via Homebrew and cross-platform installers.
- **Tauri GUI** (`app/`) — a double-click manager for non-technical users. Plain
  HTML/CSS/JS frontend (no bundler) over the same core, via Tauri commands.

Both link **`aincient-core`** (`crates/aincient-core`) directly, so there is exactly one
implementation of every operation and no duplicated logic.

## Why this exists

Install friction is the biggest leak in the evaluator funnel. The real prerequisite floor is
**Docker** (a CMS needs PHP + a database + storage), not the terminal — so the manager is a
*lifecycle manager over the existing appliance*, not a way to skip prerequisites. Every
operation maps to a `docker compose` / `drush` primitive the appliance already uses:

| Command          | What it does                                                              |
| ---------------- | ------------------------------------------------------------------------- |
| `install`        | Lay down `~/.aincient/{compose.yaml,.env}`, pull, `up -d`. Idempotent.     |
| `update`         | `pull` + `up -d` — `converge.sh` migrates in place and auto-rolls-back.    |
| `check-update`   | Compare the local image digest against the registry tag.                  |
| `backup`         | `drush sql:dump --gzip` in the container, copied to `~/.aincient/backups`. |
| `restore <file>` | Drop, load the dump, rebuild caches (mirrors converge's `restore_snapshot`).|
| `reinstall`      | Wipe volumes and install fresh (destructive, confirmed).                  |
| `status`/`doctor`| Read-only health and Docker-readiness probes.                             |
| `start`/`stop`/`down`/`logs`/`open`/`password` | Everyday stack management.                  |

The stack directory defaults to `~/.aincient` (override with `AINCIENT_HOME`) and holds the
same `compose.yaml` + `.env` the `docker/install.sh` bootstrapper writes.

## Develop

```bash
cargo build            # build the workspace
cargo test             # run unit tests
cargo clippy           # lint
cargo run -p aincient-cli -- doctor   # run the CLI
cargo run -p aincient-manager         # launch the GUI
```

The built CLI binary is `target/debug/aincient` (or `target/release/aincient`); the GUI
binary is `target/debug/aincient-manager`. Packaging the GUI into a signed `.dmg`/`.msi`/
AppImage (via `cargo tauri build`) and code-signing/notarization are deferred.
