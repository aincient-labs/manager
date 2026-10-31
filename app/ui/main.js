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

function showError(msg) {
  $("error-text").textContent = msg;
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
    const input = $("confirm-input");
    if (opts.title) $("confirm-site").classList.add("hidden");
    else showSiteLine($("confirm-site"));
    $("confirm-title").textContent = opts.title || "";
    $("confirm-title").classList.toggle("hidden", !opts.title);
    $("confirm-msg").textContent = msg || "";
    $("confirm-lead").textContent = opts.lead || "";
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
      $("confirm").classList.add("hidden");
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
    });
  }
  return confirmModal(detail, { title, requireText: opts.requireText || null });
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
  return runProgressOp("Restoring your backup", () => invoke("do_restore", { path, otherSite }));
}

// Whisper labels for each core Stage, shown in the progress sub-status / feed.
const STAGE_LABELS = {
  preflight: "Checking Docker",
  scaffold: "Preparing",
  pull: "Downloading",
  starting: "Starting",
  booting: "Booting the console",
  ready: "Ready",
  working: "Working",
};

let lastStage = null;
// Whether this op reported a numeric fraction (a phased op like install/update),
// so it owns the bar — vs. an indeterminate op (backup/stop/…) we finish green.
let sawFraction = false;

function progressReset(title) {
  showSiteLine($("progress-site"));
  $("progress-title").textContent = title;
  $("progress-fill").style.width = "0%";
  $("progress-fill").classList.remove("done");
  $("progressbar").classList.add("indeterminate"); // until a fraction arrives
  $("progress-stage").textContent = "Working…";
  $("progress-log").textContent = "";
  resetPull();
  lastStage = null;
  sawFraction = false;
}

// Settle the bar to a full mint "done" — for indeterminate ops that finished OK.
function progressFinish() {
  $("progressbar").classList.remove("indeterminate");
  $("progress-fill").style.width = "100%";
  $("progress-fill").classList.add("done");
  $("progress-stage").textContent = "Done.";
}

function appendLog(line, cls) {
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
}

function progressUpdate(p) {
  if (typeof p.fraction === "number") {
    sawFraction = true;
    $("progressbar").classList.remove("indeterminate");
    $("progress-fill").style.width = `${Math.round(p.fraction * 100)}%`;
    if (p.fraction >= 1) $("progress-fill").classList.add("done");
  }
  if (p.kind === "pull") {
    pullUpdate(p.pull);
    return;
  }
  if (p.kind === "log") {
    if (p.message.trim()) appendLog(p.message, logClass(p.message));
    return;
  }
  $("progress-stage").textContent = p.message;
  if (p.stage !== lastStage) {
    appendLog(`▸ ${STAGE_LABELS[p.stage] || p.message}`);
    lastStage = p.stage;
  }
}

