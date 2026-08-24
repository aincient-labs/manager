# Dev overlay — used by `atelier pack dev`, NEVER in production (production
# packs are baked into the image; see the Dockerfile).
# ${PACK_DIR} and ${PACK_MODULE} are injected by the CLI.
services:
  app:
    environment:
      # Opens the /atelier/dev/* pack-developer endpoints (gallery, catalog,
      # gate — what `atelier mcp` proxies). Localhost dev stacks only.
      AINCIENT_DEV: "1"
    volumes:
      # The pack source, mounted where converge links it into the docroot.
      - ${PACK_DIR}:/opt/drupal/packs/${PACK_MODULE}
      # The enablement drop-in converge reads on every boot.
      - ${PACK_DIR}/dev/pack.yml:/opt/drupal/packs.d/${PACK_MODULE}.yml:ro
      # Dev PHP: opcache revalidates mtimes, so a Twig/PHP edit is visible on
      # the next request instead of the next container.
      - ${PACK_DIR}/dev/zz-dev.ini:/usr/local/etc/php/conf.d/zz-atelier-dev.ini:ro
      # Dev Twig: debug + auto_reload + no cache (settings.appliance.php
      # includes this file when present).
      - ${PACK_DIR}/dev/services.dev.yml:/opt/drupal/web/sites/default/services.yml:ro
  css-watch:
    image: node:22-alpine
    working_dir: /pack
    volumes:
      - ${PACK_DIR}:/pack
      - atelier-pack-npm:/root/.npm
    # Installs into the pack's own gitignored node_modules (the CSS import
    # of "tailwindcss" resolves upward from build/input.css); the named npm
    # volume keeps the download warm across recreates.
    command: ["sh", "-c", "npm install --no-save --no-package-lock --no-audit --no-fund tailwindcss@4 @tailwindcss/cli@4 && exec node_modules/.bin/tailwindcss -i build/input.css -o assets/${PACK_MODULE}.css --watch=always"]
    restart: unless-stopped
volumes:
  atelier-pack-npm:
