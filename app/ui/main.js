// Atelier Manager — frontend. All real work lives in the Rust core (aincient-core),
// reached through Tauri commands. This file only orchestrates screens and input.
//
// Tone note: many people opening this have never run a website before. Every
// screen aims to feel calm and welcoming — one clear message, one obvious next
// step, plain language, and the technical bits tucked behind disclosures.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// The four top-level "views" (what the whole window is showing) vs. the four
// tab "panels" inside the installed app.
const SCREENS = ["loading", "docker", "unreachable", "install"];
const VIEWS = [...SCREENS, "app"];
const PANELS = ["home", "publish", "backups", "settings"];

let lastStatus = null; // most recent get_status, so panels can adapt to it
let currentTab = "home";
let exportDir = null; // folder chosen for the static export
let lastExportPath = null; // where the last export landed, for "Open the folder"
let lastExportHadBaseUrl = false; // did that export get a real website address?
let managerVersion = ""; // this app's own version, shown in the chrome
let siteRows = []; // list_sites rows, for the header switcher
let selectedSlug = null; // the site this window operates on (registry slug)
let startingRecheck = null; // timer that re-reads status while "Starting up…"
let startingSince = null; // when this "Starting up…" began (ms), for the slow note
// The last start/update in words when it didn't finish (or finished without
// the site answering) — one Home line until the next successful one. In memory.
let homeNotice = null;
// Which program runs Docker here (docker_runtime: name, can_restart,
// manual_steps, copy, others, stops) — for the Restart button's label, or the
// steps when the Manager can't restart it. Asked when a failure first offers it.
let dockerRt = null;
let dockerRtAsked = false;
// Sites on a server (M2): what each one's server last said (probe_site /
// get_status), by slug — {state: running|frozen|stopped|unreachable, …}.
const remoteStates = {};
const probing = new Set();

// Stamp the manager's version into the title bar once, before anything else can
// fail. It's compile-time constant — no Docker, no stack — so it must still show
// on the Docker-missing screen, which is exactly where a screenshot gets taken.
async function showManagerVersion() {
  try {
    managerVersion = await invoke("manager_version");
    $("manager-version").textContent = managerVersion;
    $("manager-version").title = `Atelier Manager ${managerVersion}`;
  } catch {
    // A missing version label must never block the app from starting.
  }
}

// --- view + tab routing -----------------------------------------------------

function setView(name) {
  for (const v of SCREENS) $(`screen-${v}`).classList.toggle("hidden", name !== v);
  const app = name === "app";
  $("sidebar").classList.toggle("hidden", !app);
  for (const p of PANELS) $(`panel-${p}`).classList.toggle("hidden", !(app && p === currentTab));
  if (app) showTab(currentTab);
}

function showTab(tab) {
  currentTab = tab;
  for (const p of PANELS) $(`panel-${p}`).classList.toggle("hidden", p !== tab);
  document
    .querySelectorAll(".nav-item")
    .forEach((el) => el.classList.toggle("active", el.dataset.tab === tab));
  // Lazy-load each panel's data the moment it's shown.
  if (tab === "publish") updatePublishGate();
  if (tab === "backups") refreshBackups();
  if (tab === "settings") loadSettings();
}

// `opts.dockerRestart`: the way out is restarting Docker — the banner offers it.
function showError(msg, opts = {}) {
  $("error-text").textContent = msg;
  showDockerSlot($("error-docker"), !!opts.dockerRestart);
  $("error").classList.remove("hidden");
}

function hideError() {
  $("error").classList.add("hidden");
}

// In-page confirm (the webview blocks native confirm() without the dialog plugin).
// Pass { requireText: "confirm" } to gate a destructive action behind a typed word;
// { title } heads it (naming the site and where it runs — D-07), { exact } makes
// the typed word case-sensitive (a site's slug), { lead } precedes "Type …".
function confirmModal(msg, opts = {}) {
  return new Promise((resolve) => {
    const requireText = opts.requireText || null;
    const yes = $("confirm-yes");
    // { yes } names the confirm button ("Restart").
    yes.textContent = opts.yes || "Confirm";
    const input = $("confirm-input");
    if (opts.title) $("confirm-site").classList.add("hidden");
    else showSiteLine($("confirm-site"));
    $("confirm-title").textContent = opts.title || "";
    $("confirm-title").classList.toggle("hidden", !opts.title);
    $("confirm-msg").textContent = msg || "";
    $("confirm-lead").textContent = opts.lead || "";
    // { checks: ["label", …] } adds unticked checkboxes under the message; what
    // was ticked comes back in opts.checked (one boolean each).
    const checks = $("confirm-checks");
    checks.innerHTML = "";
    for (const text of opts.checks || []) {
      const label = document.createElement("label");
      label.className = "check";
      const box = document.createElement("input");
      box.type = "checkbox";
      const span = document.createElement("span");
      span.textContent = text;
      label.append(box, span);
      checks.append(label);
    }
    checks.classList.toggle("hidden", !(opts.checks && opts.checks.length));
    if (requireText) {
      $("confirm-word").textContent = requireText;
      input.value = "";
      $("confirm-typecheck").classList.remove("hidden");
      yes.disabled = true;
      input.oninput = () => {
        yes.disabled = opts.exact
          ? input.value.trim() !== requireText
          : input.value.trim().toLowerCase() !== requireText.toLowerCase();
      };
    } else {
      $("confirm-typecheck").classList.add("hidden");
      yes.disabled = false;
    }
    $("confirm").classList.remove("hidden");
    if (requireText) setTimeout(() => input.focus(), 0);
    const done = (val) => {
      opts.checked = [...checks.querySelectorAll("input")].map((b) => val && b.checked);
      $("confirm").classList.add("hidden");
      yes.textContent = "Confirm";
      yes.onclick = null;
      $("confirm-no").onclick = null;
      input.oninput = null;
      yes.disabled = false;
      resolve(val);
    };
    yes.onclick = () => done(true);
    $("confirm-no").onclick = () => done(false);
  });
}

// THE confirmation before a site is changed (m2-8, D-07) — the window's half of
// the CLI's confirm_change. `question` names the site with "{site}" ("Restore
// {site}?" → "Restore Shop on vps.example.com?"). A live site on a server always
// asks, with the confirm button disabled until its slug is typed; anything else
// asks a plain confirm only when `always` (the actions that asked before remote
// sites existed), with an optional `requireText` word as before.
async function confirmChange(question, detail, opts = {}) {
  const row = currentSite();
  let guard = null;
  document.body.classList.add("busy");
  try {
    guard = await invoke("change_guard");
  } catch {
    /* can't ask: a site on a server then counts as live (below) */
  } finally {
    document.body.classList.remove("busy");
  }
  const host = guard ? guard.host : row && row.host;
  const label = guard ? guard.label : row ? row.label : "your site";
  const slug = guard ? guard.slug : row && row.slug;
  const place = host ? `on ${host}` : "on this computer";
  const typed = guard ? guard.typed_slug : !!host;
  if (!typed && !opts.always) return true;
  const title = question.replace("{site}", `${label} ${place}`);
  if (typed) {
    return confirmModal(detail, {
      title,
      requireText: slug,
      exact: true,
      lead: `${label} is live ${place}. `,
      yes: opts.yes,
    });
  }
  return confirmModal(detail, { title, requireText: opts.requireText || null, yes: opts.yes });
}

// Another site's backup (m2-6, D-04): name both, and make "Restore anyway" a
// deliberate second button, never the default.
function mismatchModal(m) {
  return new Promise((resolve) => {
    $("mismatch-archive").textContent = `${m.archive_site_name} (id ${m.archive_site_id})`;
    $("mismatch-target").textContent =
      `${m.target_site_name} (id ${m.target_site_id || "none yet"})`;
    $("mismatch").classList.remove("hidden");
    const done = (val) => {
      $("mismatch").classList.add("hidden");
      $("mismatch-yes").onclick = null;
      $("mismatch-no").onclick = null;
      resolve(val);
    };
    $("mismatch-yes").onclick = () => done(true);
    $("mismatch-no").onclick = () => done(false);
    setTimeout(() => $("mismatch-no").focus(), 0);
  });
}

// Restore `path` onto the selected site: whose backup it is first (another
// site's needs "Restore anyway"), then the D-07 confirm, then the restore —
// identity-checked in the backend too.
async function restoreFlow(path, what) {
  let otherSite = false;
  let note = null;
  try {
    const check = await invoke("check_restore", { path });
    if (check.mismatch) {
      if (!(await mismatchModal(check.mismatch))) return;
      otherSite = true;
    }
    note = check.note;
  } catch (e) {
    return showError(String(e));
  }
  const detail =
    `Restoring replaces the current pages, images, and settings with this ${what}.` +
    (note ? `\n\n${note}` : "");
  if (!(await confirmChange("Restore {site}?", detail, { always: true }))) return;
  const restore = (noSafetyBackup) =>
    runProgressOp("Restoring your backup", () =>
      invoke("do_restore", { path, otherSite, noSafetyBackup })
    );
  if (await restore(false)) return true;
  // The copy of the site as it is now failed (a full disk, a damaged database):
  // nothing was restored. The way past it costs that copy, so it is asked.
  if (!(lastOpOutcome && lastOpOutcome.safety_backup_failed)) return false;
  const go = await confirmChange(
    "Restore {site} without a safety copy?",
    "Atelier couldn't save a copy of your site as it is now, so nothing was restored.\n\n" +
      "Restoring without that copy replaces your current pages, images, and settings for " +
      "good: they can't be brought back afterwards.",
    { always: true, yes: "Restore without a copy" }
  );
  if (!go) return false;
  return restore(true);
}

// --- progress panel (DECISIONS 0515) ---------------------------------------
// The core announces an op's steps up front (`plan` event: id, label, and the
// slice of 0..1 each covers), then reports progress as `stage` fractions. The
// panel draws one group of blocks per step and fills it column by column. Three
// modes: "plan" (steps known), "fill" (a fraction but no plan — a plain matrix),
// "busy" (no fraction at all — one row with a slow comet and an elapsed clock).

const PM_COLS = 30; // blocks across the whole grid
const PM_ROWS = 3;
const PM_MIN_COLS = 4; // a short step still gets a group its label fits under
const LOG_KEEP = 400; // lines kept for the report

const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");

// Pacing of the block matrix: the blocks shown never jump to where the op is.
// They run through every block in order — one per PM_STEP_MS at rest, faster
// when behind, so any gap closes within PM_CATCHUP_MS — and on success a moss
// wave sweeps the grid in PM_WAVE_MS before the panel closes. Calmer than a
// group snapping on when a quick step finishes.
const PM_STEP_MS = 45;
const PM_CATCHUP_MS = 600;
const PM_WAVE_MS = 450;

const pm = {
  title: "",
  mode: "busy",
  steps: null, // [{id,label,from,to}]
  groups: [], // [{step, cols, start, cells:[el]}]
  busyCells: [],
  fraction: 0,
  pullFrac: null, // download bytes fraction, from the pull events
  failed: false,
  done: false,
  error: "",
  stageText: "",
  started: 0,
  entered: [], // ms timestamp each step was entered, by index
  current: -1,
  log: [],
  raf: 0,
};

function progressReset(title) {
  // A failure panel still waiting for Close is superseded: let its op finish
  // rather than leave it awaiting forever.
  if (pm.release) pm.release();
  pm.release = null;
  pm.lastStage = pm.lastStep = pm.lastMsg = undefined;
  pm.failedAt = undefined;
  showSiteLine($("progress-site"));
  pm.title = title;
  pm.mode = "busy";
  pm.steps = null;
  pm.groups = [];
  pm.busyCells = [];
  pm.fraction = 0;
  pm.pullFrac = null;
  pm.cells = [];
  pm.onSettled = null;
  pm.failed = false;
  pm.done = false;
  pm.error = "";
  pm.stageText = "Working…";
  pm.started = Date.now();
  pm.entered = [];
  pm.current = -1;
  pm.log = [];
  pm.tidied = null;
  $("progress-title").textContent = title;
  $("progress-stage").textContent = pm.stageText;
  $("progress-stage").classList.remove("failed");
  $("progress-log").textContent = "";
  setDetails(false);
  $("progress-copy").classList.add("hidden");
  $("progress-close").classList.add("hidden");
  showDockerSlot($("progress-docker"), false);
  showBlockers(null);
  pm.afterClose = null;
  resetPull();
  buildBusy();
}

function setDetails(open) {
  $("progress-more").classList.toggle("hidden", !open);
  $("progress-details").setAttribute("aria-expanded", String(open));
  $("progress-details").textContent = open ? "Hide details" : "Details";
}

function pmCell() {
  const c = document.createElement("div");
  c.className = "pm-cell";
  return c;
}

// The single strip for ops that report no fraction.
function buildBusy() {
  const grid = $("pm-grid");
  grid.style.gridTemplateColumns = `repeat(${PM_COLS}, 1fr)`;
  pm.busyCells = Array.from({ length: PM_COLS }, pmCell);
  grid.replaceChildren(...pm.busyCells);
  $("pm-labels").replaceChildren();
  $("pm-labels").classList.add("hidden");
}

// Columns per step, proportional to its slice, never fewer than PM_MIN_COLS.
function stepColumns(steps) {
  const span = Math.max(1e-6, steps[steps.length - 1].to - steps[0].from);
  const cols = steps.map((s) => Math.max(PM_MIN_COLS, Math.round(((s.to - s.from) / span) * PM_COLS)));
  return cols;
}

// One group per step, a 3px gutter between groups; or one plain group ("fill").
function buildGroups(steps) {
  const grid = $("pm-grid");
  const labels = $("pm-labels");
  const cols = stepColumns(steps);
  const template = cols.map((n) => `repeat(${n}, 1fr)`).join(" 3px ");
  grid.style.gridTemplateColumns = template;
  labels.style.gridTemplateColumns = template;
  const cells = [];
  const spans = [];
  let col = 1;
  pm.groups = steps.map((step, g) => {
    const group = { step, cols: cols[g], start: col, cells: [] };
    for (let k = 0; k < cols[g] * PM_ROWS; k++) {
      const c = pmCell();
      c.style.gridColumn = String(col + Math.floor(k / PM_ROWS));
      c.style.gridRow = String((k % PM_ROWS) + 1);
      group.cells.push(c);
      cells.push(c);
    }
    const label = document.createElement("span");
    label.textContent = step.label;
    label.title = step.label;
    label.style.gridColumn = `${col} / span ${cols[g]}`;
    spans.push(label);
    group.label = label;
    col += cols[g] + 1;
    return group;
  });
  grid.replaceChildren(...cells);
  labels.replaceChildren(...spans);
  labels.classList.toggle("hidden", steps.length < 2);
  pm.cells = cells; // in fill order: column by column, group by group
  pm.shown = 0;
  pm.waved = 0;
  pm.target = 0;
  pm.rate = 1 / PM_STEP_MS;
  pm.lastFrame = 0;
}

function setPlan(steps) {
  if (!steps || !steps.length) return;
  // The same plan announced twice (reinstall → install) changes nothing.
  if (pm.steps && JSON.stringify(pm.steps) === JSON.stringify(steps)) return;
  pm.steps = steps;
  pm.mode = "plan";
  pm.entered = [];
  pm.current = -1;
  // A new plan is a new run of blocks: progress from before it doesn't carry over.
  pm.fraction = 0;
  pm.pullFrac = null;
  buildGroups(steps);
}

