# __LABEL__

An [Atelier CMS](https://aincient-labs.com) component pack — an ordinary Drupal
module whose components carry `thirdPartySettings.atelier`, admitted to the
page agent's palette through the same gate Atelier's own components pass.

## Develop

    atelier pack dev

Brings up the pinned Atelier image with this pack mounted, dev PHP/Twig
overlays (edit a Twig file, refresh, see it — no restart), a Tailwind watcher
over `build/input.css`, and the dev endpoints:

- Gallery: `http://localhost:41221/atelier/packs/__MODULE__/gallery` — every
  declared `examples:` entry at three widths, light and inverted tone.
- `atelier pack validate` — the admission gate + CSS lint + `atelier.pack.yml`
  check, exactly what a boot runs.
- `atelier mcp` — a stdio MCP server for your coding agent (catalog, gate,
  prompt manifest, rendered examples, design tokens). Point Claude Code or
  Cursor at it:

      { "mcpServers": { "atelier": { "command": "atelier", "args": ["mcp"] } } }

`atelier pack dev` also syncs `build/atelier/` — the token/utility preset from
the exact image version you develop against. Commit it: CI rebuilds your CSS
against the committed preset and fails on drift.

## The contract

- **Tokens, not hex.** Route every colour/size through the design tokens
  (`var(--…)`) so a site rebrand reaches your markup. `pack validate` lints.
- **CSS only for now.** There is no JS channel yet (`script:` is reserved).
- **`use:` is the whole ballgame.** The agent places your component purely by
  its one-line hint and `examples:` — write them like you mean them.
- **A pack is a module.** It runs with full Drupal power; installing one is
  exactly as much trust as installing any Drupal module. There is no sandbox.

## Ship

`docker build .` produces your deployable image: the pinned Atelier appliance
plus this pack baked in (see `Dockerfile`). Every push runs the reference
pipeline (`.github/workflows/build.yml`):

1. rebuild the CSS against the committed preset, fail on drift;
2. `docker build .`;
3. boot that image against a throwaway database (`compose.ci.yaml`) and let
   converge enable the pack — the exact path a production boot takes;
4. `drush atelier:pack-validate` (the admission gate), `drush
   atelier:kind-check`, and a front-page + gallery render smoke;
5. on `main`: re-stamp the `dev.atelier.extensions` label from the validated
   image — the registration the Atelier manager's pre-pull extension diff
   relies on — and push to `ghcr.io/<owner>/<repo>`.

Deployment is that image tag: point your appliance's `AINCIENT_IMAGE` at it.
An Atelier upgrade is a PR bumping the `FROM` pin in the `Dockerfile`; the
same pipeline then proves the pack against the new version before it ships.
