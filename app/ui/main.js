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
const VIEWS = ["loading", "docker", "install", "app"];
const PANELS = ["home", "publish", "backups", "settings"];

let lastStatus = null; // most recent get_status, so panels can adapt to it
let currentTab = "home";
let exportDir = null; // folder chosen for the static export
let lastExportPath = null; // where the last export landed, for "Open the folder"

// --- view + tab routing -----------------------------------------------------

function setView(name) {
  for (const v of VIEWS.slice(0, 3)) $(`screen-${v}`).classList.toggle("hidden", name !== v);
  const app = name === "app";
  $("tabbar").classList.toggle("hidden", !app);
  for (const p of PANELS) $(`panel-${p}`).classList.toggle("hidden", !(app && p === currentTab));
  if (app) showTab(currentTab);
}

function showTab(tab) {
  currentTab = tab;
  for (const p of PANELS) $(`panel-${p}`).classList.toggle("hidden", p !== tab);
  document
    .querySelectorAll(".tab")
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

// In-page confirm (the webview blocks native confirm() without the dialog plugin).
// Pass { requireText: "confirm" } to gate a destructive action behind a typed word.
function confirmModal(msg, opts = {}) {
  return new Promise((resolve) => {
    const requireText = opts.requireText || null;
    const yes = $("confirm-yes");
    const input = $("confirm-input");
    $("confirm-msg").textContent = msg;
    if (requireText) {
      $("confirm-word").textContent = requireText;
      input.value = "";
      $("confirm-typecheck").classList.remove("hidden");
      yes.disabled = true;
      input.oninput = () => {
        yes.disabled = input.value.trim().toLowerCase() !== requireText.toLowerCase();
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
  $("progress-title").textContent = title;
  $("progress-fill").style.width = "0%";
  $("progress-fill").classList.remove("done");
  $("progressbar").classList.add("indeterminate"); // until a fraction arrives
  $("progress-stage").textContent = "Working…";
  $("progress-log").textContent = "";
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

function appendLog(line) {
  const log = $("progress-log");
  log.textContent += (log.textContent ? "\n" : "") + line;
  log.scrollTop = log.scrollHeight;
}

function progressUpdate(p) {
  if (typeof p.fraction === "number") {
    sawFraction = true;
    $("progressbar").classList.remove("indeterminate");
    $("progress-fill").style.width = `${Math.round(p.fraction * 100)}%`;
    if (p.fraction >= 1) $("progress-fill").classList.add("done");
  }
  if (p.kind === "log") {
    if (p.message.trim()) appendLog(p.message);
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
  const problem = await invoke("preflight_problem");
  if (problem) {
    $("docker-msg").textContent = problem;
    setView("docker");
    return;
  }

  const status = await invoke("get_status");
  lastStatus = status;
  if (!status.installed) {
    setView("install");
    return;
  }

  renderStatus(status);
  setView("app");
  // Best-effort, non-blocking enrichment.
  refreshUpdate();
}

// Paint the Home hero from the current status — and adapt the one primary action
// to whatever the person most likely wants to do next.
function renderStatus(status) {
  const dot = $("status-dot");
  const headline = $("status-headline");
  const sub = $("status-sub");
  const primary = $("home-primary");
  const primaryLabel = $("home-primary-label");
  const toggle = $("home-toggle");

  const url = status.console_url;
  const urlLink = $("console-url");
  const primaryIcon = $("home-primary-icon");
  urlLink.textContent = url;
  urlLink.href = url;
  // Only present the address as a live link once it actually answers.
  urlLink.classList.toggle("hidden", !status.reachable);

  if (status.running && status.reachable) {
    dot.className = "dot up";
    headline.textContent = "Your website is running";
    sub.textContent = "It's live on this computer and ready for you.";
    primary.disabled = false;
    primary.dataset.action = "open";
    primaryIcon.setAttribute("href", "#i-open");
    primaryLabel.textContent = "Open my website";
    toggle.classList.remove("hidden");
    $("startstop-label").textContent = "Stop";
  } else if (status.running) {
    dot.className = "dot up";
    headline.textContent = "Starting up…";
    sub.textContent = "Almost there — this usually takes a few seconds.";
    primary.disabled = true;
    primary.dataset.action = "open";
    primaryIcon.setAttribute("href", "#i-open");
    primaryLabel.textContent = "Starting…";
    toggle.classList.remove("hidden");
    $("startstop-label").textContent = "Stop";
  } else {
    dot.className = "dot down";
    headline.textContent = "Your website is stopped";
    sub.textContent = "Start it whenever you'd like to work on your site.";
    primary.disabled = false;
    primary.dataset.action = "startstop";
    primaryIcon.setAttribute("href", "#i-play");
    primaryLabel.textContent = "Start my website";
    toggle.classList.add("hidden");
  }

  // Keep any open panels honest about the new state.
  if (currentTab === "publish") updatePublishGate();
  if (currentTab === "settings") $("image-tag").textContent = status.image || "—";
}

async function refreshUpdate() {
  try {
    const u = await invoke("get_update");
    $("update-banner").classList.toggle("hidden", u.update_available !== true);
  } catch {
    $("update-banner").classList.add("hidden");
  }
}

// --- Publish panel ----------------------------------------------------------

// Publish preferences are remembered between sessions (webview localStorage) so
// a repeat export doesn't ask for the same folder and address every time. The
// website address especially matters — localhost isn't a place anyone can visit,
// so links must be rendered against where the site will actually live.
const PREF_URL = "atelier.publish.baseUrl";
const PREF_DIR = "atelier.publish.dir";

function initPublishPrefs() {
  const url = localStorage.getItem(PREF_URL);
  if (url) $("export-baseurl").value = url;
  const dir = localStorage.getItem(PREF_DIR);
  if (dir) {
    exportDir = dir;
    $("export-dir").value = dir;
    $("export-btn").disabled = false;
  }
  // Remember the address as it's typed, not only on export.
  $("export-baseurl").addEventListener("input", (e) => {
    const v = e.target.value.trim();
    if (v) localStorage.setItem(PREF_URL, v);
    else localStorage.removeItem(PREF_URL);
  });
}

// You can only export a running site, so gate the form gently rather than
// letting the export fail with a raw error.
function updatePublishGate() {
  const running = !!(lastStatus && lastStatus.running);
  $("publish-needs-running").classList.toggle("hidden", running);
  $("publish-form").classList.toggle("hidden", !running);
  $("export-btn").classList.toggle("hidden", !running);
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

function loadSettings() {
  $("image-tag").textContent = (lastStatus && lastStatus.image) || "—";
  loadModels();
}

// A model role, rendered read-first with a foldable inline editor for the
// curious. Most people never touch this — they set AI up inside Atelier.
function roleRow(r) {
  const row = document.createElement("div");
  row.className = "role";

  const head = document.createElement("div");
  head.className = "role-head";

  const name = document.createElement("span");
  name.className = "role-name";
  name.textContent = r.label || r.role;
  if (r.default === "yes") {
    const star = document.createElement("span");
    star.className = "star";
    star.textContent = "★";
    star.title = "This is the default the console uses";
    name.appendChild(star);
  }

  const bind = document.createElement("span");
  const set = r.provider && r.model;
  bind.className = set ? "role-binding" : "role-binding unset";
  bind.textContent = set ? `${r.provider} · ${r.model}` : "Not set";

  const edit = document.createElement("button");
  edit.className = "btn ghost small";
  edit.textContent = "Edit";
  edit.onclick = () => row.classList.toggle("editing");

  head.append(name, bind, edit);

  const editor = document.createElement("div");
  editor.className = "role-edit";
  const provider = document.createElement("input");
  provider.placeholder = "Provider (e.g. anthropic)";
  provider.value = r.provider || "";
  const model = document.createElement("input");
  model.placeholder = "Model (e.g. claude-sonnet-5)";
  model.value = r.model || "";
  const save = document.createElement("button");
  save.className = "btn small primary";
  save.textContent = "Save";
  save.onclick = async () => {
    if (!provider.value.trim() || !model.value.trim()) return;
    save.disabled = true;
    save.textContent = "Saving…";
    try {
      await invoke("set_model_role", {
        role: r.role,
        provider: provider.value.trim(),
        model: model.value.trim(),
      });
      await loadModels();
    } catch (e) {
      showError(String(e));
      save.disabled = false;
      save.textContent = "Save";
    }
  };
  editor.append(provider, model, save);

  row.append(head, editor);
  return row;
}

async function loadModels() {
  const box = $("model-roles");
  const note = $("model-note");
  box.innerHTML = "";
  note.classList.add("hidden");
  if (!(lastStatus && lastStatus.running)) {
    note.textContent = "Start your website to view and adjust its AI settings.";
    note.classList.remove("hidden");
    return;
  }
  try {
    const roles = await invoke("get_model_roles");
    if (!roles.length) {
      note.textContent = "No AI connected yet — open Atelier to set it up.";
      note.classList.remove("hidden");
      return;
    }
    for (const r of roles) box.appendChild(roleRow(r));
  } catch {
    note.textContent = "Couldn't read the AI settings — open Atelier to set it up there.";
    note.classList.remove("hidden");
  }
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

// --- actions ----------------------------------------------------------------

const actions = {
  recheck: () => refresh(),

  "dismiss-error": () => $("error").classList.add("hidden"),

  open: () => invoke("open_console").catch((e) => showError(String(e))),

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
    const port = parseInt($("install-port").value, 10) || null;
    return runProgressOp("Setting up Atelier", () => invoke("do_install", { image: null, port }));
  },

  update: () => runProgressOp("Updating Atelier", () => invoke("do_update")),

  // Start or stop, decided by the live status rather than a label.
  startstop: () => {
    const running = !!(lastStatus && lastStatus.running);
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
      localStorage.setItem(PREF_DIR, dir);
    } catch (e) {
      showError(String(e));
    }
  },

  "export-site": async () => {
    if (!exportDir) return;
    lastExportPath = null;
    $("publish-result").classList.add("hidden");
    const ok = await runProgressOp("Exporting your site", async () => {
      lastExportPath = await invoke("site_export", {
        out: exportDir,
        baseUrl: $("export-baseurl").value.trim() || null,
        zip: $("export-zip").checked,
        includeConfig: $("export-config").checked,
        includeUsers: $("export-users").checked,
        skipLinkCheck: $("export-skiplinks").checked,
      });
    });
    if (ok && lastExportPath) {
      $("export-path").textContent = lastExportPath;
      $("publish-result").classList.remove("hidden");
    }
  },

  "reveal-export": () => {
    if (!lastExportPath) return;
    invoke("reveal_path", { path: lastExportPath }).catch((e) => showError(String(e)));
  },

  // ---- Backups ----
  backup: () => runProgressOp("Backing up your site", () => invoke("do_backup", { label: null })),

  restore: async () => {
    const path = $("backup-select").value;
    if (!path) return;
    const ok = await confirmModal(
      "Restoring replaces your current site (pages, images, and settings) with this backup. Continue?"
    );
    if (!ok) return;
    return runProgressOp("Restoring your backup", () => invoke("do_restore", { path }));
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
    const ok = await confirmModal(
      "Restoring replaces your current site (pages, images, and settings) with this file. Continue?"
    );
    if (!ok) return;
    return runProgressOp("Restoring your backup", () => invoke("do_restore", { path }));
  },

  // ---- Settings ----
  "refresh-logs": () => refreshLogs(),

  "check-update": async () => {
    const s = $("update-status");
    s.classList.remove("hidden");
    s.textContent = "Checking…";
    try {
      const u = await invoke("get_update");
      if (u.update_available === true) {
        s.textContent = "A new version is available — go to Home to update.";
      } else if (u.update_available === false) {
        s.textContent = "You're on the latest version.";
      } else {
        s.textContent = "Couldn't check right now. Make sure your site is running and you're online.";
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
    const ok = await confirmModal(msg, wipe ? { requireText: "erase" } : {});
    if (!ok) return;
    return runProgressOp(wipe ? "Removing and erasing" : "Removing containers", () =>
      invoke("do_down", { wipe })
    );
  },

  reinstall: async () => {
    const ok = await confirmModal(
      "Reinstalling erases everything — your pages, images, settings, and password — and sets up a fresh Atelier. This cannot be undone.",
      { requireText: "confirm" }
    );
    if (!ok) return;
    return runProgressOp("Reinstalling from scratch", () => invoke("do_reinstall"));
  },
};

// Event delegation for tabs and every [data-action] control.
document.addEventListener("click", (e) => {
  const tab = e.target.closest(".tab");
  if (tab) {
    showTab(tab.dataset.tab);
    return;
  }
  const target = e.target.closest("[data-action]");
  if (!target) return;
  const name = target.getAttribute("data-action");
  const fn = actions[name];
  if (fn) {
    e.preventDefault();
    fn();
  }
});

initPublishPrefs();
refresh().catch((e) => showError(String(e)));