function setFraction(f) {
  // Never step backwards: a late helper (the db-image move) may re-report a
  // lower fraction after "ready".
  pm.fraction = Math.max(pm.fraction, Math.min(1, f));
  if (pm.mode === "busy") {
    pm.mode = pm.steps ? "plan" : "fill";
    if (!pm.steps) buildGroups([{ id: "all", label: "", from: 0, to: 1 }]);
  }
  const steps = pm.steps;
  if (!steps) return;
  let i = -1;
  steps.forEach((s, k) => {
    if (pm.fraction + 1e-6 >= s.from) i = k;
  });
  if (i !== pm.current) {
    const now = Date.now();
    for (let k = pm.current + 1; k <= i; k++) pm.entered[k] = pm.entered[k] ?? now;
    pm.current = i;
  }
}

// Per-frame DOM writes only when the value changed: the comet moves every 110 ms
// and the clock once a second, but this runs ~60×/s — rewriting ARIA and text
// each frame is main-thread work and mutation noise for assistive tech.
function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}
function setAttr(el, name, value) {
  if (value === null) {
    if (el.hasAttribute(name)) el.removeAttribute(name);
  } else if (el.getAttribute(name) !== value) el.setAttribute(name, value);
}

// Which state each block is in; called every frame while the panel is open.
function renderMatrix(now) {
  if (pm.mode === "busy") {
    const n = pm.busyCells.length;
    if (pm.done) {
      pm.busyCells.forEach((c) => (c.className = "pm-cell done"));
      if (pm.onSettled) {
        pm.onSettled();
        pm.onSettled = null;
      }
    } else if (pm.failed) {
      pm.busyCells.forEach((c, k) => (c.className = k === 0 ? "pm-cell err" : "pm-cell"));
    } else {
      const span = n - 1;
      const cyc = (now / 110) % (2 * span);
      const dir = cyc < span ? 1 : -1;
      const head = Math.round(cyc < span ? cyc : 2 * span - cyc);
      pm.busyCells.forEach((c, k) => {
        const d = (head - k) * dir;
        const cls = reduceMotion.matches ? "t2" : d === 0 ? "on" : d === 1 ? "t1" : d === 2 || d === 3 ? "t2" : "";
        const want = cls ? `pm-cell ${cls}` : "pm-cell";
        if (c.className !== want) c.className = want;
      });
    }
    setAttr($("progressbar"), "aria-valuenow", null);
    if (!pm.done && !pm.failed) {
      const secs = Math.floor((Date.now() - pm.started) / 1000);
      const clock = `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, "0")}`;
      setText($("progress-stage"), `${pm.stageText} · ${clock}`);
    }
    return;
  }
  const f = pm.fraction;
  // How many blocks the op has really reached, in fill order: past groups full,
  // the current one by its own fraction (or pull bytes), later ones empty.
  let target = 0;
  pm.groups.forEach((g, gi) => {
    const s = g.step;
    const n = g.cells.length;
    let local = Math.max(0, Math.min(1, (f - s.from) / Math.max(1e-6, s.to - s.from)));
    const isCurrent = pm.mode === "fill" || gi === pm.current;
    if (s.id === "pull" && isCurrent && pm.pullFrac != null) local = Math.max(local, pm.pullFrac);
    if (pm.done || (pm.mode === "plan" && gi < pm.current)) target += n;
    else if (isCurrent) target += Math.min(n, Math.floor(local * n));
  });
  const total = pm.cells.length;
  // Advance what's shown toward it — every block in turn, never a jump.
  const dt = pm.lastFrame ? Math.min(100, now - pm.lastFrame) : 0;
  pm.lastFrame = now;
  if (reduceMotion.matches || pm.snap) {
    pm.shown = target;
    if (pm.done) pm.waved = total;
  } else {
    // The speed is set when the target moves, then held: closing a gap at a
    // speed proportional to what's left would only ever approach it.
    if (target > pm.target) {
      pm.target = target;
      pm.rate = Math.max(1 / PM_STEP_MS, (target - pm.shown) / PM_CATCHUP_MS);
    }
    if (pm.shown < target) pm.shown = Math.min(target, pm.shown + dt * pm.rate);
    if (pm.done && pm.shown >= total) pm.waved = Math.min(total, pm.waved + (dt * total) / PM_WAVE_MS);
  }
  const lit = Math.floor(pm.shown);
  const waved = Math.floor(pm.waved);
  pm.cells.forEach((c, k) => {
    let cls = "";
    if (k < waved) cls = "done";
    else if (k < lit) cls = "on";
    else if (k === lit && !pm.done) cls = pm.failed ? "err" : "head";
    const want = cls ? `pm-cell ${cls}` : "pm-cell";
    if (c.className !== want) c.className = want;
  });
  if (pm.done && waved >= total && pm.onSettled) {
    pm.onSettled();
    pm.onSettled = null;
  }
  // Labels follow the blocks shown, so a heading never runs ahead of its group.
  let start = 0;
  pm.groups.forEach((g, gi) => {
    const end = start + g.cells.length;
    let state = "";
    if (pm.failed && gi === pm.current) state = "failed";
    else if (lit >= end) state = "past";
    else if (lit >= start) state = pm.done ? "past" : "now";
    if (g.label && g.label.className !== state) g.label.className = state;
    start = end;
  });
  const pct = Math.round((pm.done ? 1 : f) * 100);
  const bar = $("progressbar");
  setAttr(bar, "aria-valuenow", String(pct));
  const step = pm.steps && pm.steps[pm.current];
  setAttr(bar, "aria-valuetext", step ? `${step.label}, ${pct}%` : `${pct}%`);
}

function startMatrixLoop() {
  cancelAnimationFrame(pm.raf);
  const tick = (now) => {
    renderMatrix(now);
    pm.raf = requestAnimationFrame(tick);
  };
  pm.raf = requestAnimationFrame(tick);
}

// Settle to a full moss "done".
// Resolves once the blocks have run through to the end and the moss wave has
// swept the grid (at once with reduced motion, or for the busy strip).
function progressFinish() {
  pm.done = true;
  pm.fraction = 1;
  $("progress-stage").textContent = pm.stageText && pm.mode !== "busy" ? pm.stageText : "Done.";
  const settled = new Promise((resolve) => {
    pm.onSettled = resolve;
  });
  renderMatrix(performance.now());
  // Never hold the panel longer than a full catch-up plus the wave.
  return Promise.race([settled, new Promise((r) => setTimeout(r, PM_CATCHUP_MS + PM_WAVE_MS + 400))]);
}

function appendLog(line, cls) {
  pm.log.push(line);
  if (pm.log.length > LOG_KEEP) pm.log.shift();
  const log = $("progress-log");
  const row = document.createElement("div");
  row.textContent = line;
  if (cls) row.className = cls;
  log.appendChild(row);
  log.scrollTop = log.scrollHeight;
}

// Colour a docker/converge line by what it reports, so a problem reads at a
// glance: coral for errors, ochre for warnings, dim for routine chatter.
function logClass(line) {
  if (/\b(error|fatal|failed|failure|panic|denied|unauthorized)\b/i.test(line)) return "log-error";
  if (/\b(warn|warning|deprecated|retrying)\b/i.test(line)) return "log-warn";
  return "";
}
// --- structured pull progress -------------------------------------------
// One row per image layer, driven by compose's `--progress json` events (id ·
// bar · phase/bytes). `dlCur`/`dlTot` remember download bytes separately from
// the live phase, so the summary's "X of Y downloaded" stays monotone while a
// layer moves on to extraction.
let pullLayers = new Map();
let pullImage = null;
let pullRenderQueued = false;

