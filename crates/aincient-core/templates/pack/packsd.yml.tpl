# The packs.d enablement drop-in: converge enables this module on every boot
# (idempotent, never fatal). In dev it is bind-mounted; the Dockerfile bakes
# the same file for production.
module: __MODULE__
