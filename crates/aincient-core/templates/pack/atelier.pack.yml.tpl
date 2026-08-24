# The pack manifest — the contract with the appliance. Validated by
# `atelier pack validate` (drush atelier:pack-validate) and the admission gate.
api: 1
name: __MODULE__
requires:
  atelier: '^0.10'
# What this pack carries. components | page_kinds | providers — a provider
# payload must be declared here explicitly, it never rides in silently.
provides: [components]
# Config this pack owns (fenced from config:import). Patterns must stay inside
# your own namespace (__MODULE__.*) or your page kinds
# (aincient_pages.page_kind.*) — anything wider is rejected.
owns: []