function fmtBytes(n) {
  if (n == null) return "";
  const units = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n >= 10 || i === 0 ? Math.round(n) : n.toFixed(1)} ${units[i]}`;
}

function resetPull() {
  pullLayers = new Map();
  pullImage = null;
  $("pull-layers").innerHTML = "";
  $("pull-layers").classList.add("hidden");
  $("pull-summary").textContent = "";
  $("pull-summary").classList.add("hidden");
}

function pullUpdate(ev) {
  if (!ev) return;
  // Image-level envelope ("Image ghcr.io/… Pulling/Pulled"). A new image means
  // a new pull — stepped upgrades pull several — so start the panel fresh.
  if (!ev.parent_id) {
    if (ev.id !== pullImage) {
      resetPull();
      pullImage = ev.id;
    }
    const ref = ev.id.replace(/^Image /, "");
    if (ev.status === "Done") appendLog(`✓ pulled ${ref}`);
    if (ev.status === "Error") appendLog(`✗ ${ref} — ${ev.text || "pull failed"}`, "log-error");
    return;
  }
  const rec = pullLayers.get(ev.id) || {};
  rec.text = ev.text || rec.text;
  rec.status = ev.status || rec.status;
  rec.percent = ev.percent ?? rec.percent;
  if (ev.text === "Downloading") {
    rec.dlCur = ev.current;
    rec.dlTot = ev.total;
  } else if (ev.status === "Done" || ev.text === "Download complete") {
    if (rec.dlTot != null) rec.dlCur = rec.dlTot;
  }
  pullLayers.set(ev.id, rec);
  if (!pullRenderQueued) {
    pullRenderQueued = true;
    requestAnimationFrame(() => {
      pullRenderQueued = false;
      renderPull();
    });
  }
}

function renderPull() {
  const panel = $("pull-layers");
  const summary = $("pull-summary");
  panel.classList.remove("hidden");
  summary.classList.remove("hidden");

  let done = 0, dlCur = 0, dlTot = 0;
  const frag = document.createDocumentFragment();
  for (const [id, rec] of pullLayers) {
    if (rec.status === "Done") done++;
    dlCur += rec.dlCur || 0;
    dlTot += rec.dlTot || 0;

    const row = document.createElement("div");
    row.className = "pull-row";
    if (rec.status === "Done") row.classList.add("pull-done");
    else if (rec.status === "Warning") row.classList.add("pull-warn");
    else if (rec.status === "Error") row.classList.add("pull-fail");

    const idEl = document.createElement("span");
    idEl.className = "pull-id";
    idEl.textContent = id;

    const bar = document.createElement("div");
    bar.className = "pull-bar";
    const fill = document.createElement("div");
    fill.className = "pull-bar-fill";
    const pct = rec.status === "Done" ? 100 : rec.percent ?? 0;
    fill.style.width = `${Math.max(0, Math.min(100, pct))}%`;
    bar.appendChild(fill);

    const meta = document.createElement("span");
    meta.className = "pull-meta";
    meta.title = rec.text || "";
    if (rec.status === "Error") meta.textContent = "failed";
    else if (rec.text === "Already exists") meta.textContent = "cached";
    else if (rec.status === "Done") meta.textContent = "done";
    else if (rec.text === "Downloading" && rec.dlTot != null)
      meta.textContent = `${fmtBytes(rec.dlCur)} / ${fmtBytes(rec.dlTot)}`;
    else if (rec.text === "Extracting")
      meta.textContent = `unpack ${Math.round(rec.percent ?? 0)}%`;
    else meta.textContent = (rec.text || "…").toLowerCase();

    row.append(idEl, bar, meta);
    frag.appendChild(row);
  }
  panel.replaceChildren(frag);
  const bytes = dlTot > 0 ? ` · ${fmtBytes(dlCur)} of ${fmtBytes(dlTot)}` : "";
  summary.textContent = `${done} of ${pullLayers.size} layers done${bytes}`;
  // The Download group fills from bytes; the stage line says how far.
  pm.pullFrac = dlTot > 0 ? dlCur / dlTot : null;
  if (dlTot > 0 && !pm.failed) {
    $("progress-stage").textContent = `Downloading · ${fmtBytes(dlCur)} of ${fmtBytes(dlTot)}`;
  }
}

function progressUpdate(p) {
  if (p.kind === "plan") {
    setPlan(p.steps);
    return;
  }
  if (typeof p.fraction === "number") setFraction(p.fraction);
  if (p.kind === "pull") {
    pullUpdate(p.pull);
    return;
  }
  if (p.kind === "log") {
    if (p.message.trim()) appendLog(p.message, logClass(p.message));
    // The after-update clean-up's one line ("Removed N old download(s), …"):
    // the finished panel says it, not only the Details log.
    if (/^Removed \d+ old download/.test(p.message)) pm.tidied = p.message.trim();
    return;
  }
  pm.stageText = p.message;
  if (pm.mode !== "busy") $("progress-stage").textContent = p.message;
  // One feed line per step (and per distinct message of a no-percentage op),
  // not one per boot-poll tick.
  if (p.stage !== pm.lastStage || pm.current !== pm.lastStep || (p.stage === "working" && p.message !== pm.lastMsg)) {
    appendLog(`▸ ${p.message}`);
  }
  pm.lastStage = p.stage;
  pm.lastStep = pm.current;
  pm.lastMsg = p.message;
}

// Keep the panel open on the step that broke: brick block, the error in the
// stage line, the log open, and a report to copy. Resolves on Close / Escape.
// `dockerRestart`: the way out is restarting Docker — the panel offers it.
function progressFail(message, dockerRestart = false) {
  showDockerSlot($("progress-docker"), dockerRestart);
  pm.failed = true;
  pm.error = message;
  pm.failedAt = Date.now();
  $("progress-stage").textContent = message;
  $("progress-stage").classList.add("failed");
  appendLog(`✗ ${message}`, "log-error");
  setDetails(true);
  pm.snap = true; // show where it stopped, not where the run-through had got to
  renderMatrix(performance.now());
  pm.snap = false;
  cancelAnimationFrame(pm.raf); // a failed panel holds still
  $("progress-copy").classList.remove("hidden");
  $("progress-close").classList.remove("hidden");
  $("progress-close").focus();
  return new Promise((resolve) => {
    const onKey = (e) => {
      if (e.key === "Escape") done();
    };
    const done = () => {
      document.removeEventListener("keydown", onKey);
      $("progress-close").onclick = null;
      pm.release = null;
      resolve();
    };
    pm.release = done;
    $("progress-close").onclick = done;
    document.addEventListener("keydown", onKey);
  });
}

// An update refused because the site isn't ready (gate.rs): each reason in its
// own words, with the button for its way forward when the app has one. The
// actions are the window's own; a Restart Docker reason gets the Docker slot
// (the button, or the steps for a runtime the Manager can't restart).
const BLOCKER_BUTTONS = {
  "free-space-open": "Free up space",
  "backups-open": "Open Backups",
  "startstop-from-publish": "Start your site",
  "checkup-now": "Run a checkup",
  "engine-unlock": "Unlock",
};

function showBlockers(blockers) {
  const list = $("progress-blockers");
  list.textContent = "";
  list.classList.toggle("hidden", !(blockers && blockers.length));
  if (!blockers) return;
  for (const b of blockers) {
    const li = document.createElement("li");
    const text = document.createElement("span");
    text.textContent = b.text;
    li.append(text);
    if (b.action === "restart-docker") {
      const slot = document.createElement("span");
      slot.className = "docker-slot";
      slot.dataset.dockerSlot = "button";
      li.append(slot);
      showDockerSlot(slot, true);
    } else if (BLOCKER_BUTTONS[b.action]) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "btn small";
      btn.dataset.action = "blocker";
      btn.dataset.next = b.action;
      btn.textContent = BLOCKER_BUTTONS[b.action];
      li.append(btn);
    }
    list.append(li);
  }
}

function fmtSecs(ms) {
  return `${(ms / 1000).toFixed(1)}s`;
}

// Plain text for an issue or a support thread: what ran, on what, which step
// broke after how long, and the tail of the log.
function progressReport() {
  const lines = [`Atelier Manager ${managerVersion || "(unknown version)"}: ${pm.title}`];
  const row = currentSite();
  if (row) lines.push(`Site: ${row.label} · ${siteHost(row)}`);
  if (lastStatus) {
    const digest = lastStatus.image_digest ? ` @ ${lastStatus.image_digest}` : "";
    lines.push(`Atelier: ${lastStatus.version || "unknown"} · ${lastStatus.image}${digest}`);
  }
  lines.push(`Started: ${new Date(pm.started).toISOString()} · ${navigator.platform}`);
  if (pm.steps) {
    lines.push("", "Steps:");
    pm.steps.forEach((s, i) => {
      const state = i < pm.current ? "done" : i === pm.current ? (pm.failed ? "FAILED" : "running") : "not run";
      const t0 = pm.entered[i];
      const t1 = i < pm.current ? pm.entered[i + 1] : i === pm.current ? pm.failedAt ?? Date.now() : null;
      const took = t0 != null && t1 != null ? fmtSecs(t1 - t0) : "";
      lines.push(`  ${state.padEnd(8)} ${s.label.padEnd(12)} ${took}`.trimEnd());
    });
  } else {
    lines.push(`Ran for ${fmtSecs((pm.failedAt ?? Date.now()) - pm.started)}`);
  }
  if (pm.error) lines.push("", `Error: ${pm.error}`);
  const tail = pm.log.slice(-60);
  lines.push("", `Log (last ${tail.length} of ${pm.log.length} lines):`, ...tail);
  return lines.join("\n");
}

$("progress-details").addEventListener("click", () =>
  setDetails($("progress-more").classList.contains("hidden"))
);
$("progress-copy").addEventListener("click", () => copyText(progressReport(), $("progress-copy")));

// Wrap any long op in the progress panel: stream its steps via op-progress
// events, then refresh. Returns whether the op completed without error.
// Which runProgressOp owns the panel. A failure panel superseded by a new op is
// released (progressReset), and its runProgressOp then resumes while the new one
// is running: it must not hide the panel or stop the new op's render loop.
let progressOwner = 0;
// The last op's answer object ({state, reason, …}), so a flow can offer what
// its failure allows (safety_backup_failed, backup_failed); null when the op
// answered with anything else.
let lastOpOutcome = null;

// `opts.lifecycle` names a start-or-update kind of op ("start" | "update" |
// "setup"; "repair" and "clear" only clear): its failure leaves a line on Home, its
// success clears it, and a result of `false` (the site didn't answer before
// the wait ran out) is said as "not yet", not success.
const NOT_READY_TEXT =
  "Atelier started, but your site hasn't answered yet. It may still be finishing — " +
  "if it doesn't come up in a few minutes, run a checkup.";
const DIDNT_FINISH = {
  start: "Your website didn't finish starting last time.",
  update: "The last update didn't finish.",
  setup: "The last setup didn't finish.",
};

// While the progress panel is up the page behind it is inert: Enter on a
// focused button there must not start a second operation (the panel is a modal
// in markup only; focus could still sit on a control underneath).
function setProgressOpen(open) {
  for (const el of document.querySelectorAll(".topbar, .shell")) el.inert = open;
  $("progress").classList.toggle("hidden", !open);
  if (open) {
    const panel = document.querySelector("#progress .progress-panel");
    panel.tabIndex = -1;
    panel.focus({ preventScroll: true });
  }
}

const ALREADY_CURRENT_TEXT = "Already up to date. Nothing was downloaded or restarted.";
// The same build, but its settings (.env, compose.yaml) had changed: the
// containers were made again on it, nothing was backed up.
const SETTINGS_APPLIED_TEXT = "Already up to date. Applied your changed settings.";

async function runProgressOp(title, fn, opts = {}) {
  const me = ++progressOwner;
  // A new attempt supersedes the previous failure — leaving the old banner up
  // makes a retry that succeeds look like it failed.
  hideError();
  progressReset(title);
  setProgressOpen(true);
  startMatrixLoop();
  const unlisten = await listen("op-progress", (e) => progressUpdate(e.payload));
  let failure = null;
  let dockerRestart = false;
  let result;
  lastOpOutcome = null;
  try {
    result = await fn();
  } catch (e) {
    failure = String(e);
  }
  if (result && typeof result === "object" && typeof result.state === "string") {
    lastOpOutcome = result;
  }
  // A start, setup or update says how it came out: {state: ready|booting|failed,
  // reason}. Failed gets the failure panel (Close / Copy / Details) with its
  // reason; booting is the "not answering yet" notice, already past the point
  // where "a few seconds" would be true; current is an update that found the
  // site already on the newest build (nothing downloaded, backed up or restarted),
  // or on it with changed settings it then applied (state "settings").
  let current = null;
  if (result && typeof result === "object" && typeof result.state === "string") {
    if (result.state === "current") current = ALREADY_CURRENT_TEXT;
    else if (result.state === "settings") current = SETTINGS_APPLIED_TEXT;
    if (result.state === "failed") {
      failure = result.reason || "Your website couldn't finish starting.";
      // The core marked it (a typed error, not this text): restarting Docker
      // is the way out, so every place showing it offers the button.
      dockerRestart = !!result.docker_restart;
      if (result.blockers) {
        showBlockers(result.blockers);
        // The banner left after Close keeps every reason; the panel lists them.
        failure = [failure, ...result.blockers.map((b) => b.text)].join(" ");
      }
    } else if (result.state === "booting") {
      result = false;
      startingSince = Date.now() - STARTING_SLOW_MS;
    } else {
      result = true;
    }
  }
  try {
    if (failure !== null) {
      // A busy site (another operation holds it, oplock.rs) attempted
      // nothing: no "didn't finish" line, the panel says to wait.
      if (DIDNT_FINISH[opts.lifecycle] && !(lastOpOutcome && lastOpOutcome.site_busy)) {
        setHomeNotice(DIDNT_FINISH[opts.lifecycle], { dockerRestart });
      }
      await progressFail(failure, dockerRestart);
      // Closing the panel must not erase the failure: the banner stays until the
      // next action (unless a newer op already took over the screen).
      if (progressOwner === me) showError(failure, { dockerRestart });
      // The failed op may have left the site somewhere new (stopped, half
      // updated): show where it is now, not where it was.
      try {
        await refresh();
      } catch {
        /* the failure above is the message that matters */
      }
      return false;
    }
    if (opts.lifecycle) setHomeNotice(result === false ? NOT_READY_TEXT : null);
    const settled = progressFinish();
    if (opts.lifecycle && result === false) $("progress-stage").textContent = NOT_READY_TEXT;
    else if (current) $("progress-stage").textContent = current;
    else if (pm.tidied) $("progress-stage").textContent = `Done. ${pm.tidied}`;
    try {
      await refresh();
    } catch (e) {
      showError(String(e));
    }
    await settled; // let the blocks finish their run before the panel closes
    // Long enough to read the clean-up line before the panel goes.
    if ((current || pm.tidied) && result !== false) await new Promise((r) => setTimeout(r, 2500));
    // An update, setup or channel switch changes what "up to date" means: ask
    // again rather than leave the line on a stale or empty answer.
    if (opts.lifecycle === "update" || opts.lifecycle === "setup") backgroundUpdateRefresh(false);
    return true;
  } finally {
    unlisten();
    if (progressOwner === me) {
      cancelAnimationFrame(pm.raf);
      setProgressOpen(false);
      // The failure panel's Restart button closed it: run that once it's gone.
      const next = pm.afterClose;
      pm.afterClose = null;
      if (next) setTimeout(next, 0);
    }
  }
}

// --- restart Docker ---------------------------------------------------------
// Several failures end with "Restart Docker" (a half-attached network, a start
// still stuck after clearing Docker's leftovers, Docker not answering). The core
// marks them (docker_restart on the answer); each place that shows one carries a
// slot ([data-docker-slot]) for the button, or — for a runtime the Manager can't
// restart (Docker Engine on Linux) — the steps and a Copy button.

function dockerSlotHtml(kind) {
  const name = dockerRt ? dockerRt.name : "Docker";
  if (dockerRt && !dockerRt.can_restart) {
    let html = `<span class="docker-manual">${esc(dockerRt.manual_steps || "")}</span>`;
    if (dockerRt.copy) {
      html +=
        `<button type="button" class="btn ghost small" data-action="copy-docker-cmd">` +
        `<svg class="ic" aria-hidden="true"><use href="#i-copy" /></svg><span>Copy the command</span></button>`;
    }
    return html;
  }
  if (kind === "text") {
    return `<button type="button" class="text-btn" data-action="restart-docker">Restart ${esc(name)}</button>`;
  }
  return (
    `<button type="button" class="btn small" data-action="restart-docker">` +
    `<svg class="ic" aria-hidden="true"><use href="#i-refresh" /></svg><span>Restart ${esc(name)}</span></button>`
  );
}

// Fill every slot with the button (or the steps) for what we know so far.
function fillDockerSlots() {
  for (const el of document.querySelectorAll("[data-docker-slot]")) {
    el.innerHTML = dockerSlotHtml(el.dataset.dockerSlot);
  }
}

// Show (or hide) one slot; the first one shown asks which runtime this is, so
// the button can name it ("Restart OrbStack").
function showDockerSlot(el, on) {
  el.classList.toggle("hidden", !on);
  if (!on) return;
  el.innerHTML = dockerSlotHtml(el.dataset.dockerSlot);
  if (dockerRtAsked) return;
  dockerRtAsked = true;
  invoke("docker_runtime")
    .then((info) => {
      dockerRt = info;
      fillDockerSlots();
    })
    .catch(() => {
      dockerRtAsked = false; // ask again next time; the plain label stands
    });
}

// The button: ask afresh (what else is running changes), confirm naming it,
// then restart and start the site again in the progress panel.
async function restartDockerFlow() {
  let info;
  document.body.classList.add("busy");
  try {
    info = await invoke("docker_runtime");
  } catch (e) {
    return showError(String(e));
  } finally {
    document.body.classList.remove("busy");
  }
  dockerRt = info;
  dockerRtAsked = true;
  fillDockerSlots();
  if (!info.can_restart) return; // the slots now show the steps instead
  const ok = await confirmModal(info.stops, {
    title: `Restart ${info.name}?`,
    yes: "Restart",
  });
  if (!ok) return;
  await runProgressOp(`Restarting ${info.name}`, () => invoke("do_restart_docker"), {
    lifecycle: "start",
  });
}

// --- status -----------------------------------------------------------------

async function refresh() {
  await refreshSites();
  const row = currentSite();
  const remote = !!(row && row.host);
  const problem = await invoke("preflight_problem");
  if (problem) {
    $("docker-msg").textContent = problem;
    setView("docker");
    return;
  }

  let status;
  try {
    status = await invoke("get_status");
  } catch (e) {
    if (!remote) throw e;
    // A server that didn't answer: say which, and offer to try again.
    remoteStates[row.slug] = { state: "unreachable", reason: String(e) };
    renderSites();
    const removed = String(e).includes(REMOVED_PHRASE);
    $("unreachable-title").textContent = removed
      ? `This Manager was removed from ${row.label}`
      : `Can't reach ${row.host}`;
    $("unreachable-lede").textContent = row.engine
      ? "This site is reached through its engine over HTTPS, without SSH. Nothing was changed."
      : UNREACHABLE_SSH_LEDE;
    $("unreachable-msg").textContent = String(e);
    $("unreachable-forget").classList.toggle("hidden", !row.engine);
    setView("unreachable");
    return;
  }
  lastStatus = status;
  if (remote) {
    const before = remoteStates[row.slug];
    if (!(before && before.state === "frozen" && status.running)) {
      // The engine's word for a site that isn't up, where it has one.
      const word = engineWord(status);
      const held = ["resting", "waking", "locked"].includes(word) ? word : "stopped";
      remoteStates[row.slug] = {
        state: status.running ? "running" : held,
        version: status.version,
      };
      renderSites();
    }
  }
  // Nothing is ever set up on a server from here: a remote site is attached,
  // so it always gets the app view.
  if (!status.installed && !remote) {
    renderInstall();
    setView("install");
    return;
  }

  renderStatus(status);
  setView("app");
  // Best-effort, non-blocking enrichment. Reads the cached check only — refresh()
  // runs after every lifecycle op, and must never start a registry check
  // (manager#6); startUpdateRefresh() owns those.
  refreshUpdate();
}

// A refused token on an engine-paired site (M4): the Rust side says this.
const REMOVED_PHRASE = "this Manager was removed from";
const UNREACHABLE_SSH_LEDE =
  "This site runs on a server, and the Manager reaches it over SSH. The server didn't " +
  "answer — it may be offline, or the connection may need a moment. Nothing was changed.";

// --- sites (header switcher) -----------------------------------------------
// One Manager, several sites (DECISIONS 0475 · M1). The backend holds which site
// this window operates on; every command resolves its stack from that, and the
// header always names it — so no action can land on a site you aren't looking at.

function currentSite() {
  return siteRows.find((r) => r.slug === selectedSlug) || null;
}

// Where a site runs, in words: this computer, or the server's ssh target.
function siteHost(row) {
  return row && row.host ? row.host : "This computer";
}

// "on this computer" / "on deploy@vps…" — for sentences.
function sitePlace(row) {
  return row && row.host ? `on ${row.host}` : "on this computer";
}

const REMOTE_STATE_WORDS = {
  running: "Running",
  frozen: "Frozen",
  stopped: "Stopped",
  resting: "Resting",
  waking: "Waking",
  locked: "Locked down",
};

function siteState(row) {
  if (row.host) {
    const st = remoteStates[row.slug];
    if (!st) return "Checking…";
    if (st.state === "unreachable") return "Can't reach it";
    return REMOTE_STATE_WORDS[st.state] || "—";
  }
  if (!row.installed) return "Not set up yet";
  if (row.running) return "Running";
  return REMOTE_STATE_WORDS[localHeld(row)] || "Stopped";
}

// A site on this computer that isn't running: the engine's word for it
// (resting, waking, locked) when it has one, else plain "stopped".
function localHeld(row) {
  const word = row.engine_state;
  return ["resting", "waking", "locked"].includes(word) ? word : "stopped";
}

// The switcher line under a site's name. A server that didn't answer is
// named outright: "Can't reach deploy@vps.example.com".
function siteMeta(row) {
  const st = row.host && remoteStates[row.slug];
  if (st && st.state === "unreachable") return `Can't reach ${row.host}`;
  return `${siteHost(row)} · ${siteState(row)}`;
}

function siteDot(row) {
  if (!row.host) {
    if (row.running) return "up";
    return { resting: "warn", waking: "warn", locked: "down" }[localHeld(row)] || "down";
  }
  const st = remoteStates[row.slug];
  if (!st) return "";
  return (
    { running: "up", frozen: "warn", resting: "warn", waking: "warn", locked: "down", stopped: "down", unreachable: "warn" }[
      st.state
    ] || ""
  );
}

// Ask each server for its site's state, after the list painted — one bounded
// probe per site, never more than one in flight, so a slow server never holds
// the switcher (or anything else) up.
function probeRemoteSites() {
  for (const r of siteRows) {
    if (!r.host || probing.has(r.slug)) continue;
    probing.add(r.slug);
    invoke("probe_site", { slug: r.slug })
      .then((st) => (remoteStates[r.slug] = st))
      .catch((e) => (remoteStates[r.slug] = { state: "unreachable", reason: String(e) }))
      .finally(() => {
        probing.delete(r.slug);
        renderSites();
      });
  }
}

// "Blog · This computer" above a confirm or a running op. Only once there is more
// than one site — with one, the line would just repeat the obvious.
function showSiteLine(el) {
  const row = currentSite();
  const show = !!row && siteRows.length > 1;
  el.classList.toggle("hidden", !show);
  el.textContent = show ? `${row.label} · ${siteHost(row)}` : "";
}

