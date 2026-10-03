# The pack manifest — the contract with the appliance. Validated by
# `atelier pack validate` (drush atelier:pack-validate) and the admission gate.
api: 1
name: __MODULE__
requires:
  atelier: '__REQUIRES__'
# What this pack carries. components | page_kinds | providers | studios — a
# provider payload must be declared here explicitly, it never rides in silently.
provides: [__PROVIDES__]
# Config this pack owns (fenced from config:import). Patterns must stay inside
# your own namespace (__MODULE__.*) or your page kinds
# (aincient_pages.page_kind.*) — anything wider is rejected.
owns: []
