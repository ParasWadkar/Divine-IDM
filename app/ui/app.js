// The window. Everything here is display and intent; every decision that
// matters is the engine's, reached through the same request surface the CLI
// and the browser extension use.
//
// No framework and no build step. The whole project has no npm dependency,
// which for a download manager is a supply chain worth not having, and this
// is a list, a graph and a form — well inside what plain DOM handles.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

/** Last known snapshot, so a click can act without waiting for the next tick. */
let latest = { jobs: [], settings: null, paired: [], pending: [] };

/** Rolling throughput samples for the graph. */
const SAMPLES = 120;
const history = [];

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

function bytes(value) {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const power = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  const scaled = value / 1024 ** power;
  // One decimal below 100 keeps the number legible without it jittering by a
  // digit every tick.
  return `${scaled >= 100 || power === 0 ? Math.round(scaled) : scaled.toFixed(1)} ${units[power]}`;
}

const rate = (value) => `${bytes(value)}/s`;

function duration(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return "—";
  if (seconds < 60) return `${Math.ceil(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.round((seconds % 3600) / 60)}m`;
}

function when(ms) {
  if (!ms) return "";
  const date = new Date(Number(ms));
  const today = new Date();
  const sameDay = date.toDateString() === today.toDateString();
  return sameDay
    ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString([], { month: "short", day: "numeric" });
}

function toast(message, bad = false) {
  const element = $("toast");
  element.textContent = message;
  element.classList.toggle("bad", bad);
  element.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (element.hidden = true), 4200);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function render(snapshot) {
  latest = snapshot;
  renderPairing(snapshot.pending ?? []);
  renderJobs(snapshot.jobs ?? []);
  renderSettings(snapshot.settings);
  renderPaired(snapshot.paired ?? []);

  const running = (snapshot.jobs ?? []).filter((job) => job.state === "running");
  const total = running.reduce((sum, job) => sum + (job.bytes_per_sec ?? 0), 0);
  const connections = running.reduce((sum, job) => sum + (job.connections ?? 0), 0);

  $("speed-now").textContent = rate(total);
  $("speed-conns").textContent = String(connections);

  history.push(total);
  while (history.length > SAMPLES) history.shift();
  const peak = Math.max(...history, 0);
  $("speed-peak").textContent = peak > 0 ? `peak ${rate(peak)}` : "";
  drawGraph();

  $("engine-dot").className = "dot good";
  $("engine-text").textContent = running.length
    ? `${running.length} downloading`
    : "idle";
}

function renderPairing(pending) {
  const box = $("pairing");
  box.hidden = pending.length === 0;
  if (!pending.length) return;

  box.replaceChildren(
    ...pending.map((request) => {
      const row = document.createElement("div");
      row.className = "pair-row";

      const text = document.createElement("p");
      // textContent, never innerHTML: the label is chosen by the extension,
      // which is exactly the input that should not be able to write markup
      // into the window that approves it.
      text.append("A browser is asking to send downloads here: ");
      const name = document.createElement("strong");
      name.textContent = request.label;
      text.append(name);

      const approve = document.createElement("button");
      approve.textContent = "Approve";
      approve.onclick = () => act("approve_pairing", { requestId: request.request_id });

      const deny = document.createElement("button");
      deny.className = "ghost";
      deny.textContent = "Deny";
      deny.onclick = () => act("deny_pairing", { requestId: request.request_id });

      row.append(text, approve, deny);
      return row;
    }),
  );
}