async function refreshSites() {
  try {
    const view = await invoke("list_sites");
    siteRows = view.sites || [];
    const before = selectedSlug;
    selectedSlug = view.selected || null;
    // The first answer (or an outside change) names the site: its own publish
    // preferences replace whatever the form showed.
    if (selectedSlug !== before) loadPublishPrefs();
  } catch {
    // The switcher is an addition, never a gate: a broken registry still leaves
    // the selected site's screens working (they report the error themselves).
    siteRows = [];
  }
  const row = currentSite();
  document.body.classList.toggle("remote", !!(row && row.host));
  renderSites();
  probeRemoteSites();
}

function renderSites() {
  const row = currentSite();
  $("site-switcher").classList.toggle("hidden", !row);
  if (!row) return;
  $("site-current-label").textContent = row.label;
  $("site-current-host").textContent = siteMeta(row);
  $("site-current-dot").className = `dot ${siteDot(row)}`;
  $("site-current").title = row.host
    ? `${row.label} (${row.slug}) — ${row.remote_site || row.slug} on ${row.host}`
    : `${row.label} (${row.slug}) — ${row.home}`;

  const list = $("site-list");
  const frag = document.createDocumentFragment();
  for (const r of siteRows) {
    const li = document.createElement("li");
    const btn = document.createElement("button");
    btn.className = "site-option";
    btn.setAttribute("role", "menuitemradio");
    btn.setAttribute("aria-checked", String(r.slug === selectedSlug));
    btn.dataset.action = "select-site";
    btn.dataset.slug = r.slug;
    btn.title = r.host ? `${r.remote_site || r.slug} on ${r.host}` : r.home;
    btn.innerHTML =
      `<span class="dot ${siteDot(r)}"></span>` +
      `<span class="site-option-text"><span class="site-option-label">${esc(r.label)}</span>` +
      `<span class="site-option-meta">${esc(siteMeta(r))}</span></span>` +
      `<svg class="ic tick" aria-hidden="true"><use href="#i-tick" /></svg>`;
    li.appendChild(btn);
    frag.appendChild(li);
  }
  list.replaceChildren(frag);
}

function setSiteMenu(open) {
  $("site-menu").classList.toggle("hidden", !open);
  $("site-current").setAttribute("aria-expanded", String(open));
  if (open) {
    const checked = $("site-list").querySelector('[aria-checked="true"]');
    (checked || $("site-list").querySelector("button"))?.focus();
  }
}

// Forget everything the window learned about the previous site, so nothing from
// it (an export path, a checkup, a log, an update answer) shows under the new one.
function resetSiteUi() {
  lastStatus = null;
  lastExportPath = null;
  $("publish-result").classList.add("hidden");
  $("update-status").classList.add("hidden");
  $("update-banner").classList.add("hidden");
  $("update-line").classList.add("hidden");
  lastCachedUpdate = null;
  updateCheckProblem = null;
  updateAttempted = false;
  updateChecks = 0;
  siteGen++;
  startingSince = null;
  setHomeNotice(null);
  $("doctor-summary").classList.add("hidden");
  $("doctor-results").classList.add("hidden");
  $("doctor-fix").classList.add("hidden");
  $("logs-view").textContent = "Tap Refresh to load the latest activity.";
  $("down-wipe").checked = false;
  loadPublishPrefs();
}

async function switchSite(slug) {
  setSiteMenu(false);
  if (slug === selectedSlug) return;
  hideError();
  try {
    await invoke("select_site", { slug });
  } catch (e) {
    return showError(String(e));
  }
  selectedSlug = slug;
  resetSiteUi();
  currentTab = "home";
  setView("loading");
  await refresh().catch((e) => showError(String(e)));
  backgroundUpdateRefresh(false);
}

// A short name from a display name: "My Blog!" → "my-blog".
function slugify(label) {
  return label
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/g, "");
}

const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,39}$/;
let slugEdited = false; // stop deriving the short name once someone types one

let addMode = "local"; // "local" | "server"

function setAddMode(mode) {
  addMode = mode;
  document.querySelectorAll("#add-site .seg-opt").forEach((el) =>
    el.setAttribute("aria-checked", String(el.dataset.mode === mode))
  );
  document.querySelectorAll("#add-site .add-local").forEach((el) =>
    el.classList.toggle("hidden", mode !== "local")
  );
  document.querySelectorAll("#add-site .add-server").forEach((el) =>
    el.classList.toggle("hidden", mode !== "server")
  );
  $("add-how").classList.toggle("hidden", !(mode === "server" && experimental));
  if (!experimental) document.querySelector('input[name="add-server-how"][value="ssh"]').checked = true;
  setHowMode();
}

// Over SSH, with an install code, or asking to join (M4; experimental only).
function addHowMode() {
  const checked = document.querySelector('input[name="add-server-how"]:checked');
  return addMode === "server" && experimental && checked ? checked.value : "ssh";
}

function setHowMode() {
  const how = addHowMode();
  const server = addMode === "server";
  document.querySelectorAll("#add-site .how-ssh").forEach((el) =>
    el.classList.toggle("hidden", how !== "ssh")
  );
  document.querySelectorAll("#add-site .how-engine").forEach((el) =>
    el.classList.toggle("hidden", how === "ssh")
  );
  document.querySelectorAll("#add-site .how-code").forEach((el) =>
    el.classList.toggle("hidden", how !== "code")
  );
  document.querySelectorAll("#add-site .how-join").forEach((el) =>
    el.classList.toggle("hidden", how !== "join")
  );
  $("add-site-submit").textContent = !server
    ? "Add the site"
    : how === "code"
      ? "Pair with the site"
      : how === "join"
        ? "Ask to join"
        : "Attach the site";
  $("add-site-error").classList.add("hidden");
  if (how !== "ssh" && !$("add-site-name").value) {
    invoke("manager_name")
      .then((n) => {
        if (!$("add-site-name").value) $("add-site-name").value = n;
      })
      .catch(() => {});
  }
}

function addKeyMode() {
  const checked = document.querySelector('input[name="add-site-key"]:checked');
  return checked ? checked.value : "ssh";
}

function openAddSite() {
  setSiteMenu(false);
  slugEdited = false;
  $("add-site-label").value = "";
  $("add-site-slug").value = "";
  for (const id of [
    "add-site-host",
    "add-site-remote",
    "add-site-domain",
    "add-site-url",
    "add-site-code",
    "add-site-name",
  ])
    $(id).value = "";
  document.querySelector('input[name="add-server-how"][value="ssh"]').checked = true;
  document.querySelector('input[name="add-site-key"][value="ssh"]').checked = true;
  $("add-site-guided").classList.add("hidden");
  $("add-site-key-out").classList.add("hidden");
  setAddMode("local");
  $("add-site-error").classList.add("hidden");
  $("add-site").classList.remove("hidden");
  setTimeout(() => $("add-site-label").focus(), 0);
}

async function submitAddSite() {
  const label = $("add-site-label").value.trim();
  const slug = $("add-site-slug").value.trim();
  const errEl = $("add-site-error");
  const fail = (msg) => {
    errEl.textContent = msg;
    errEl.classList.remove("hidden");
  };
  if (!label) return fail("Give the site a name.");
  if (!SLUG_RE.test(slug))
    return fail("The short name needs lowercase letters, digits and dashes, starting with a letter or digit.");
  if (siteRows.some((r) => r.slug === slug)) return fail(`There's already a site called "${slug}".`);
  const server = addMode === "server";
  const how = addHowMode();
  const host = $("add-site-host").value.trim();
  const url = $("add-site-url").value.trim();
  const name = $("add-site-name").value.trim();
  if (server && how === "ssh" && !host)
    return fail("Name the server: an SSH alias, a host, or user@host.");
  if (server && how !== "ssh") {
    if (!/^https:\/\//i.test(url))
      return fail("The site's address starts with https:// — the Manager never pairs over plain http.");
    if (!name) return fail("Give this Manager a name.");
    if (how === "code" && !$("add-site-code").value.trim())
      return fail("Type the install code the site's installer printed.");
  }
  const submit = $("add-site-submit");
  const idle = submit.textContent;
  submit.disabled = true;
  if (server) submit.textContent = how === "ssh" ? `Connecting to ${host}…` : "Talking to the site…";
  try {
    if (server && how === "code") {
      await invoke("pair_with_install_code", {
        slug,
        label,
        url,
        code: $("add-site-code").value.trim(),
        name,
      });
    } else if (server && how === "join") {
      const view = await invoke("join_start", { slug, label, url, name });
      $("add-site").classList.add("hidden");
      return waitForJoin(view);
    } else if (server) {
      await invoke("add_remote_site", {
        slug,
        label,
        host,
        remoteSite: $("add-site-remote").value.trim() || null,
        domain: $("add-site-domain").value.trim() || null,
        guided: addKeyMode() === "guided",
      });
    } else {
      await invoke("add_site", { slug, label });
    }
  } catch (e) {
    return fail(String(e));
  } finally {
    submit.disabled = false;
    submit.textContent = idle;
  }
  $("add-site").classList.add("hidden");
  selectedSlug = slug;
  resetSiteUi();
  currentTab = "home";
  await refresh().catch((e) => showError(String(e)));
}

// --- joining an engine-paired site (M4, E-02) -------------------------------
// The code, big and clear, while the window polls; the primary's owner types
// it. Polls 2 s, growing to 5 s; the request dies after 10 minutes.

let joinTimer = null;

function stopJoin() {
  if (joinTimer) clearTimeout(joinTimer);
  joinTimer = null;
  $("join-wait").classList.add("hidden");
}

function waitForJoin(view) {
  $("join-code").textContent = view.code;
  $("join-url").textContent = view.url;
  $("join-error").classList.add("hidden");
  $("join-left").textContent = `It works for ${Math.round(view.expires_in / 60)} minutes. Waiting…`;
  $("join-wait").classList.remove("hidden");
  let n = 0;
  const poll = async () => {
    let r;
    try {
      r = await invoke("join_poll");
    } catch (e) {
      $("join-error").textContent = String(e);
      $("join-error").classList.remove("hidden");
      joinTimer = setTimeout(poll, 5000);
      return;
    }
    if (r.state === "paired") {
      stopJoin();
      selectedSlug = r.slug;
      resetSiteUi();
      currentTab = "home";
      await refresh().catch((e) => showError(String(e)));
      return;
    }
    if (r.state === "expired") {
      stopJoin();
      showError("The request expired (or the site locked down) before it was approved. Ask to join again for a new code.");
      return;
    }
    const m = Math.floor(r.left / 60);
    $("join-left").textContent = `Waiting for the approval… ${m}:${String(r.left % 60).padStart(2, "0")} left.`;
    n += 1;
    joinTimer = setTimeout(poll, Math.min(2000 + 1000 * n, 5000));
  };
  joinTimer = setTimeout(poll, 2000);
}

function openRenameSite() {
  setSiteMenu(false);
  const row = currentSite();
  if (!row) return;
  $("rename-site-label").value = row.label;
  $("rename-site-error").classList.add("hidden");
  $("rename-site").classList.remove("hidden");
  setTimeout(() => $("rename-site-label").select(), 0);
}

async function submitRenameSite() {
  const row = currentSite();
  const label = $("rename-site-label").value.trim();
  const errEl = $("rename-site-error");
  if (!row) return;
  if (!label) {
    errEl.textContent = "The name can't be empty.";
    errEl.classList.remove("hidden");
    return;
  }
  try {
    await invoke("rename_site", { slug: row.slug, label });
  } catch (e) {
    errEl.textContent = String(e);
    errEl.classList.remove("hidden");
    return;
  }
  $("rename-site").classList.add("hidden");
  await refreshSites();
}

// The set-up screen, for whichever site is selected. A first run is a welcome;
// a site added next to others is named, and its console port is the one it was
// given — so "Set up" never moves it onto a port another site holds.
let installPortDefault = null;

function renderInstall() {
  const row = currentSite();
  const others = siteRows.length > 1;
  $("install-title").textContent = others && row ? `Set up ${row.label}` : "Welcome to Atelier";
  $("install-lede").textContent = others
    ? "This downloads Atelier and starts this site next to your others — it gets its own pages, backups and address."
    : "Let's set up your website. This downloads Atelier and starts it right here on your computer — the first time takes a minute or two. You'll choose your AI assistant afterward, inside Atelier.";
  $("install-btn-label").textContent = others ? "Set up this site" : "Set up Atelier";
  installPortDefault = (row && row.port) || 41221;
  $("install-port").value = installPortDefault;
}

// Paint the Home hero from the current status — and adapt the one primary action
// to whatever the person most likely wants to do next.
// The engine's word for the site (M5), when it runs one and answered.
function engineWord(status) {
  return (status && status.engine && status.engine.state) || null;
}

function renderStatus(status) {
  const dot = $("status-dot");
  const headline = $("status-headline");
  const sub = $("status-sub");
  const primary = $("home-primary");
  const primaryLabel = $("home-primary-label");
  const view = $("home-view");
  const toggle = $("home-toggle");

  // The address shown is the public site root (what visitors see), not the
  // /atelier console — clicking it "views" the site with no login wall.
  const url = status.site_url;
  const urlLink = $("console-url");
  const primaryIcon = $("home-primary-icon");
  urlLink.textContent = url;
  urlLink.href = url;
  // Only present the address as a live link once the site actually answers.
  urlLink.classList.toggle("hidden", !status.reachable || !url);
  const place = sitePlace(currentSite());

  // "Starting up…" is timed from when it began; any other state ends it.
  const starting = status.running && !status.reachable;
  if (!starting) startingSince = null;
  else if (startingSince === null) startingSince = Date.now();
  const slow = starting && Date.now() - startingSince >= STARTING_SLOW_MS;
  $("starting-slow").classList.toggle("hidden", !slow);
  // A site that answers settles any "not answering yet" line.
  // ...and so does a start that failed once the site is up again; a "still
  // starting" line is wrong the moment nothing is running.
  if (status.running && status.reachable && (homeNotice === NOT_READY_TEXT || homeNotice === DIDNT_FINISH.start)) {
    setHomeNotice(null);
  } else if (!status.running && homeNotice === NOT_READY_TEXT) {
    setHomeNotice(null);
  }
  syncCheckupButtons();

  if (status.running && status.reachable) {
    dot.className = "dot up";
    headline.textContent = "Your website is running";
    sub.textContent = `It's live ${place} and ready for you.`;
    // "Edit my site" → console, signed in. "View my site" → public front page.
    primary.disabled = false;
    primary.dataset.action = "edit";
    primaryIcon.setAttribute("href", "#i-open");
    primaryLabel.textContent = "Edit my site";
    // A site on a server without a recorded address has no URL to view
    // from here — "Edit my site" opens its console through a tunnel.
    view.classList.toggle("hidden", !url);
    toggle.classList.remove("hidden");
    $("startstop-label").textContent = engineWord(status) ? "Rest" : "Stop";
  } else if (status.running) {
    // Nothing else re-reads status here (a Start from this window waits for the
    // site itself), so a window opened while the site boots would stay on
    // "Starting up…" until something was clicked.
    if (!startingRecheck) {
      startingRecheck = setTimeout(() => {
        startingRecheck = null;
        refresh().catch((e) => showError(String(e)));
      }, 3000);
    }
    dot.className = "dot warn"; // waiting = ochre (0501), not yet running
    headline.textContent = "Starting up…";
    sub.textContent = slow
      ? "This is taking longer than usual."
      : "Almost there — this usually takes a few seconds.";
    primary.disabled = true;
    primary.dataset.action = "edit";
    primaryIcon.setAttribute("href", "#i-open");
    primaryLabel.textContent = "Starting…";
    view.classList.add("hidden");
    toggle.classList.remove("hidden");
    $("startstop-label").textContent = engineWord(status) ? "Rest" : "Stop";
  } else if (engineWord(status) === "resting" || engineWord(status) === "locked") {
    // Resting mode (M5): visitors still read the snapshot.
    const locked = engineWord(status) === "locked";
    // The switcher's dots: amber resting, grey locked down.
    dot.className = locked ? "dot down" : "dot warn";
    headline.textContent = locked ? "Your website is locked down" : "Your website is resting";
    sub.textContent = locked
      ? "Visitors read its saved pages; nothing else runs. Unlock it under System → Experimental features."
      : "Visitors read its saved pages; sign-in and forms are paused. Wake it to work on it.";
    primary.disabled = locked;
    primary.dataset.action = experimental ? "engine-wake" : "startstop";
    primaryIcon.setAttribute("href", "#i-play");
    primaryLabel.textContent = "Wake my website";
    view.classList.add("hidden");
    toggle.classList.add("hidden");
  } else {
    dot.className = "dot down";
    headline.textContent = "Your website is stopped";
    sub.textContent = "Start it whenever you'd like to work on your site.";
    primary.disabled = false;
    primary.dataset.action = "startstop";
    primaryIcon.setAttribute("href", "#i-play");
    primaryLabel.textContent = "Start my website";
    view.classList.add("hidden");
    toggle.classList.add("hidden");
  }

  // Keep any open panels honest about the new state.
  if (currentTab === "publish") updatePublishGate();
  if (currentTab === "backups") updateBackupsGate();
  if (currentTab === "settings") {
    $("image-tag").textContent = applianceName(status);
    renderChannel();
  }
}

// How long "Starting up…" may last before Home says so and offers a way on.
const STARTING_SLOW_MS = 3 * 60 * 1000;

// The Home line about the last start or update (null clears it).
function setHomeNotice(text, opts = {}) {
  homeNotice = text || null;
  $("home-notice-text").textContent = homeNotice || "";
  $("home-notice").classList.toggle("hidden", !homeNotice);
  showDockerSlot($("home-notice-docker"), !!(homeNotice && opts.dockerRestart));
  syncCheckupButtons();
}

// One "Run a checkup" at a time: the slow-start row has its own, so the notice
// under it drops its button while both are up.
function syncCheckupButtons() {
  $("home-notice-checkup").classList.toggle("hidden", !$("starting-slow").classList.contains("hidden"));
}

// "just now", "12 minutes ago", "3 hours ago", "yesterday", "on 12 Nov 2026".
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function fmtDay(d) {
  return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}`;
}

function timeAgo(unixSecs) {
  const then = new Date(unixSecs * 1000);
  const mins = Math.floor(Math.max(0, Date.now() - then.getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return mins === 1 ? "1 minute ago" : `${mins} minutes ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  const yesterday = new Date();
  yesterday.setDate(yesterday.getDate() - 1);
  if (then.toDateString() === yesterday.toDateString()) return "yesterday";
  return `on ${fmtDay(then)}`;
}

