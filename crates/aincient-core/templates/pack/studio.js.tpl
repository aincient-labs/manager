// @ts-check
// A pack studio: the console import()s this file and calls mount(el, ctx).
// Plain JS, no build step, no framework — use whatever you like inside `el`
// (Preact, Svelte, your own bundled React); React never crosses the boundary.
// The whole contract is atelier-studio.d.ts — anything not in it is internal.
// EXPERIMENTAL: needs Atelier 0.16 or later.

/** @typedef {import("./atelier-studio").StudioMountContext} StudioMountContext */
/** @typedef {import("./atelier-studio").StudioMountHandle} StudioMountHandle */

export const apiVersion = 1;

/**
 * @param {HTMLElement} el
 * @param {StudioMountContext} ctx
 * @returns {StudioMountHandle}
 */
export function mount(el, ctx) {
  const root = document.createElement("section");
  root.className = "__MODULE__-studio";
  const title = document.createElement("h2");
  title.textContent = `Hello from ${ctx.studio.name}`;
  const lede = document.createElement("p");
  lede.className = "__MODULE__-studio__lede";
  lede.textContent = "This rail is your pack's studio, mounted through the console's mount boundary.";
  const ask = document.createElement("button");
  ask.type = "button";
  ask.textContent = "Ask the agent";
  // ctx.signal aborts on unmount: hand it to every listener and fetch.
  ask.addEventListener("click", () => ctx.chat.send("What can you do in this studio?"), { signal: ctx.signal });
  root.append(title, lede, ask);
  el.append(root);
  return {
    unmount() {
      root.remove();
    },
  };
}