function renderJobs(jobs) {
  const list = $("jobs");
  const live = jobs.filter((job) => job.state !== "completed");

  if (!live.length) {
    list.replaceChildren(empty("Nothing downloading. Paste a URL above, or start one in your browser."));
    return;
  }

  list.replaceChildren(
    ...live.map((job) => {
      const card = document.createElement("div");
      card.className = "job";

      const top = document.createElement("div");
      top.className = "job-top";
      const name = document.createElement("div");
      name.className = "job-name";
      name.textContent = job.filename;
      name.title = job.url;
      const state = document.createElement("div");
      state.className = `job-state ${job.state}`;
      state.textContent = job.state;
      top.append(name, state);

      // An unknown total means the server never told us; a moving
      // indeterminate bar is honest where a percentage would be invented.
      const known = typeof job.total === "number" && job.total > 0;
      const fraction = known ? Math.min(job.downloaded / job.total, 1) : 0;
      const bar = document.createElement("div");
      bar.className = known ? "bar" : "bar indeterminate";
      const fill = document.createElement("span");
      fill.style.width = `${(fraction * 100).toFixed(1)}%`;
      bar.append(fill);

      const foot = document.createElement("div");
      foot.className = "job-foot";
      const size = document.createElement("span");
      size.textContent = known
        ? `${bytes(job.downloaded)} of ${bytes(job.total)}`
        : bytes(job.downloaded);
      const speed = document.createElement("span");
      speed.textContent = job.state === "running" ? rate(job.bytes_per_sec) : "";
      const conns = document.createElement("span");
      conns.textContent =
        job.state === "running" && job.connections ? `${job.connections} connections` : "";
      const eta = document.createElement("span");
      if (known && job.state === "running" && job.bytes_per_sec > 0) {
        eta.textContent = `${duration((job.total - job.downloaded) / job.bytes_per_sec)} left`;
      }

      const spacer = document.createElement("span");
      spacer.className = "spacer";

      const cancel = document.createElement("button");
      cancel.className = "ghost";
      cancel.textContent = "Cancel";
      cancel.onclick = () => act("cancel_job", { id: job.id });

      foot.append(size, speed, conns, eta, spacer);
      if (job.state === "running" || job.state === "queued") foot.append(cancel);

      card.append(top, bar, foot);
      if (job.error) {
        const error = document.createElement("div");
        error.className = "job-error";
        error.textContent = job.error;
        card.append(error);
      }
      return card;
    }),
  );
}

function renderSettings(settings) {
  if (!settings || document.activeElement?.closest("#panel-settings")) {
    // Not while the user is in the middle of changing something: a tick
    // landing mid-interaction would reset the control under their cursor.
    return;
  }
  $("integration").checked = settings.browser_integration;
  $("min-size").value = String(settings.min_takeover_bytes);
  $("post-action").value = settings.post_action;
  $("countdown").value = String(settings.post_action_countdown_secs);
}

function renderPaired(paired) {
  const box = $("paired");
  if (!paired.length) {
    box.replaceChildren(empty("No browser extensions paired yet."));
    return;
  }
  box.replaceChildren(
    ...paired.map((client) => {
      const row = document.createElement("div");
      row.className = "hrow";
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = client.label;
      const revoke = document.createElement("button");
      revoke.className = "ghost";
      revoke.textContent = "Revoke";
      revoke.onclick = () => act("revoke_pairing", { clientId: client.client_id });
      row.append(name, revoke);
      return row;
    }),
  );
}

function empty(message) {
  const element = document.createElement("div");
  element.className = "empty";
  element.textContent = message;
  return element;
}

// ---------------------------------------------------------------------------
// Speed graph
// ---------------------------------------------------------------------------