// The update answer Home last painted from: the cache status ({fresh,
// checked_at, check}) or null (never checked). Kept so the "checked … ago"
// words can be repainted every minute without asking anything.
let lastCachedUpdate = null;
// Checks in flight for the open site (a count: Check now can overlap the
// background poll, and the first to finish must not say "done" for both).
let updateChecks = 0;
// Bumped when the open site changes, so a slow check for the previous site
// can't paint its answer onto this one.
let siteGen = 0;
let updateAttempted = false; // a check has run for this site since it was opened
// A live check that couldn't answer ({text, since: checked_at it saw}). The
// cache keeps an older conclusive answer, so this says what the attempt found
// until a newer answer lands.
let updateCheckProblem = null;

// Release tags are `v<semver>`; the image label may or may not carry the `v`.
// Only a plain X.Y.Z names a tag (build metadata after `+` is cut); anything
// else ("edge", a digest, a prerelease) links the releases list instead of a 404.
const RELEASES_URL = "https://github.com/aincient-labs/atelier-cms/releases";
const releaseNotesUrl = (version) => {
  const v = String(version).split("+")[0].replace(/^v/, "");
  return /^\d+\.\d+\.\d+$/.test(v) ? `${RELEASES_URL}/tag/v${v}` : RELEASES_URL;
};

// Paint Home's update banner and line, and the System tab's "Last checked",
// from a cache status. Nothing shows for a site on a server: its check is the
// server's own (the cache is always null there).
function renderUpdateBanner(cached) {
  lastCachedUpdate = cached || null;
  const row = currentSite();
  const remote = !!(row && row.host);
  const check = cached && cached.check;
  if (updateCheckProblem && cached && cached.checked_at > updateCheckProblem.since) {
    updateCheckProblem = null;
  }
  const available = !remote && !!check && check.update_available === true;
  $("update-banner").classList.toggle("hidden", !available);

  const ago = cached ? `checked ${timeAgo(cached.checked_at)}` : "";
  $("update-last-checked").textContent = cached
    ? `Last checked ${timeAgo(cached.checked_at)}.`
    : "It hasn't checked yet.";

  if (available) {
    // A stepped update that stopped at a waypoint: continue it, not "a new version".
    const unfinished = !!check.unfinished;
    $("update-banner-title").textContent = unfinished
      ? "An update didn't finish."
      : "A new version of Atelier is ready.";
    $("update-banner-sub").textContent = unfinished
      ? "Your site is safe on the version it reached. Continue to finish the update."
      : "Updating keeps all your pages and settings.";
    $("update-banner-action").textContent = unfinished ? "Continue update" : "Update";
    const parts = [];
    if (check.current_version && check.latest_version) {
      parts.push(esc(`${check.current_version} → ${check.latest_version}`));
    }
    parts.push(esc(ago));
    if (check.latest_version) {
      const url = releaseNotesUrl(check.latest_version);
      parts.push(`<a href="${esc(url)}">What's new</a>`);
    }
    setHtml($("update-banner-meta"), parts.join(" · "));
  }

  const line = $("update-line");
  if (remote || available) {
    line.classList.add("hidden");
    return;
  }
  // Text and button are updated in place (never rebuilt), so the 60 s repaint
  // can't drop focus from "Check now".
  let text;
  let label = null; // the button's label, or null for none
  let title = "";
  if (updateCheckProblem || (check && check.update_available !== false && check.update_available !== true)) {
    const why = (updateCheckProblem && updateCheckProblem.text) || (check && check.problem) || "try again later";
    text = `Couldn't check for updates: ${shortProblem(why)}`;
    title = why;
    label = "Try again";
  } else if (check && check.update_available === false) {
    const v = check.current_version ? ` (${check.current_version})` : "";
    text = `Up to date${v} · ${ago}`;
    label = "Check now";
  } else if (updateChecks > 0 || !updateAttempted) {
    text = "Checking for updates…";
  } else {
    // Nothing cached and nothing running: not a failure, just not asked yet
    // (a fresh site, or one changed by an update or setup).
    text = "Not checked since the last change";
    label = "Check now";
  }
  const parts = updateLineParts(line);
  parts.text.textContent = label ? `${text} · ` : text;
  line.title = title;
  parts.button.classList.toggle("hidden", !label);
  if (label) {
    parts.button.textContent = updateChecks > 0 ? "Checking…" : label;
    parts.button.disabled = updateChecks > 0;
  }
  line.classList.remove("hidden");
}

// The update line's two stable children (text, button), made on first use.
function updateLineParts(line) {
  if (!line.firstElementChild) {
    const text = document.createElement("span");
    const button = document.createElement("button");
    button.className = "text-btn";
    button.dataset.action = "update-check-now";
    line.replaceChildren(text, button);
  }
  return { text: line.children[0], button: line.children[1] };
}

// Set innerHTML only when it changed, so a repaint doesn't replace a focused link.
function setHtml(el, html) {
  if (el.dataset.html !== html) {
    el.innerHTML = html;
    el.dataset.html = html;
  }
}

// Registry and Docker trouble in one short sentence; the full text stays in the
// line's title. Anything else is shown as it came.
function shortProblem(why) {
  const w = String(why);
  if (/docker|buildx/i.test(w)) return "Docker isn't ready.";
  if (/offline|registry|reach|resolve|timed? ?out|network/i.test(w)) return "couldn't reach the update server.";
  return w;
}

// Paint the banner from the on-disk cache — a file read, never the network.
async function refreshUpdate() {
  const gen = siteGen;
  try {
    const cached = await invoke("get_cached_update");
    if (gen === siteGen) renderUpdateBanner(cached);
  } catch {
    if (gen !== siteGen) return;
    $("update-banner").classList.add("hidden");
    $("update-line").classList.add("hidden");
  }
}

// A live check (Check now / Try again, and the System tab's button): asks the
// registry, records what this attempt found, then repaints from the cache.
// Returns the check, or throws what went wrong.
async function liveUpdateCheck() {
  const since = lastCachedUpdate ? lastCachedUpdate.checked_at : 0;
  const gen = siteGen;
  updateChecks++;
  renderUpdateBanner(lastCachedUpdate);
  try {
    const u = await invoke("get_update");
    if (gen !== siteGen) return u;
    const answered = u.update_available === true || u.update_available === false;
    updateCheckProblem = answered ? null : { text: u.problem || "try again later", since };
    return u;
  } catch (e) {
    if (gen === siteGen) updateCheckProblem = { text: String(e), since };
    throw e;
  } finally {
    if (gen === siteGen) {
      updateChecks--;
      updateAttempted = true;
      await refreshUpdate();
    }
  }
}

// "checked 12 minutes ago" must not go stale while the window sits open.
setInterval(() => {
  if (!$("update-line").classList.contains("hidden") || !$("update-banner").classList.contains("hidden")) {
    renderUpdateBanner(lastCachedUpdate);
  } else if (lastCachedUpdate) {
    $("update-last-checked").textContent = `Last checked ${timeAgo(lastCachedUpdate.checked_at)}.`;
  }
}, 60 * 1000);

// Background registry checks (manager#6). One forced check shortly after first
// paint, then a cheap poll: the backend only re-asks the registry once its cache
// is older than UPDATE_CACHE_TTL (6 h, Rust-side) or holds no conclusive answer,
// so most ticks are a file read. Single-flight in the backend, so a tick that
// lands during another check just shares its result. Silent on failure.
const UPDATE_POLL_MS = 30 * 60 * 1000;
let updateRefreshStarted = false;

async function backgroundUpdateRefresh(force) {
  // Only a call that will ask the registry says "Checking…"; a fresh cache is
  // a file read and repaints without a flicker.
  const asks = force || !lastCachedUpdate || !lastCachedUpdate.fresh;
  const gen = siteGen;
  if (asks) {
    updateChecks++;
    renderUpdateBanner(lastCachedUpdate);
  }
  try {
    const cached = await invoke("refresh_update_cache", { force });
    if (gen === siteGen) renderUpdateBanner(cached);
  } catch {
    /* offline / no stack / Docker down — the banner simply stays as it was. */
  } finally {
    if (gen === siteGen) {
      updateAttempted = true;
      if (asks) {
        updateChecks--;
        renderUpdateBanner(lastCachedUpdate);
      }
    }
  }
}

function startUpdateRefresh() {
  if (updateRefreshStarted) return;
  updateRefreshStarted = true;
  backgroundUpdateRefresh(true);
  setInterval(() => backgroundUpdateRefresh(false), UPDATE_POLL_MS);
}

// --- manager self-update nudge -----------------------------------------------
// The manager has no updater of its own; the most we owe people is knowing a
// newer one exists. One unauthenticated GitHub API call per launch (the 60/hr
// rate limit is plenty), fired after first render, and every failure path is
// silent — offline must look exactly like up to date.
const MANAGER_RELEASES_PAGE = "https://github.com/aincient-labs/manager/releases/latest";
let managerUpdateChecked = false;

// True when `latest` is a plain X.Y.Z strictly newer than `current`. Our stable
// tags carry no prerelease part, so anything that doesn't parse as three
// numbers is treated as "not newer" rather than guessed at.
function isNewerVersion(latest, current) {
  const a = String(latest).split(".").map(Number);
  const b = String(current).split(".").map(Number);
  if (a.length !== 3 || b.length !== 3 || [...a, ...b].some(Number.isNaN)) return false;
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] > b[i];
  return false;
}

// Give up on the GitHub call after this long (manager#6): a stalled connection
// must not leave a request hanging for the life of the window.
const MANAGER_UPDATE_TIMEOUT_MS = 5000;

async function checkManagerUpdate() {
  if (managerUpdateChecked || !managerVersion) return;
  managerUpdateChecked = true;
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), MANAGER_UPDATE_TIMEOUT_MS);
  try {
    const res = await fetch("https://api.github.com/repos/aincient-labs/manager/releases/latest", {
      headers: { Accept: "application/vnd.github+json" },
      signal: abort.signal,
    });
    if (!res.ok) return;
    const body = await res.json();
    const latest = String(body.tag_name || "").replace(/^v/, "");
    if (!isNewerVersion(latest, managerVersion)) return;
    $("manager-update-version").textContent = latest;
    $("manager-update-version-row").textContent = latest;
    $("manager-update-banner").classList.remove("hidden");
    $("manager-update-hint").classList.remove("hidden");
  } catch {
    // No network, timed out, rate-limited, API reshaped — all mean "say nothing".
  } finally {
    clearTimeout(timer);
  }
}

// --- Publish panel ----------------------------------------------------------

// Publish preferences are remembered between sessions (webview localStorage) so
// a repeat export doesn't ask for the same folder and address every time. The
// website address especially matters — localhost isn't a place anyone can visit,
// so links must be rendered against where the site will actually live.
//
// Kept per site: each site lives at its own address, so one site's website
// address must never be offered for another's export. The unscoped keys from
// before multi-site belong to the first site (`default`) and are read as its own.
const PREF_URL = "atelier.publish.baseUrl";
const PREF_DIR = "atelier.publish.dir";

function prefKey(base) {
  return selectedSlug ? `${base}.${selectedSlug}` : base;
}

function readPref(base) {
  const own = localStorage.getItem(prefKey(base));
  if (own !== null || (selectedSlug && selectedSlug !== "default")) return own;
  return localStorage.getItem(base);
}

function loadPublishPrefs() {
  $("export-baseurl").value = readPref(PREF_URL) || "";
  exportDir = readPref(PREF_DIR);
  $("export-dir").value = exportDir || "";
  $("export-btn").disabled = !exportDir;
}

function initExperimental() {
  $("experimental-toggle").addEventListener("change", async (e) => {
    const on = e.target.checked;
    try {
      experimental = !!(await invoke("set_experimental", { on })).experimental;
    } catch (err) {
      showError(String(err));
    }
    renderExperimental();
  });
}

function initPublishPrefs() {
  loadPublishPrefs();
  // Remember the address as it's typed, not only on export.
  $("export-baseurl").addEventListener("input", (e) => {
    const v = e.target.value.trim();
    if (v) localStorage.setItem(prefKey(PREF_URL), v);
    else localStorage.removeItem(prefKey(PREF_URL));
  });
}

// You can only export a running site, so gate the form gently rather than
// letting the export fail with a raw error.
function updatePublishGate() {
  const running = !!(lastStatus && lastStatus.running);
  $("publish-needs-running").classList.toggle("hidden", running);
  $("publish-form").classList.toggle("hidden", !running);
  $("export-btn").classList.toggle("hidden", !running);
  // A server writes the export itself: no folder to choose here.
  const row = currentSite();
  if (row && row.host) $("export-btn").disabled = false;
  else $("export-btn").disabled = !exportDir;
}

function revealExport() {
  if (!lastExportPath) return;
  invoke("reveal_path", { path: lastExportPath }).catch((e) => showError(String(e)));
}

// Hand the export off to a host: put the folder on screen (it's about to be
// dragged) and open the host in the system browser, never in this window.
function openHost(url) {
  revealExport();
  invoke("open_url", { url }).catch((e) => showError(String(e)));
}

