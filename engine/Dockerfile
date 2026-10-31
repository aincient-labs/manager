# The Atelier engine sidecar: EXPERIMENTAL, opt-in only (DECISIONS 0483).
# Published by the release workflow from each Manager tag as
# ghcr.io/aincient-labs/atelier-engine:<version> (linux/amd64 + linux/arm64),
# and every Manager build carries that image's digest: `atelier engine enable`
# pins <repo>@sha256:<digest>, never the tag (ATELIER_ENGINE_IMAGE in .env,
# DECISIONS 0487). Developer path: engine/build.sh builds it locally as
# atelier-engine:dev, which only a developer build of the Manager pins, by
# image ID (m7-4). Context: the manager repo root. Base images are pinned by
# digest (tag kept for the reader).
FROM rust:1.88-alpine@sha256:9dfaae478ecd298b6b5a039e1f2cc4fc040fc818a2de9aa78fa714dea036574d AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p atelier

# The Docker CLI with the Compose plugin; the engine drives the host daemon
# through the mounted socket.
FROM docker:28-cli@sha256:625d9431a9f54c5a2bc90f24f0e1c3d55b1349fd857dd85035f98c2c9acbdd4d
COPY --from=build /src/target/release/atelier /usr/local/bin/atelier
EXPOSE 8080
# `serve` refuses to start unless ATELIER_ENGINE_EXPERIMENTAL=1, which only
# compose.engine.yaml sets (m7-2): `docker run` of this image alone exits.
ENTRYPOINT ["atelier", "engine", "serve"]
