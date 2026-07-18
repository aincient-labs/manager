// Atelier Manager — frontend. All real work lives in the Rust core (aincient-core),
// reached through Tauri commands. This file only orchestrates screens and input.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const screens = ["loading", "docker", "install", "main"];

function showScreen(name) {
  for (const s of screens) $(`screen-${s}`).classList.toggle("hidden", s !== name);
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
  pull: "Downloading image",
  starting: "Starting containers",
  booting: "Booting the console",
  ready: "Ready",
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

// Apply one op-progress event: advance the bar, update the sub-status, and feed
// the log. A numeric fraction switches the bar to determinate (and mint at 1.0);
// repeated ticks update the live status without spamming the feed; each new phase
// and every passed-through docker line get a feed line.
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
  // A stage milestone.
  $("progress-stage").textContent = p.message; // live, e.g. "Booting… (12s)"
  if (p.stage !== lastStage) {
    appendLog(`▸ ${STAGE_LABELS[p.stage] || p.message}`);
    lastStage = p.stage;
  }
}

// Wrap any long op in the progress panel: stream its phases/steps via op-progress
// events, then refresh. Phased ops (install/update/start) drive the bar with
// fractions; the rest run an indeterminate bar that settles green on success.
async function runProgressOp(title, fn) {
  progressReset(title);
  $("progress").classList.remove("hidden");
  const unlisten = await listen("op-progress", (e) => progressUpdate(e.payload));
  try {
    await fn();
    if (!sawFraction) progressFinish();
    await refresh();
  } catch (e) {
    showError(String(e));
  } finally {
    unlisten();
    $("progress").classList.add("hidden");
  }
}

async function refresh() {
  const problem = await invoke("preflight_problem");
  if (problem) {
    $("docker-msg").textContent = problem;
    showScreen("docker");
    return;
  }

  const status = await invoke("get_status");
  if (!status.installed) {
    showScreen("install");
    return;
  }

  renderStatus(status);
  showScreen("main");
  // Best-effort, non-blocking enrichment.
  refreshUpdate();
  refreshBackups();
}

function renderStatus(status) {
  const dot = $("status-dot");
  const text = $("status-text");
  if (status.running) {
    dot.className = "dot up";
    text.textContent = status.reachable ? "Running" : "Running (starting…)";
  } else {
    dot.className = "dot down";
    text.textContent = "Stopped";
  }
  const url = status.console_url;
  $("console-url").textContent = url;
  $("console-url").href = url;
  $("startstop-label").textContent = status.running ? "Stop" : "Start";
}

async function refreshUpdate() {
  try {
    const u = await invoke("get_update");
    const banner = $("update-banner");
    if (u.update_available === true) {
      $("update-text").textContent = "An update is available.";
      banner.classList.remove("hidden");
    } else {
      banner.classList.add("hidden");
    }
  } catch {
    $("update-banner").classList.add("hidden");
  }
}

async function refreshBackups() {
  const select = $("backup-select");
  const restoreBtn = document.querySelector('[data-action="restore"]');
  const exportBtn = $("btn-export");
  const setEnabled = (on) => {
    restoreBtn.disabled = !on;
    exportBtn.disabled = !on;
  };
  try {
    const backups = await invoke("list_backups");
    select.innerHTML = "";
    if (!backups.length) {
      select.innerHTML = '<option value="">No backups yet</option>';
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

// --- actions ----------------------------------------------------------------

const actions = {
  recheck: () => refresh(),

  "dismiss-error": () => $("error").classList.add("hidden"),

  open: () => invoke("open_console").catch((e) => showError(String(e))),

  // Send the operator straight to Drupal's /user/login form in their browser.
  // The manager never displays the admin password; if they've forgotten it,
  // "Reset password" below sets a new one.
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
    return runProgressOp("Installing Atelier", () =>
      invoke("do_install", { image: null, port })
    );
  },

  update: () => runProgressOp("Updating", () => invoke("do_update")),

  startstop: async () => {
    const running = $("startstop-label").textContent === "Stop";
    return runProgressOp(running ? "Stopping" : "Starting", () =>
      invoke(running ? "do_stop" : "do_start")
    );
  },

  backup: () => runProgressOp("Backing up", () => invoke("do_backup", { label: null })),

  restore: async () => {
    const path = $("backup-select").value;
    if (!path) return;
    const ok = await confirmModal(
      "Restore will REPLACE the current database and files with this snapshot. Continue?"
    );
    if (!ok) return;
    return runProgressOp("Restoring", () => invoke("do_restore", { path }));
  },

  // Export the selected backup out of ~/.atelier/backups to a user-chosen
  // location (native Save As), for archiving off-machine.
  export: async () => {
    const source = $("backup-select").value;
    if (!source) return;
    try {
      await invoke("export_backup", { source });
    } catch (e) {
      showError(String(e));
    }
  },

  // Import: pick an external snapshot (native Open File) and restore it through
  // the same confirm + core path as a listed backup.
  import: async () => {
    let path;
    try {
      path = await invoke("pick_restore_file");
    } catch (e) {
      return showError(String(e));
    }
    if (!path) return; // cancelled
    const ok = await confirmModal(
      "Restore will REPLACE the current database and files with this snapshot. Continue?"
    );
    if (!ok) return;
    return runProgressOp("Restoring", () => invoke("do_restore", { path }));
  },

  reinstall: async () => {
    const ok = await confirmModal(
      "Reinstall DELETES all data (database, files, admin password) and installs fresh. " +
        "This cannot be undone.",
      { requireText: "confirm" }
    );
    if (!ok) return;
    return runProgressOp("Reinstalling from scratch", () => invoke("do_reinstall"));
  },
};

// Event delegation for every [data-action] control.
document.addEventListener("click", (e) => {
  const target = e.target.closest("[data-action]");
  if (!target) return;
  const name = target.getAttribute("data-action");
  const fn = actions[name];
  if (fn) {
    e.preventDefault();
    fn();
  }
});

refresh().catch((e) => showError(String(e)));