// Copy to the clipboard and say so on the button itself — a silent copy leaves
// people wondering whether it worked.
async function copyText(text, button) {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    showError("Couldn't reach the clipboard — you can select the text and copy it by hand.");
    return;
  }
  if (!button) return;
  const label = button.querySelector("span") ?? button;
  if (button.dataset.copyBusy) return;
  button.dataset.copyBusy = "1";
  const original = label.textContent;
  label.textContent = "Copied";
  setTimeout(() => {
    label.textContent = original;
    delete button.dataset.copyBusy;
  }, 1600);
}

// --- Backups panel ----------------------------------------------------------

let hasBackups = false; // the list holds at least one backup

// Backing up and restoring both need the site up: offer the start, as Publish
// does, rather than letting either fail with "isn't running". Saving a copy of
// an existing backup is only a file copy, so it stays available. A site whose
// database was removed from Docker can't start (Start refuses and points here),
// so it may restore while stopped: the restore starts it on an empty database
// and loads the backup into it.
const BACKUPS_NEEDS_RUNNING = "Your website needs to be running to make a backup or restore one.";

function updateBackupsGate() {
  const up = !!(lastStatus && lastStatus.running);
  // Running isn't answering: a backup of a site still booting fails or is
  // partial, so Back up now waits. A restore only needs the containers up (it
  // runs inside them), and a site that runs but errors — broken by a
  // migration, say — is exactly the one that needs it (review 2026-11-14).
  const answering = up && !!lastStatus.reachable;
  const notAnswering = up && !answering;
  const erased = !up && !!(lastStatus && lastStatus.database_erased);
  const canRestore = up || erased;
  $("backups-needs-running").classList.toggle("hidden", answering || erased);
  $("backups-needs-running-text").textContent = notAnswering
    ? "Your website isn't answering yet (still starting, or it has a problem), so it can't " +
      "be backed up until it does. You can restore a backup now."
    : BACKUPS_NEEDS_RUNNING;
  document.querySelector('#backups-needs-running [data-action="startstop-from-publish"]').classList.toggle("hidden", notAnswering);
  $("backups-database-erased").classList.toggle("hidden", !erased);
  $("backup-now").classList.toggle("hidden", !answering);
  document.querySelector('[data-action="restore"]').disabled = !(canRestore && hasBackups);
  $("btn-import").disabled = !canRestore;
}

// What a backup's file name says it was taken for, in words.
const BACKUP_LABELS = {
  "before-update": "Before update",
  "before-stepped-upgrade": "Before update",
  "before-restore": "Before restore",
  "before-channel-switch": "Before switching versions",
};

// "14 Nov 2026, 10:15 · 12 MB", led by why it was taken when the name says
// ("Before update · …"). Names are `aincient-[label-]YYYYMMDD-HHMMSS[-N].tar.gz`
// (the `aincient-` prefix is not part of the label: it read "Aincient before
// update"); any other file (an imported one) keeps its name so it can still be
// told apart.
function backupText(b) {
  const d = new Date(b.modified_unix * 1000);
  const time = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const parts = [`${fmtDay(d)}, ${time}`, fmtBytes(b.size_bytes)];
  const m = /^(?:aincient-)?(?:(.+?)-)?\d{8}-\d{6}(?:-\d+)?\.(?:tar\.gz|tgz)$/i.exec(b.name);
  if (!m) parts.push(b.name);
  else if (m[1]) {
    const words = m[1].replace(/-/g, " ");
    parts.unshift(BACKUP_LABELS[m[1]] || words.charAt(0).toUpperCase() + words.slice(1));
  }
  return parts.join(" · ");
}

async function refreshBackups() {
  const select = $("backup-select");
  const empty = $("backups-empty");
  const exportBtn = $("btn-export");
  const setEnabled = (on) => {
    hasBackups = on;
    exportBtn.disabled = !on;
    empty.classList.toggle("hidden", on);
    select.classList.toggle("hidden", !on);
    updateBackupsGate();
  };
  updateBackupsGate();
  try {
    const backups = await invoke("list_backups");
    select.innerHTML = "";
    if (!backups.length) {
      setEnabled(false);
      return;
    }
    for (const b of backups) {
      const opt = document.createElement("option");
      opt.value = b.path;
      opt.textContent = backupText(b);
      opt.title = b.name;
      select.appendChild(opt);
    }
    setEnabled(true);
  } catch {
    setEnabled(false);
  }
}

// --- Settings panel ---------------------------------------------------------

// What the System tab calls the appliance: the version its container reports
// (v0.19.2), else its image — a bare digest cut to a short hash.
function applianceName(st) {
  if (!st) return "—";
  if (st.version) return st.version;
  const digest = /^(?:(.*)@)?sha256:([0-9a-f]{12})[0-9a-f]*$/.exec(st.image || "");
  if (digest) return `${digest[1] ? `${digest[1]}@` : ""}sha256:${digest[2]}`;
  return st.image || "—";
}

// System → Disk space (DECISIONS 0527): what Atelier can remove because it can
// download it again. Machine-wide; refreshed whenever the System tab opens.
function decimalBytes(n) {
  const mb = n / 1e6;
  return mb >= 1000 ? `${(mb / 1000).toFixed(1)} GB` : `${Math.max(1, Math.round(mb))} MB`;
}

// While a clean-up runs, a refresh (the System tab reopening) must not turn
// the button back on.
let cleaning = false;
// The last list shown, so the confirmation names exactly what will go.
let cleanupFound = null;
// Each local site's older automatic backups (taken by a Manager before 0.17,
// never pruned): offered as unticked items in the confirmation.
let olderFound = [];

function olderBackupsLabel(o) {
  const n = o.backups.length;
  return `${n} older automatic backup${n === 1 ? "" : "s"} of ${o.label}, ${decimalBytes(o.total_bytes)}`;
}

function cleanupLabel(i) {
  return i.names[0] || `${i.repo} (old version)`;
}

async function loadCleanup() {
  const btn = $("free-space");
  if (cleaning) return;
  try {
    const [r, older] = await Promise.all([
      invoke("find_cleanup"),
      invoke("find_older_backups").catch(() => []),
    ]);
    if (cleaning) return;
    cleanupFound = r;
    olderFound = older || [];
    $("cleanup-summary").title = "";
    if (!r) {
      $("cleanup-summary").textContent = "Docker isn't answering";
      btn.disabled = true;
      return;
    }
    const n = r.images.length;
    const olderBytes = olderFound.reduce((sum, o) => sum + o.total_bytes, 0);
    const parts = [];
    if (n) parts.push(`${decimalBytes(r.total_bytes)} can be freed (${n} download${n === 1 ? "" : "s"})`);
    if (olderFound.length) parts.push(`${decimalBytes(olderBytes)} in older backups`);
    $("cleanup-summary").textContent = parts.length ? parts.join(" · ") : "Nothing to free";
    btn.disabled = n === 0 && !olderFound.length;
  } catch (e) {
    if (cleaning) return;
    cleanupFound = null;
    $("cleanup-summary").textContent = "—";
    $("cleanup-summary").title = String(e);
    btn.disabled = true;
  }
}

function loadSettings() {
  loadCleanup();
  $("image-tag").textContent = applianceName(lastStatus);
  $("manager-version-row").textContent = managerVersion || "—";
  renderChannel();
  loadExperimental();
}

// --- experimental features (m7-3, DECISIONS 0483) ---------------------------
// Off by default and kept per user by the Rust side (~/.atelier/manager.json).
// While off, everything engine-related stays hidden — except a plain note when
// the site already runs the engine, so a root-holding helper is never invisible.

let experimental = false;

async function loadExperimental() {
  try {
    experimental = !!(await invoke("get_settings")).experimental;
  } catch {
    experimental = false;
  }
  renderExperimental();
}

function renderExperimental() {
  $("experimental-toggle").checked = experimental;
  $("experimental-body").classList.toggle("hidden", !experimental);
  const hasEngine = !!(lastStatus && lastStatus.engine && lastStatus.engine.enabled);
  $("engine-hidden-note").classList.toggle("hidden", experimental || !hasEngine);
  if (experimental) refreshEngine();
}

const ENGINE_BUTTONS = [
  "engine-enable",
  "engine-lockdown",
  "engine-unlock",
  "engine-rest",
  "engine-wake",
  "engine-wake-code",
  "engine-wake-code-revoke",
  "engine-remove",
  "engine-update-manager",
];

// Resting mode (M5): the engine's word for the site, in the window's words.
const ENGINE_STATES = {
  resting: "Resting",
  waking: "Waking",
  awake: "Awake",
  locked: "Locked down",
  stopped: "Stopped",
};

// Rest or wake (whichever applies), and the wake code, for an unlocked site.
function restingButtons(v) {
  const shown = [v.state === "resting" ? "engine-wake" : "engine-rest", "engine-wake-code"];
  if (v.wake_code_set) shown.push("engine-wake-code-revoke");
  return v.state === "waking" ? ["engine-wake-code"] : shown;
}

function engineStateText(v) {
  const word = ENGINE_STATES[v.state] || (v.lockdown ? "Locked down" : "Running");
  return v.api ? `${word} (API ${v.api})` : word;
}

function showEngineButtons(shown) {
  for (const id of ENGINE_BUTTONS) $(id).classList.toggle("hidden", !shown.includes(id));
}

// Bumped on every refresh: an answer for a site switched away from is dropped.
let engineAsked = 0;

async function refreshEngine() {
  const state = $("engine-state");
  const problem = $("engine-problem");
  const asked = ++engineAsked;
  state.textContent = "Checking…";
  problem.classList.add("hidden");
  showEngineButtons([]);
  // Nothing of the previous site's engine stays up while this one is asked.
  $("managers-panel").classList.add("hidden");
  let v;
  try {
    v = await invoke("engine_state");
  } catch (e) {
    if (asked !== engineAsked) return;
    state.textContent = "Unknown";
    problem.textContent = String(e);
    problem.classList.remove("hidden");
    return;
  }
  if (asked !== engineAsked) return;
  $("managers-panel").classList.toggle("hidden", !v.paired);
  if (v.paired) {
    // Paired by token, without SSH: lock down / unlock and the Managers;
    // adding or removing the engine needs SSH.
    state.textContent = `Paired · ${engineStateText(v)}`;
    showEngineButtons(
      v.lockdown ? ["engine-unlock"] : [...restingButtons(v), "engine-lockdown"]
    );
    refreshManagers();
    return;
  }
  if (!v.enabled) {
    state.textContent = "Not on this site";
    showEngineButtons(["engine-enable"]);
    return;
  }
  if (!v.running) {
    state.textContent = "Added, but not running";
    showEngineButtons(["engine-remove"]);
    return;
  }
  if (v.problem) {
    state.textContent = v.api ? `Running (API ${v.api})` : "Running";
    problem.textContent = v.problem;
    problem.classList.remove("hidden");
    // A newer engine than this Manager understands (C-08): update the Manager
    // first; nothing else is offered until then.
    showEngineButtons(v.manager_too_old ? ["engine-update-manager"] : ["engine-remove"]);
    return;
  }
  if (v.lockdown) {
    state.textContent = "Locked down";
    showEngineButtons(["engine-unlock"]);
    return;
  }
  state.textContent = engineStateText(v);
  showEngineButtons([...restingButtons(v), "engine-lockdown", "engine-remove"]);
}

// --- Managers (M4, E-02) -----------------------------------------------------
// Every Manager paired with this site, senior first. The primary also sees who
// waits to join: name and address, never a code.

let lastManagers = null;

function ago(at) {
  if (!at) return "never";
  const s = Math.max(0, Math.floor(Date.now() / 1000) - at);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} days ago`;
}

async function refreshManagers() {
  const list = $("managers-list");
  const hint = $("managers-hint");
  let v;
  try {
    v = await invoke("engine_managers");
  } catch (e) {
    list.replaceChildren();
    hint.textContent = String(e);
    $("requests-box").classList.add("hidden");
    return;
  }
  lastManagers = v;
  const myIndex = v.pairings.findIndex((p) => p.id === v.self);
  const frag = document.createDocumentFragment();
  v.pairings.forEach((p, i) => {
    const li = document.createElement("li");
    const rank = p.primary ? "Primary" : `#${i + 1}`;
    const me = p.id === v.self ? " · this Manager" : "";
    li.innerHTML =
      `<span class="m-text"><span class="m-name">${esc(p.name)}</span>` +
      `<span class="m-meta">${rank}${me} · last seen ${esc(ago(p.last_used))}</span></span>`;
    // A Manager revokes only those junior to it (listed after it), never the primary.
    if (p.id !== v.self && !p.primary && myIndex >= 0 && i > myIndex) {
      const b = document.createElement("button");
      b.className = "btn small";
      b.dataset.action = "engine-revoke";
      b.dataset.id = p.id;
      b.dataset.name = p.name;
      b.textContent = "Revoke";
      li.appendChild(b);
    }
    frag.appendChild(li);
  });
  list.replaceChildren(frag);
  const waiting = (v.requests || []).filter((r) => !r.approved);
  $("requests-box").classList.toggle("hidden", !v.primary || waiting.length === 0);
  const rf = document.createDocumentFragment();
  for (const r of waiting) {
    const li = document.createElement("li");
    li.innerHTML =
      `<span class="m-text"><span class="m-name">${esc(r.name)}</span>` +
      `<span class="m-meta">from ${esc(r.ip)} · asked ${esc(ago(r.created))}</span></span>` +
      `<button class="btn small primary" data-action="approve-open">Approve…</button>`;
    rf.appendChild(li);
  }
  $("requests-list").replaceChildren(rf);
  hint.textContent = v.primary
    ? "This is the site's primary Manager: new Managers join by asking, then you type the code they show."
    : "Only the site's primary Manager approves new ones.";
}

function openApprove() {
  const v = lastManagers;
  if (!v) return;
  const row = currentSite();
  $("approve-title").textContent = `Let a Manager into ${row ? row.label : "this site"}`;
  const frag = document.createDocumentFragment();
  for (const r of (v.requests || []).filter((r) => !r.approved)) {
    const li = document.createElement("li");
    li.innerHTML =
      `<span class="m-text"><span class="m-name">${esc(r.name)}</span>` +
      `<span class="m-meta">asking from ${esc(r.ip)} · ${esc(ago(r.created))}</span></span>`;
    frag.appendChild(li);
  }
  $("approve-requests").replaceChildren(frag);
  // Never filled in: the owner reads the code off the new Manager and types it.
  $("approve-code").value = "";
  $("approve-error").classList.add("hidden");
  $("approve").classList.remove("hidden");
  setTimeout(() => $("approve-code").focus(), 0);
}

async function submitApprove() {
  const code = $("approve-code").value.trim();
  const errEl = $("approve-error");
  const fail = (m) => {
    errEl.textContent = m;
    errEl.classList.remove("hidden");
  };
  if (!code) return fail("Type the code the new Manager shows.");
  $("approve").classList.add("hidden");
  const ok = await confirmChange("Let the Manager showing that code manage {site}?", "", {
    always: true,
  });
  if (!ok) return;
  let name;
  try {
    name = await invoke("engine_approve", { code });
  } catch (e) {
    $("approve").classList.remove("hidden");
    return fail(String(e));
  }
  await refreshManagers();
  $("managers-hint").textContent = `Approved "${name}". It finishes pairing on its own within seconds; refresh to see it listed.`;
}

