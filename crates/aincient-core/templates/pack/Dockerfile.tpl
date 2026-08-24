# Your deployable appliance: the pinned Atelier image + this pack baked in.
# Deployment is an image tag; an Atelier upgrade is a PR bumping this pin.
ARG ATELIER_IMAGE=ghcr.io/aincient-labs/atelier-cms:latest
FROM ${ATELIER_IMAGE}
COPY . /opt/drupal/web/modules/custom/__MODULE__
COPY dev/pack.yml /opt/drupal/packs.d/__MODULE__.yml

# Re-stamp the shipped-extensions label. The base image's list does not know
# this pack, and the Atelier manager reads dev.atelier.extensions from the
# registry BEFORE pulling to refuse an update that would drop code the site
# has installed — inheriting the base (pack-less) list would make every
# update look like it drops this pack. CI reads the real list out of the
# built image and passes it here; a plain local `docker build .` stamps it
# empty, which the manager treats as "unknown", never as "ships nothing".
ARG ATELIER_EXTENSIONS=
LABEL dev.atelier.extensions="${ATELIER_EXTENSIONS}"
