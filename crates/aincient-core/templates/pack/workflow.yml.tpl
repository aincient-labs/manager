# Pack CI — the Atelier client reference pipeline:
#   css    prove the committed CSS is the built CSS (against the committed preset)
#   image  build the deployable image, boot it against a throwaway database,
#          let converge enable the pack (the exact production path), run the
#          admission gate + kind-check + a gallery render smoke, then — on main
#          — re-stamp dev.atelier.extensions from the validated image and push.
# Deployment is the pushed tag; an Atelier upgrade is a PR bumping the
# Dockerfile's FROM pin, which this same pipeline then proves the pack against.
name: build
on:
  push:
    branches: [main]
  pull_request:
env:
  MODULE: __MODULE__
jobs:
  css:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 22 }
      - name: Build CSS and assert no diff vs the committed file
        run: |
          npx -y @tailwindcss/cli@4 -i build/input.css -o "assets/${MODULE}.css"
          git diff --exit-code -- assets/
  image:
    runs-on: ubuntu-latest
    needs: css
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Build the deployable image
        run: docker build -t "${MODULE}:ci" .

      - name: Boot and converge
        run: |
          set -euo pipefail
          docker compose -f compose.ci.yaml up -d
          # converge.sh installs (first boot) or updates on start; wait for its
          # own verdict in the log rather than racing the container healthcheck.
          for _ in $(seq 1 180); do
            docker compose -f compose.ci.yaml logs app 2>&1 | grep -q "converge OK" && exit 0
            sleep 5
          done
          echo "converge never reported OK" >&2
          docker compose -f compose.ci.yaml logs app | tail -100 >&2
          exit 1

      - name: Admission gate, kind-check, gallery smoke
        run: |
          set -euo pipefail
          drush() { docker compose -f compose.ci.yaml exec -T app /opt/drupal/vendor/bin/drush --root=/opt/drupal/web "$@"; }
          # The exact gate a boot runs — a REJECTED component fails the build.
          drush atelier:pack-validate "$MODULE"
          # Catalog change vs stored pages: BREAKING orphans fail the build.
          # (Meaningful against real content: seed the database from a
          # production snapshot here if you have one.)
          drush atelier:kind-check
          # The site actually serves, and every declared example renders.
          curl -fsS -o /dev/null http://localhost:8080/
          curl -fsS -o /dev/null "http://localhost:8080/atelier/packs/${MODULE}/gallery"

      # The list stamped as dev.atelier.extensions — read out of the built
      # image because only the image knows what it ships. Mirrors Atelier's own
      # release pipeline; the manager diffs this label against a site's
      # installed extensions BEFORE pulling an update. The pack itself must be
      # in the list: that is the registration that keeps that diff true.
      - name: Collect the shipped extension list
        run: |
          set -euo pipefail
          EXT="$(docker run --rm --entrypoint sh "${MODULE}:ci" -c \
            "find /opt/drupal/web -name '*.info.yml' -not -path '/opt/drupal/web/core/*' \
             | sed -e 's|.*/||' -e 's/\.info\.yml\$//' | sort -u | paste -sd, -")"
          if [ -z "$EXT" ]; then
            echo "no extensions found in the built image — refusing to stamp an empty list" >&2
            exit 1
          fi
          case ",${EXT}," in
            *",${MODULE},"*) ;;
            *) echo "${MODULE} missing from the extension list — the pack was not baked in" >&2; exit 1 ;;
          esac
          echo "ATELIER_EXTENSIONS=${EXT}" >> "$GITHUB_ENV"

      # Rebuild with the label build-arg — every layer cache-hits from the
      # build the smoke above validated (only the zero-byte metadata layer
      # differs), so the pushed bytes are the validated bytes.
      - name: Push
        if: github.event_name == 'push' && github.ref == 'refs/heads/main'
        run: |
          set -euo pipefail
          echo "${{ secrets.GITHUB_TOKEN }}" | docker login ghcr.io -u "${{ github.actor }}" --password-stdin
          IMAGE="ghcr.io/${GITHUB_REPOSITORY@L}"
          docker build -t "${IMAGE}:latest" -t "${IMAGE}:sha-${GITHUB_SHA::7}" \
            --build-arg "ATELIER_EXTENSIONS=${ATELIER_EXTENSIONS}" .
          docker push "${IMAGE}:latest"
          docker push "${IMAGE}:sha-${GITHUB_SHA::7}"