// Which images this install follows. The menu offers released versions and
// nothing else: edge ships unreleased work and is not somewhere to end up by
// picking a menu item (`atelier app channel edge` is the deliberate way in). A
// pinned image, or an install already on edge, belongs to neither offer — each
// gets its own option so the row tells the truth and shows the way back, rather
// than being misreported as the supported channel.
function renderChannel() {
  const select = $("channel-select");
  const hint = $("channel-hint");
  const channel = (lastStatus && lastStatus.channel) || "stable";
  select.querySelector('option[value="pinned"]')?.remove();
  select.querySelector('option[value="edge"]')?.remove();
  if (channel === "pinned" || channel === "edge") {
    const opt = document.createElement("option");
    opt.value = channel;
    opt.textContent =
      channel === "edge"
        ? "Every new build (unreleased)"
        : "This exact version (pinned)";
    select.prepend(opt);
  }
  select.value = channel;
  hint.textContent =
    channel === "edge"
      ? "Unreleased builds off main — for testing. Switch to released versions when you're done."
      : channel === "pinned"
        ? "Pinned to one image, so no updates arrive. Pick a channel to follow updates again."
        : "Released versions only — the supported choice.";
}

async function refreshLogs() {
  const view = $("logs-view");
  const svc = $("logs-service").value || null;
  view.textContent = "Loading…";
  try {
    const out = await invoke("get_logs", { service: svc, lines: 400 });
    view.textContent = out.trim() || "Nothing here yet.";
    view.scrollTop = view.scrollHeight;
  } catch (e) {
    view.textContent = String(e);
  }
}

// --- Troubleshoot (doctor) --------------------------------------------------
let doctorRunning = false;

// Tier headings, in the order the core reports them: outward-in, from the
// machine to the site. Plain words — the core's tier ids are the stable handle,
// these are just what a person reads.
function doctorTiers() {
  const row = currentSite();
  if (row && row.host) {
    return [
      ["connection", `Reaching ${row.host}`],
      ["host", `The server (${row.host})`],
      ["stack", "Atelier on the server"],
      ["site", "Your website"],
    ];
  }
  return [
    ["host", "Your computer"],
    ["stack", "Atelier itself"],
    ["site", "Your website"],
  ];
}

const DOCTOR_MARK = { ok: "✓", warn: "!", fail: "✕", skipped: "–" };

// Render a report from the core. Skipped checks are folded away unless
// something actually failed — on a healthy appliance nothing is skipped, and on
// a broken one they're the trail showing how far the checkup got.
function renderDoctor(report) {
  const results = $("doctor-results");
  const summary = $("doctor-summary");
  const failures = report.checks.filter((c) => c.severity === "fail").length;
  const warnings = report.checks.filter((c) => c.severity === "warn").length;

  let html = "";
  for (const [tier, title] of doctorTiers()) {
    const checks = report.checks.filter((c) => c.tier === tier);
    if (!checks.length) continue;
    html += `<h4 class="doctor-tier">${esc(title)}</h4><ul class="doctor-list">`;
    for (const c of checks) {
      html += `<li class="doctor-item sev-${esc(c.severity)}">`;
      html += `<span class="doctor-mark" aria-hidden="true">${DOCTOR_MARK[c.severity] || "?"}</span>`;
      html += `<span class="doctor-body"><span class="doctor-label">${esc(c.label)}</span>`;
      if (c.detail) html += `<span class="doctor-detail">${esc(firstLine(c.detail))}</span>`;
      if (c.remedy) {
        html += `<span class="doctor-remedy">${esc(c.remedy)}`;
        // Restarting Docker is the repair: offer it here too, asked first.
        if (c.fix === "restart_docker" && c.severity === "fail") {
          html += `<span class="docker-slot" data-docker-slot="text"></span>`;
        }
        html += "</span>";
      }
      html += "</span></li>";
    }
    html += "</ul>";
  }

  if (report.actions && report.actions.length) {
    html += `<h4 class="doctor-tier">What we repaired</h4><ul class="doctor-list">`;
    for (const a of report.actions) {
      const sev = a.succeeded ? "ok" : "fail";
      html += `<li class="doctor-item sev-${sev}">`;
      html += `<span class="doctor-mark" aria-hidden="true">${DOCTOR_MARK[sev]}</span>`;
      html += `<span class="doctor-body"><span class="doctor-label">${esc(a.description)}</span>`;
      if (!a.succeeded && a.detail) {
        html += `<span class="doctor-detail">${esc(firstLine(a.detail))}</span>`;
      }
      html += "</span></li>";
    }
    html += "</ul>";
  }

  results.innerHTML = html;
  results.classList.remove("hidden");
  const slot = results.querySelector("[data-docker-slot]");
  if (slot) showDockerSlot(slot, true);

  if (failures === 0) {
    summary.textContent = warnings
      ? "Everything essential is working. There's one thing worth a look below."
      : "Everything looks good.";
  } else {
    summary.textContent =
      failures === 1
        ? "We found 1 problem. Details below."
        : `We found ${failures} problems. Details below.`;
  }
  summary.classList.remove("hidden");

  // Only offer the repair button when there's actually something it can do —
  // an unfixable failure (a port taken by another program) needs the person,
  // not a button that would appear to do nothing.
  const fixable = report.checks.some(
    (c) => c.fix && (c.severity === "fail" || c.severity === "warn")
  );
  $("doctor-fix").classList.toggle("hidden", !fixable);
}

function firstLine(s) {
  return String(s).split("\n").map((l) => l.trim()).find((l) => l) || "";
}

