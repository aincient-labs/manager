# A console studio (EXPERIMENTAL, Atelier 0.16+). The id is forever — it is in
# every stored URL — and must be the module name or start with `__MODULE___`.
# The console import()s ui.script and calls its mount(el, ctx); the contract is
# studio/atelier-studio.d.ts. Access is a derived permission ("Use the __LABEL__ studio"),
# so a pack studio can never ship ungated.
__MODULE__:
  label: '__LABEL__'
  description: 'The __LABEL__ studio.'
  weight: 900
  ui:
    script: studio/studio.js
    style: studio/studio.css
    name: '__LABEL__'
    icon: blocks
