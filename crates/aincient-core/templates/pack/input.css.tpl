/* Tailwind entry for the pack build (optional — hand-written token-routed CSS
   in assets/__MODULE__.css works without any build).
   The two atelier/ imports are the appliance's token/utility preset, synced
   from the running dev appliance by `atelier pack dev` — they pin your
   utilities to the exact image version you develop against. */
@import "tailwindcss";
@source "../components/**/*.twig";
@import "./atelier/tw-palette.generated.css";
@import "./atelier/tokens.generated.css";
