# CI-only stack — used by .github/workflows/build.yml, never in production or
# dev. Boots the image `docker build .` just produced (the pack BAKED in — the
# exact production path) against a throwaway database, so CI can run converge,
# the admission gate, kind-check and the gallery smoke before anything ships.
services:
  db:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_DB: aincient
      POSTGRES_USER: aincient
      POSTGRES_PASSWORD: aincient
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U aincient -d aincient"]
      interval: 10s
      retries: 10
  app:
    image: ${PACK_IMAGE:-__MODULE__:ci}
    depends_on:
      db:
        condition: service_healthy
    environment:
      DATABASE_URL: pgsql://aincient:aincient@db/aincient
      # Throwaway CI database — this salt protects nothing and is not a secret.
      HASH_SALT: ci-throwaway-salt
      # Opens the /atelier/dev/* endpoints so CI can smoke the gallery.
      # CI-only: never set this on a deployed appliance.
      AINCIENT_DEV: "1"
    ports:
      - "8080:80"