// Wrap any long op in the progress panel: stream its phases/steps via op-progress
// events, then refresh. Returns whether the op completed without error.
async function runProgressOp(title, fn) {
  // A new attempt supersedes the previous failure — leaving the old banner up
  // makes a retry that succeeds look like it failed.
  hideError();
  progressReset(title);
  $("progress").classList.remove("hidden");
  const unlisten = await listen("op-progress", (e) => progressUpdate(e.payload));
  let ok = false;
  try {
    await fn();
    if (!sawFraction) progressFinish();
    ok = true;
    await refresh();
  } catch (e) {
    showError(String(e));
  } finally {
    unlisten();
    $("progress").classList.add("hidden");
  }
  return ok;
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
      const rested = engineWord(status) === "resting";
      remoteStates[row.slug] = {
        state: status.running ? "running" : rested ? "resting" : "stopped",
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
};

function siteState(row) {
  if (row.host) {
    const st = remoteStates[row.slug];
    if (!st) return "Checking…";
    if (st.state === "unreachable") return "Can't reach it";
    return REMOTE_STATE_WORDS[st.state] || "—";
  }
  if (!row.installed) return "Not set up yet";
  return row.running ? "Running" : "Stopped";
}

// The switcher line under a site's name. A server that didn't answer is
// named outright: "Can't reach deploy@vps.example.com".
function siteMeta(row) {
  const st = row.host && remoteStates[row.slug];
  if (st && st.state === "unreachable") return `Can't reach ${row.host}`;
  return `${siteHost(row)} · ${siteState(row)}`;
}

function siteDot(row) {
  if (!row.host) return row.running ? "up" : "down";
  const st = remoteStates[row.slug];
  if (!st) return "";
  return (
    { running: "up", frozen: "warn", resting: "warn", stopped: "down", unreachable: "warn" }[
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
    $("startstop-label").textContent = "Stop";
  } else if (status.running) {
    dot.className = "dot up";
    headline.textContent = "Starting up…";
    sub.textContent = "Almost there — this usually takes a few seconds.";
    primary.disabled = true;
    primary.dataset.action = "edit";
    primaryIcon.setAttribute("href", "#i-open");
    primaryLabel.textContent = "Starting…";
    view.classList.add("hidden");
    toggle.classList.remove("hidden");
    $("startstop-label").textContent = "Stop";
  } else if (engineWord(status) === "resting" || engineWord(status) === "locked") {
    // Resting mode (M5): visitors still read the snapshot.
    const locked = engineWord(status) === "locked";
    dot.className = "dot down";
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
  if (currentTab === "settings") {
    $("image-tag").textContent = applianceName(status);
    renderChannel();
  }
}

// Show/hide the Home update banner from a cache status ({fresh, checked_at,
// check}) or null (never checked) — hidden unless the check says yes.
function renderUpdateBanner(cached) {
  $("update-banner").classList.toggle("hidden", !(cached && cached.check.update_available === true));
}

// Paint the banner from the on-disk cache — a file read, never the network.
async function refreshUpdate() {
  try {
    renderUpdateBanner(await invoke("get_cached_update"));
  } catch {
    $("update-banner").classList.add("hidden");
  }
}

// Background registry checks (manager#6). One forced check shortly after first
// paint, then a cheap poll: the backend only re-asks the registry once its cache
// is older than UPDATE_CACHE_TTL (6 h, Rust-side) or holds no conclusive answer,
// so most ticks are a file read. Single-flight in the backend, so a tick that
// lands during another check just shares its result. Silent on failure.
const UPDATE_POLL_MS = 30 * 60 * 1000;
let updateRefreshStarted = false;

async function backgroundUpdateRefresh(force) {
  try {
    renderUpdateBanner(await invoke("refresh_update_cache", { force }));
  } catch {
    /* offline / no stack / Docker down — the banner simply stays as it was. */
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

async function refreshBackups() {
  const select = $("backup-select");
  const empty = $("backups-empty");
  const restoreBtn = document.querySelector('[data-action="restore"]');
  const exportBtn = $("btn-export");
  const setEnabled = (on) => {
    restoreBtn.disabled = !on;
    exportBtn.disabled = !on;
    empty.classList.toggle("hidden", on);
    select.classList.toggle("hidden", !on);
  };
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
      const mb = (b.size_bytes / 1048576).toFixed(1);
      opt.textContent = `${b.name}  (${mb} MB)`;
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

function loadSettings() {
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

async function refreshEngine() {
  const state = $("engine-state");
  const problem = $("engine-problem");
  state.textContent = "Checking…";
  problem.classList.add("hidden");
  showEngineButtons([]);
  let v;
  try {
    v = await invoke("engine_state");
  } catch (e) {
    state.textContent = "Unknown";
    problem.textContent = String(e);
    problem.classList.remove("hidden");
    return;
  }
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
      if (c.remedy) html += `<span class="doctor-remedy">${esc(c.remedy)}</span>`;
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
    return runProgressOp("Setting up Atelier", () => invoke("do_install", { image: null, port }));
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
    return runProgressOp("Updating Atelier", () => invoke("do_update"));
  },

  // Start or stop, decided by the live status rather than a label.
  startstop: async () => {
    const running = !!(lastStatus && lastStatus.running);
    // Stopping a live site on a server takes its slug (D-07); locally it never asked.
    if (
      running &&
      !(await confirmChange("Stop {site}?", "Visitors can't reach it until it's started again."))
    )
      return;
    return runProgressOp(running ? "Stopping your website" : "Starting your website", () =>
      invoke(running ? "do_stop" : "do_start")
    );
  },

  "startstop-from-publish": () =>
    runProgressOp("Starting your website", () => invoke("do_start")),

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
    const summary = $("doctor-summary");
    summary.classList.remove("hidden");
    summary.textContent = "Checking…";
    $("doctor-results").classList.add("hidden");
    $("doctor-fix").classList.add("hidden");
    try {
      renderDoctor(await invoke("run_doctor"));
    } catch (e) {
      summary.textContent = String(e);
    }
  },

  "doctor-fix": async () => {
    // Repairs can take minutes (a self-heal run streams its own output), so this
    // goes through the shared progress overlay rather than a silent button.
    let report = null;
    await runProgressOp("Repairing your website", async () => {
      report = await invoke("run_doctor_fix");
    });
    if (report) renderDoctor(report);
  },

  "check-update": async () => {
    const s = $("update-status");
    s.classList.remove("hidden");
    s.textContent = "Checking…";
    try {
      const u = await invoke("get_update");
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
      refreshUpdate();
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
    await runProgressOp("Putting the site to rest", () => invoke("engine_rest", { rest: true }));
    refreshEngine();
    refresh().catch((e) => showError(String(e)));
  },

  "engine-wake": async () => {
    await runProgressOp("Waking the site", () => invoke("engine_rest", { rest: false }));
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
    return runProgressOp("Reinstalling from scratch", () => invoke("do_reinstall"));
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
  await runProgressOp("Switching update channel", () => invoke("set_channel", { channel: target }));
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