function esc(s) {
  return String(s).replace(
    /[&<>"']/g,
    (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]
  );
}

// --- actions ----------------------------------------------------------------

const actions = {
  recheck: () => refresh(),

  "toggle-sites": () => setSiteMenu($("site-menu").classList.contains("hidden")),
  "select-site": (el) => switchSite(el.dataset.slug),
  "add-site-open": () => openAddSite(),
  "add-site-cancel": () => $("add-site").classList.add("hidden"),
  "add-site-mode": (el) => setAddMode(el.dataset.mode),
  "add-site-key": async () => {
    const host = $("add-site-host").value.trim();
    const errEl = $("add-site-error");
    errEl.classList.add("hidden");
    if (!host) {
      errEl.textContent = "Name the server first: the key is made for it.";
      errEl.classList.remove("hidden");
      return;
    }
    try {
      const key = await invoke("guided_key", { host });
      $("add-site-pubkey").textContent = key.public_line;
      $("add-site-keycmd").textContent = key.command;
      $("add-site-key-out").classList.remove("hidden");
    } catch (e) {
      errEl.textContent = String(e);
      errEl.classList.remove("hidden");
    }
  },
  "add-site-copy-cmd": (el) => copyText($("add-site-keycmd").textContent, el),
  "rename-site-open": () => openRenameSite(),
  "rename-site-cancel": () => $("rename-site").classList.add("hidden"),

  "manager-release": () =>
    invoke("open_url", { url: MANAGER_RELEASES_PAGE }).catch((e) => showError(String(e))),

  "dismiss-error": () => hideError(),

  // From the failure panel: close it first, then ask (runProgressOp runs
  // pm.afterClose once the panel is gone).
  "restart-docker": () => {
    if (pm.release) {
      pm.afterClose = restartDockerFlow;
      pm.release();
      return;
    }
    restartDockerFlow();
  },
  "copy-docker-cmd": (el) => copyText(dockerRt && dockerRt.copy, el),

  // A refused update's way forward (showBlockers): close the failure panel,
  // then run the window's own action for it.
  blocker: (el) => {
    const next = actions[el.dataset.next];
    if (!next) return;
    if (pm.release) {
      pm.afterClose = () => next(el);
      pm.release();
      return;
    }
    next(el);
  },
  "free-space-open": () => {
    showTab("settings");
    $("free-space").scrollIntoView({ block: "center" });
    $("free-space").focus({ preventScroll: true });
  },
  "backups-open": () => showTab("backups"),

  // "Edit my site" — drop the operator into the console signed in. A fresh
  // appliance's admin password is never shown, so a plain /atelier link would
  // access-deny; open_console_authed mints a one-time login link instead.
  edit: () => invoke("open_console_authed").catch((e) => showError(String(e))),

  // "View my site" — the public front page, anonymous-viewable, no login wall.
  view: () => {
    const url = lastStatus && lastStatus.site_url;
    if (!url) return;
    invoke("open_url", { url }).catch((e) => showError(String(e)));
  },

  login: () => invoke("open_login").catch((e) => showError(String(e))),

  "reset-password": () => {
    $("reset-pw-input").value = "";
    $("reset-pw").classList.remove("hidden");
    setTimeout(() => $("reset-pw-input").focus(), 0);
  },

  "reset-pw-cancel": () => $("reset-pw").classList.add("hidden"),

  "reset-pw-submit": () => {
    const password = $("reset-pw-input").value;
    if (!password.trim()) return;
    $("reset-pw").classList.add("hidden");
    return runProgressOp("Setting the admin password", () =>
      invoke("set_admin_password", { password })
    );
  },

  install: () => {
    // Only an edited port is sent: the backend gives a site without one the
    // port it was registered on, or a free one.
    const typed = parseInt($("install-port").value, 10) || null;
    const port = typed === installPortDefault ? null : typed;
    return runProgressOp("Setting up Atelier", () => invoke("do_install", { image: null, port }), {
      lifecycle: "setup",
    });
  },

  // A very old site can't always reach the newest release in one move: a release
  // that drops code can only migrate a site that's already past it. The backend
  // works the route out and walks it; this asks first, because a multi-step
  // upgrade runs several migrations and takes far longer than the one restart
  // "Update" implies. Any failure to plan is not a reason to block the update —
  // the appliance refuses an impossible migration itself, without touching data.
  update: async () => {
    let plan = null;
    try {
      // Reuse the cached plan while it's fresh (manager#6) — the button must not
      // wait on the registry. Otherwise recompute live: that check is deadlined
      // backend-side, and if it finds no connection at all we say so and stop,
      // since the update's own pull could only fail (or stall) offline.
      const cached = await invoke("get_cached_update");
      if (cached && cached.fresh) {
        plan = cached.check.plan;
      } else {
        const u = await invoke("get_update");
        if (u.offline) {
          showError(u.problem || "You're offline — couldn't reach the registry.");
          refreshUpdate();
          return;
        }
        plan = u.plan;
      }
    } catch {
      /* couldn't plan — update anyway; the appliance is the real gate. */
    }
    const stepped = !!(plan && plan.steps && plan.steps.length > 1);
    let detail = "";
    if (stepped) {
      const route = plan.steps
        .map((s, i) => `${i + 1}. ${s.version || s.image}`)
        .join("\n");
      detail =
        `Getting to the newest version takes ${plan.steps.length} steps, because it ` +
        `can't move a site this old there directly:\n\n${route}\n\n` +
        `Each step updates and checks your site before the next one starts, and a ` +
        `full backup is taken first. This will take a while — please leave it running.`;
    }
    // A stepped route always asks; a live site on a server always asks for its slug.
    if (!(await confirmChange("Update {site}?", detail, { always: stepped }))) return;
    const update = (noBackup) =>
      runProgressOp("Updating Atelier", () => invoke("do_update", { noBackup }), {
        lifecycle: "update",
      });
    if (await update(false)) return true;
    // The site couldn't be backed up first (stopped, and it won't start on the
    // version it runs — often why it is being updated): nothing was updated.
    if (!(lastOpOutcome && lastOpOutcome.backup_failed)) return false;
    const go = await confirmChange(
      "Update {site} without a backup?",
      "Atelier couldn't back up your site before the update, so it wasn't updated.\n\n" +
        "Updating without a backup means that if the update goes wrong, your pages, images, " +
        "and settings as they are now can't be brought back.",
      { always: true, yes: "Update without a backup" }
    );
    if (!go) return false;
    return update(true);
  },

  // Start or stop, decided by the live status rather than a label.
  startstop: async () => {
    const running = !!(lastStatus && lastStatus.running);
    // With the engine, Stop is its Rest and Start its Wake (DECISIONS 0488).
    const engine = !!engineWord(lastStatus);
    // Stopping a live site on a server takes its slug (D-07); locally it never asked.
    if (
      running &&
      !(await (engine
        ? confirmChange(
            "Put {site} to rest?",
            "A fresh snapshot is saved first, then the site's app and database stop. Visitors " +
              "keep reading the snapshot; sign-in and forms are paused until it is woken. If the " +
              "snapshot fails, the site stays awake.",
            { always: true }
          )
        : confirmChange("Stop {site}?", "Visitors can't reach it until it's started again.")))
    )
      return;
    const label = running
      ? engine
        ? "Putting the site to rest"
        : "Stopping your website"
      : engine
        ? "Waking the site"
        : "Starting your website";
    await runProgressOp(label, () => invoke(running ? "do_stop" : "do_start"), {
      lifecycle: running ? "clear" : "start",
    });
    if (engine) refreshEngine();
  },

  "startstop-from-publish": async () => {
    await runProgressOp("Starting your website", () => invoke("do_start"), { lifecycle: "start" });
  },

  // "Run a checkup" from Home (a slow start, a start or update that didn't
  // finish): the System tab's checkup, started for you.
  "checkup-now": () => {
    showTab("settings");
    const run = actions.doctor(); // shows the summary line before it awaits
    $("doctor-summary").scrollIntoView({ block: "center" });
    return run;
  },

  // "View activity" from a slow start: the System tab's log, loaded.
  "view-activity": () => {
    showTab("settings");
    $("logs-view").scrollIntoView({ block: "center" });
    return refreshLogs();
  },

  // Home's "Check now" / "Try again": the same live check as the System tab.
  "update-check-now": () => liveUpdateCheck().catch(() => {}),

  // ---- Publish ----
  "pick-export-dir": async () => {
    try {
      const dir = await invoke("pick_export_dir");
      if (!dir) return;
      exportDir = dir;
      $("export-dir").value = dir;
      $("export-btn").disabled = false;
      localStorage.setItem(prefKey(PREF_DIR), dir);
    } catch (e) {
      showError(String(e));
    }
  },

  "export-site": async () => {
    const onServer = !!(currentSite() && currentSite().host);
    if (!exportDir && !onServer) return;
    lastExportPath = null;
    $("publish-result").classList.add("hidden");
    const baseUrl = $("export-baseurl").value.trim() || null;
    const ok = await runProgressOp("Exporting your site", async () => {
      lastExportPath = await invoke("site_export", {
        out: onServer ? null : exportDir,
        baseUrl,
        zip: $("export-zip").checked,
        includeConfig: $("export-config").checked,
        includeUsers: $("export-users").checked,
        skipLinkCheck: $("export-skiplinks").checked,
      });
    });
    if (ok && lastExportPath) {
      lastExportHadBaseUrl = baseUrl !== null;
      $("export-path").textContent = lastExportPath;
      $("export-cli-cmd").textContent = `netlify deploy --prod --dir="${lastExportPath}"`;
      $("export-baseurl-warn").classList.toggle("hidden", lastExportHadBaseUrl);
      $("publish-result").classList.remove("hidden");
    }
  },

  "reveal-export": () => revealExport(),

  "copy-export-path": (el) => {
    if (lastExportPath) copyText(lastExportPath, el);
  },

  "copy-deploy-cmd": (el) => copyText($("export-cli-cmd").textContent, el),

  // Each host hand-off does the same two things: reveal the export folder so
  // it's sitting there to drag, then open the host in the real browser.
  "host-netlify": () => openHost("https://app.netlify.com/drop"),
  "host-cloudflare": () => openHost("https://dash.cloudflare.com/?to=/:account/pages/new/upload"),
  "host-template": () =>
    openHost("https://github.com/aincient-labs/atelier-deploy-template/generate"),

  // ---- Backups ----
  backup: () => runProgressOp("Backing up your site", () => invoke("do_backup", { label: null })),

  restore: async () => {
    const path = $("backup-select").value;
    if (!path) return;
    return restoreFlow(path, "backup");
  },

  export: async () => {
    const source = $("backup-select").value;
    if (!source) return;
    try {
      await invoke("export_backup", { source });
    } catch (e) {
      showError(String(e));
    }
  },

  import: async () => {
    let path;
    try {
      path = await invoke("pick_restore_file");
    } catch (e) {
      return showError(String(e));
    }
    if (!path) return; // cancelled
    return restoreFlow(path, "file");
  },

  // ---- Settings ----
  "refresh-logs": () => refreshLogs(),

  doctor: async () => {
    // One checkup at a time, and one that belongs to the open site: a slow run
    // for the site just left must not paint its report onto this one.
    if (doctorRunning) return;
    const gen = siteGen;
    const summary = $("doctor-summary");
    summary.classList.remove("hidden");
    summary.textContent = "Checking…";
    $("doctor-results").classList.add("hidden");
    $("doctor-fix").classList.add("hidden");
    doctorRunning = true;
    try {
      const report = await invoke("run_doctor");
      if (gen === siteGen) renderDoctor(report);
    } catch (e) {
      if (gen === siteGen) summary.textContent = String(e);
    } finally {
      doctorRunning = false;
    }
  },

  "doctor-fix": async () => {
    // Repairs can take minutes (a self-heal run streams its own output), so this
    // goes through the shared progress overlay rather than a silent button.
    let report = null;
    await runProgressOp(
      "Repairing your website",
      async () => {
        report = await invoke("run_doctor_fix");
      },
      { lifecycle: "repair" }
    );
    if (report) renderDoctor(report);
  },

  "free-space": async () => {
    const btn = $("free-space");
    const status = $("cleanup-status");
    const found = cleanupFound;
    const older = olderFound;
    if (cleaning || !found || (!found.images.length && !older.length)) return;
    const n = found.images.length;
    const parts = [];
    if (n) {
      const lines = found.images
        .map((i) => `${decimalBytes(i.size_bytes)}  ${cleanupLabel(i)}`)
        .join("\n");
      parts.push(`${lines}\n\nEach downloads again when a site needs it.`);
    }
    if (older.length) {
      parts.push(
        "Earlier versions of the Manager kept every automatic backup (before updates, " +
          "restores and version switches). Tick any you want removed too; they can't be " +
          "brought back. Your own backups and the newest automatic ones always stay."
      );
    }
    parts.push("Your sites' data is untouched.");
    const opts = {
      title: n
        ? `Remove ${n} download${n === 1 ? "" : "s"} (up to ${decimalBytes(found.total_bytes)})?`
        : "Remove older automatic backups?",
      yes: "Remove",
      checks: older.map(olderBackupsLabel),
    };
    const ok = await confirmModal(parts.join("\n\n"), opts);
    if (!ok) return;
    const paths = older
      .filter((_, i) => opts.checked[i])
      .flatMap((o) => o.backups.map((b) => b.path));
    if (!n && !paths.length) return;
    cleaning = true;
    btn.disabled = true;
    status.classList.remove("hidden");
    status.textContent = "Freeing up space…";
    const said = [];
    try {
      if (n) {
        const r = await invoke("do_cleanup", { ids: found.images.map((i) => i.id) });
        const m = r.images.length;
        said.push(
          m
            ? `Freed up to ${decimalBytes(r.total_bytes)} (${m} download${m === 1 ? "" : "s"}).`
            : "No downloads to remove."
        );
        if (r.failed.length) said.push(`Kept ${r.failed.length} that Docker says are in use.`);
      }
      if (paths.length) {
        const r = await invoke("remove_older_backups", { paths });
        const m = r.removed.length;
        said.push(`Removed ${m} older backup${m === 1 ? "" : "s"} (${decimalBytes(r.total_bytes)}).`);
        if (r.failed.length) said.push(`Couldn't remove ${r.failed.length}.`);
      }
      said.push("Your sites' data is untouched.");
      status.textContent = said.join(" ");
    } catch (e) {
      status.textContent = [...said, String(e)].join(" ");
    } finally {
      cleaning = false;
    }
    loadCleanup();
    if (paths.length && currentTab === "backups") refreshBackups();
  },

  "check-update": async () => {
    const s = $("update-status");
    s.classList.remove("hidden");
    s.textContent = "Checking…";
    try {
      const u = await liveUpdateCheck();
      // Name the versions when the image says what it is — on a rolling tag the
      // digest changes every build, so "an update is available" alone is noise.
      const from = u.current_version ? ` (you're on ${u.current_version})` : "";
      if (u.update_available === true) {
        const to = u.latest_version ? `Version ${u.latest_version}` : "A new version";
        // Say up front when it's a multi-step route: it's the difference between
        // an update you start before a meeting and one you don't.
        const steps =
          u.plan && u.plan.steps && u.plan.steps.length > 1
            ? ` It takes ${u.plan.steps.length} steps from where you are, so allow extra time.`
            : "";
        s.textContent = `${to} is available — go to Home to update.${from}${steps}`;
      } else if (u.update_available === false) {
        s.textContent = `You're on the latest version${u.current_version ? ` (${u.current_version})` : ""}.`;
      } else {
        // Say which of the four causes it was, not a guess at two of them.
        s.textContent = u.problem || "Couldn't check for updates right now.";
      }
    } catch (e) {
      s.textContent = String(e);
    }
  },

  down: async () => {
    const wipe = $("down-wipe").checked;
    const msg = wipe
      ? "This removes Atelier AND erases all your data — pages, images, settings, and password. This cannot be undone."
      : "This removes the running containers. Your data is kept safe, and you can start again anytime.";
    const ok = await confirmChange(
      wipe ? "Remove and erase {site}?" : "Remove the containers of {site}?",
      msg,
      { always: true, requireText: wipe ? "erase" : null }
    );
    if (!ok) return;
    return runProgressOp(wipe ? "Removing and erasing" : "Removing containers", () =>
      invoke("do_down", { wipe })
    );
  },

  // ---- Experimental: the engine (m3-6, m3-7, m7-3) ----
  "engine-enable": async () => {
    const ok = await confirmChange(
      "Add the experimental engine to {site}?",
      "The engine holds the server's Docker socket, which is full control (root) of that " +
        "server. It is for testers. You can remove it again from here.",
      { always: true }
    );
    if (!ok) return;
    let code = null;
    await runProgressOp("Adding the engine", async () => {
      code = await invoke("engine_enable");
    });
    if (code) {
      // m4-6: shown once, next to the site's address.
      const row = currentSite();
      $("install-code-site").textContent =
        (lastStatus && lastStatus.site_url) || (row ? row.label : "this site");
      $("install-code-value").textContent = code;
      $("install-code").classList.remove("hidden");
    }
    refreshEngine();
  },

  // ---- Pairing without SSH (M4, E-02) ----
  "install-code-copy": (el) => copyText($("install-code-value").textContent, el),
  "install-code-close": () => {
    $("install-code-value").textContent = "";
    $("install-code").classList.add("hidden");
  },
  "join-cancel": async () => {
    stopJoin();
    await invoke("join_cancel").catch(() => {});
  },
  "approve-open": () => openApprove(),
  "approve-cancel": () => $("approve").classList.add("hidden"),
  "engine-revoke": async (el) => {
    const ok = await confirmChange(
      `Revoke "${el.dataset.name}" from {site}?`,
      "It stops working on its next call. It can join again only if the primary approves it.",
      { always: true }
    );
    if (!ok) return;
    try {
      await invoke("engine_revoke", { id: el.dataset.id });
    } catch (e) {
      showError(String(e));
    }
    refreshManagers();
  },
  "engine-forget": async () => {
    const ok = await confirmModal(
      "This Manager's pairing is revoked on the site (if it still exists), its key is deleted " +
        "from your keychain, and the site leaves your list. Joining again needs the primary's approval.",
      { title: "Forget this site on this Manager?" }
    );
    if (!ok) return;
    try {
      await invoke("engine_forget");
    } catch (e) {
      return showError(String(e));
    }
    selectedSlug = null;
    resetSiteUi();
    currentTab = "home";
    setView("loading");
    await refresh().catch((e) => showError(String(e)));
  },

  "engine-lockdown": async () => {
    const ok = await confirmChange(
      "Lock down {site}?",
      "The site stops at once. Waking it and pairing close, and every paired Manager but " +
        "the first is removed. Only this Manager over SSH, or the first paired Manager, " +
        "can unlock it.",
      { always: true }
    );
    if (!ok) return;
    await runProgressOp("Locking the site down", () => invoke("engine_lockdown", { lock: true }));
    refreshEngine();
  },

  "engine-unlock": async () => {
    const ok = await confirmChange(
      "Unlock {site}?",
      "Waking and pairing open again. The site stays resting until it is woken, by its " +
        "wake code at /atelier or from here.",
      { always: true }
    );
    if (!ok) return;
    await runProgressOp("Unlocking", () => invoke("engine_lockdown", { lock: false }));
    refreshEngine();
  },

  // ---- Resting mode (M5) ----
  "engine-rest": async () => {
    const ok = await confirmChange(
      "Put {site} to rest?",
      "A fresh snapshot is saved first, then the site's app and database stop. Visitors " +
        "keep reading the snapshot; sign-in and forms are paused until it is woken. If the " +
        "snapshot fails, the site stays awake.",
      { always: true }
    );
    if (!ok) return;
    await runProgressOp("Putting the site to rest", () => invoke("engine_rest", { rest: true }), {
      lifecycle: "clear",
    });
    refreshEngine();
    refresh().catch((e) => showError(String(e)));
  },

  "engine-wake": async () => {
    await runProgressOp("Waking the site", () => invoke("engine_rest", { rest: false }), {
      lifecycle: "clear",
    });
    refreshEngine();
    refresh().catch((e) => showError(String(e)));
  },

  "engine-wake-code": async () => {
    const ok = await confirmChange(
      "Make a new wake code for {site}?",
      "The old code stops working. The new one is shown once.",
      { always: true }
    );
    if (!ok) return;
    let code = null;
    try {
      code = await invoke("engine_wake_code", { revoke: false });
    } catch (e) {
      return showError(String(e));
    }
    if (code) {
      $("wake-code-value").textContent = code;
      $("wake-code").classList.remove("hidden");
    }
    refreshEngine();
  },

  "engine-wake-code-revoke": async () => {
    const ok = await confirmChange(
      "Turn waking by code off for {site}?",
      "The wake code stops working. A Manager can still wake the site, and you can make a " +
        "new code at any time.",
      { always: true }
    );
    if (!ok) return;
    try {
      await invoke("engine_wake_code", { revoke: true });
    } catch (e) {
      return showError(String(e));
    }
    refreshEngine();
  },

  "wake-code-copy": (el) => copyText($("wake-code-value").textContent, el),
  "wake-code-close": () => {
    $("wake-code-value").textContent = "";
    $("wake-code").classList.add("hidden");
  },

  "engine-remove": async () => {
    const ok = await confirmChange(
      "Remove the engine from {site}?",
      "The engine is taken off and the site restarts as before (a resting site is woken " +
        "first). Its saved data is kept.",
      { always: true }
    );
    if (!ok) return;
    await runProgressOp("Removing the engine", () => invoke("engine_remove"));
    refreshEngine();
  },

  // The Manager has no updater of its own: its update is the releases page.
  "engine-update-manager": () =>
    invoke("open_url", { url: MANAGER_RELEASES_PAGE }).catch((e) => showError(String(e))),

  reinstall: async () => {
    const ok = await confirmChange(
      "Reinstall {site} from scratch?",
      "Reinstalling erases everything — your pages, images, settings, and password — and sets up a fresh Atelier. This cannot be undone.",
      { always: true, requireText: "confirm" }
    );
    if (!ok) return;
    return runProgressOp("Reinstalling from scratch", () => invoke("do_reinstall"), {
      lifecycle: "setup",
    });
  },
};

// Event delegation for tabs and every [data-action] control.
document.addEventListener("click", (e) => {
  if (!e.target.closest("#site-switcher")) setSiteMenu(false);

  const navItem = e.target.closest(".nav-item");
  if (navItem) {
    showTab(navItem.dataset.tab);
    return;
  }

  const target = e.target.closest("[data-action]");
  if (target) {
    const name = target.getAttribute("data-action");
    const fn = actions[name];
    if (fn) {
      e.preventDefault();
      fn(target);
    }
    return;
  }

  // External links (docs/guides) must open in the system browser — a plain
  // <a target="_blank"> does nothing inside the Tauri WebView, so hand the URL
  // to the Rust opener instead.
  const link = e.target.closest('a[href^="http"]');
  if (link) {
    e.preventDefault();
    invoke("open_url", { url: link.href }).catch((err) => showError(String(err)));
  }
});

// Changing the channel pulls the other image and converges onto it, so it goes
// through the same confirm + progress overlay as any other lifecycle op. On
// cancel (or failure) the select is put back where it was — it must never show a
// channel the appliance isn't actually on.
$("channel-select").addEventListener("change", async (e) => {
  const target = e.target.value;
  // The two self-describing options are where you already are, not somewhere to
  // go — selecting one is a no-op that re-renders the row.
  if (target === "pinned" || target === "edge") {
    renderChannel();
    return;
  }
  const msg =
    "Edge can be ahead of the newest release, and a site's database only migrates forward. Atelier snapshots first and rolls back if the switch fails. Switch to released versions?";
  if (!(await confirmChange("Switch {site} to released versions?", msg, { always: true }))) {
    renderChannel();
    return;
  }
  // runProgressOp refreshes the status itself (and reports failures), so the row
  // is re-rendered from what the appliance now reports, not from the click.
  await runProgressOp("Switching update channel", () => invoke("set_channel", { channel: target }), {
    lifecycle: "update",
  });
  renderChannel();
});

$("add-site-form").addEventListener("submit", (e) => {
  e.preventDefault();
  submitAddSite();
});
$("approve-form").addEventListener("submit", (e) => {
  e.preventDefault();
  submitApprove();
});
// The code is typed from the new Manager's screen: no paste, no drop.
for (const ev of ["paste", "drop"]) {
  $("approve-code").addEventListener(ev, (e) => {
    e.preventDefault();
    $("approve-error").textContent = "Type the code as the new Manager shows it.";
    $("approve-error").classList.remove("hidden");
  });
}
document.querySelectorAll('input[name="add-server-how"]').forEach((el) =>
  el.addEventListener("change", setHowMode)
);
$("rename-site-form").addEventListener("submit", (e) => {
  e.preventDefault();
  submitRenameSite();
});
// The short name follows the name until someone edits it by hand.
$("add-site-label").addEventListener("input", (e) => {
  if (!slugEdited) $("add-site-slug").value = slugify(e.target.value);
});
$("add-site-slug").addEventListener("input", () => {
  slugEdited = true;
});
document.querySelectorAll('input[name="add-site-key"]').forEach((el) =>
  el.addEventListener("change", () =>
    $("add-site-guided").classList.toggle("hidden", addKeyMode() !== "guided")
  )
);
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (!$("site-menu").classList.contains("hidden")) {
    setSiteMenu(false);
    $("site-current").focus();
  }
  $("add-site").classList.add("hidden");
  $("rename-site").classList.add("hidden");
  $("approve").classList.add("hidden");
  if (!$("mismatch").classList.contains("hidden")) $("mismatch-no").click();
});

initPublishPrefs();
initExperimental();
// The nudge needs managerVersion to compare against, so it chains off the
// version stamp — still fire-and-forget, nothing here delays first paint.
showManagerVersion().then(checkManagerUpdate);
// Background update checks start once the first refresh has painted (or failed),
// so a slow registry can never delay the window (manager#6).
refresh()
  .catch((e) => showError(String(e)))
  .finally(startUpdateRefresh);