function drawGraph() {
  const canvas = $("graph");
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (!width) return;

  // Re-sizing the backing store clears it, so only do it when it changed.
  if (canvas.width !== width * ratio || canvas.height !== height * ratio) {
    canvas.width = width * ratio;
    canvas.height = height * ratio;
  }

  const context = canvas.getContext("2d");
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);

  // Scale to the window's own peak rather than an absolute ceiling: a 2 MB/s
  // line on a 1 Gb axis is a flat line that tells you nothing.
  const peak = Math.max(...history, 1);
  const step = width / (SAMPLES - 1);
  const y = (value) => height - 2 - (value / peak) * (height - 8);
  // Newest sample at the right edge, so a half-filled window reads as a
  // trace arriving rather than as a graph that stops in the middle.
  const left = width - (history.length - 1) * step;

  const line = new Path2D();
  const fill = new Path2D();
  fill.moveTo(left, height);
  history.forEach((value, index) => {
    const x = left + index * step;
    if (index === 0) line.moveTo(x, y(value));
    else line.lineTo(x, y(value));
    fill.lineTo(x, y(value));
  });
  fill.lineTo(width, height);
  fill.closePath();

  const gradient = context.createLinearGradient(0, 0, 0, height);
  gradient.addColorStop(0, "rgba(120, 190, 255, 0.35)");
  gradient.addColorStop(1, "rgba(120, 190, 255, 0)");
  context.fillStyle = gradient;
  context.fill(fill);

  context.strokeStyle = "#78beff";
  context.lineWidth = 1.5;
  context.lineJoin = "round";
  context.stroke(line);
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function act(command, args = {}) {
  try {
    await invoke(command, args);
    refresh();
  } catch (error) {
    toast(String(error), true);
  }
}

async function refresh() {
  try {
    render(await invoke("snapshot"));
  } catch (error) {
    $("engine-dot").className = "dot bad";
    $("engine-text").textContent = "engine unavailable";
    toast(String(error), true);
  }
}

async function saveSettings() {
  const settings = {
    browser_integration: $("integration").checked,
    min_takeover_bytes: Number($("min-size").value),
    post_action: $("post-action").value,
    post_action_countdown_secs: Number($("countdown").value),
  };
  try {
    await invoke("save_settings", { settings });
    toast("Settings saved");
  } catch (error) {
    toast(String(error), true);
  }
}

async function showHistory() {
  try {
    const rows = await invoke("history", { limit: 200 });
    const box = $("history");
    if (!rows.length) {
      box.replaceChildren(empty("Nothing has finished yet."));
      return;
    }
    box.replaceChildren(
      ...rows.map((job) => {
        const row = document.createElement("div");
        row.className = `hrow ${job.state}`;

        const name = document.createElement("div");
        name.className = "name";
        name.textContent = job.filename;
        name.title = job.url;

        const meta = document.createElement("div");
        meta.className = "meta";
        meta.textContent = `${bytes(job.downloaded)} · ${when(job.finished_at_ms)}`;

        const open = document.createElement("button");
        open.className = "ghost";
        open.textContent = "Show";
        open.onclick = () =>
          invoke("reveal", { path: `${job.output_dir}\\${job.filename}` }).catch((error) =>
            toast(String(error), true),
          );

        row.append(name, meta);
        if (job.state === "completed") row.append(open);
        return row;
      }),
    );
  } catch (error) {
    toast(String(error), true);
  }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

$("add").addEventListener("submit", async (event) => {
  event.preventDefault();
  const field = $("url");
  const url = field.value.trim();
  if (!url) return;
  try {
    await invoke("add_download", { url, filename: null });
    field.value = "";
    refresh();
  } catch (error) {
    toast(String(error), true);
  }
});

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    for (const other of document.querySelectorAll(".tab")) {
      other.classList.toggle("is-active", other === tab);
    }
    for (const name of ["active", "history", "settings"]) {
      $(`panel-${name}`).hidden = name !== tab.dataset.tab;
    }
    if (tab.dataset.tab === "history") showHistory();
  });
}

for (const id of ["integration", "min-size", "post-action", "countdown"]) {
  $(id).addEventListener("change", saveSettings);
}

$("clear-history").addEventListener("click", async () => {
  await act("clear_history");
  showHistory();
});

// The engine pushes; the window does not poll. A pull loop would keep the
// renderer busy with nothing to do and put the update rate in the hands of a
// JavaScript timer rather than the side that knows when something changed.
listen("snapshot", (event) => render(event.payload));
window.addEventListener("resize", drawGraph);

refresh();
