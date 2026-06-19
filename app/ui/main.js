// AIncient Manager — frontend. All real work lives in the Rust core (aincient-core),
// reached through Tauri commands. This file only orchestrates screens and input.

const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const screens = ["loading", "docker", "install", "main"];

function showScreen(name) {
  for (const s of screens) $(`screen-${s}`).classList.toggle("hidden", s !== name);
}

function showError(msg) {
  $("error-text").textContent = msg;
  $("error").classList.remove("hidden");
}

function busy(on, msg) {
  $("busy-msg").textContent = msg || "Working…";
  $("busy").classList.toggle("hidden", !on);
}

// In-page confirm (the webview blocks native confirm() without the dialog plugin).
function confirmModal(msg) {
  return new Promise((resolve) => {
    $("confirm-msg").textContent = msg;
    $("confirm").classList.remove("hidden");
    const done = (val) => {
      $("confirm").classList.add("hidden");
      $("confirm-yes").onclick = null;
      $("confirm-no").onclick = null;
      resolve(val);
    };
    $("confirm-yes").onclick = () => done(true);
    $("confirm-no").onclick = () => done(false);
  });
}

// Wrap a long op: show the busy overlay, run, surface errors, then refresh.
async function runOp(msg, fn) {
  busy(true, msg);
  try {
    await fn();
    await refresh();
  } catch (e) {
    showError(String(e));
  } finally {
    busy(false);
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
  refreshLogin(status);
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
  $("btn-startstop").textContent = status.running ? "Stop" : "Start";
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

async function refreshLogin(status) {
  const line = $("login-line");
  if (!status.running) {
    line.classList.add("hidden");
    return;
  }
  try {
    const pw = await invoke("admin_password");
    if (pw) {
      line.textContent = `admin / ${pw}`;
      line.classList.remove("hidden");
    } else {
      line.classList.add("hidden");
    }
  } catch {
    line.classList.add("hidden");
  }
}

async function refreshBackups() {
  const select = $("backup-select");
  const restoreBtn = document.querySelector('[data-action="restore"]');
  try {
    const backups = await invoke("list_backups");
    select.innerHTML = "";
    if (!backups.length) {
      select.innerHTML = '<option value="">No backups yet</option>';
      restoreBtn.disabled = true;
      return;
    }
    for (const b of backups) {
      const opt = document.createElement("option");
      opt.value = b.path;
      const mb = (b.size_bytes / 1048576).toFixed(1);
      opt.textContent = `${b.name}  (${mb} MB)`;
      select.appendChild(opt);
    }
    restoreBtn.disabled = false;
  } catch {
    restoreBtn.disabled = true;
  }
}

// --- actions ----------------------------------------------------------------

const actions = {
  recheck: () => refresh(),

  "dismiss-error": () => $("error").classList.add("hidden"),

  open: () => invoke("open_console").catch((e) => showError(String(e))),

  install: () => {
    const key = $("install-key").value.trim() || null;
    const port = parseInt($("install-port").value, 10) || null;
    return runOp("Installing — this can take a couple of minutes…", () =>
      invoke("do_install", { key, image: null, port })
    );
  },

  update: () =>
    runOp("Updating — snapshotting, migrating, health-checking…", () => invoke("do_update")),

  startstop: async () => {
    const running = $("btn-startstop").textContent === "Stop";
    return runOp(running ? "Stopping…" : "Starting…", () =>
      invoke(running ? "do_stop" : "do_start")
    );
  },

  backup: () => runOp("Backing up the database…", () => invoke("do_backup", { label: null })),

  restore: async () => {
    const path = $("backup-select").value;
    if (!path) return;
    const ok = await confirmModal(
      "Restore will REPLACE the current database with this backup. Continue?"
    );
    if (!ok) return;
    return runOp("Restoring the database…", () => invoke("do_restore", { path }));
  },

  reinstall: async () => {
    const ok = await confirmModal(
      "Reinstall DELETES all data (database, files, admin password) and installs fresh. " +
        "This cannot be undone. Continue?"
    );
    if (!ok) return;
    return runOp("Reinstalling from scratch…", () => invoke("do_reinstall", { key: null }));
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
