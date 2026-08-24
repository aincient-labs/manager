/* __MODULE__ — PRE-COMPILED pack stylesheet. The appliance never runs your
   build: this committed file is what ships. `atelier pack dev` keeps it fresh
   via the Tailwind watcher (build/input.css); hand-written token-routed CSS
   like the rules below works exactly as well.
   THE CONTRACT: route every colour/size through the design tokens (var(--…))
   so a rebrand reaches your markup. No hardcoded hex, no opacity-muted text —
   `atelier pack validate` lints for both. */
.showcase { display: grid; gap: 1rem; padding: 4rem 1.5rem; background: var(--neutral-surface); color: var(--neutral-ink); text-align: center; }
.showcase--split { text-align: left; }
.showcase__eyebrow { color: var(--neutral-muted-foreground); font-size: var(--size-sm); }
.showcase__heading { font-family: var(--font-family-display); font-size: var(--size-3xl); }
.showcase__claim { font-size: var(--size-lg); }
.showcase__cta { background: var(--brand-primary); color: var(--brand-primary-foreground); border-radius: var(--radius-md); padding: 0.75rem 1.5rem; display: inline-block; justify-self: center; }
.showcase--split .showcase__cta { justify-self: start; }
