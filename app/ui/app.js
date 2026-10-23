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

// The chosen download folder, kept here rather than read from a control
// because it is not one: it is picked in a system dialog. saveSettings sends
// the whole object and the engine treats any missing field as its default, so
// leaving this out of a save would reset the folder every time any other
// setting was changed.
let downloadDir = "";

// Browsers taken off the list. Same reason as the folder above: the engine
// treats any field missing from a save as its default, so leaving this out of
// one would un-hide everything whenever any other setting was changed.
let hiddenBrowsers = [];

// What the browser list was last built from, so it is rebuilt only when that
// changes.
let browserSignature = "";

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
  if (!Number.isFinite(seconds) || seconds <= 0) return "\u2014";
  const whole = Math.ceil(seconds);
  if (whole < 60) return `${whole}s`;

  // Each unit is worked out from a total rounded once, at that unit's own
  // precision. Rounding the remainder instead is how "1h 60m" gets written:
  // 3599.6 seconds has 59 whole minutes and a remainder that rounds up to 60.
  const total = whole;
  if (total < 3600) return `${Math.floor(total / 60)}m ${total % 60}s`;

  const minutes = Math.round(total / 60);
  if (minutes < 24 * 60) return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;

  // Past a day, hours are the useful precision: "2d 3h" says how long this is
  // going to take, and minutes on top would be noise at that scale. This is
  // for a long download on a slow link, where "51h 20m" makes the reader do
  // the arithmetic to find out it is more than two days.
  const hours = Math.round(total / 3600);
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
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

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

const SVG_NS = "http://www.w3.org/2000/svg";

// Drawn rather than typed. An eye, a pause bar or a play triangle written as a
// Unicode character is left to whatever font the WebView falls back to, and the
// eye came out as a small white blob. These are the same everywhere, take the
// button's text colour, and are built with DOM calls instead of markup so
// nothing here is ever parsed as HTML.
const ICONS = {
  eye: [
    ["path", { d: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12z" }],
    ["circle", { cx: "12", cy: "12", r: "3" }],
  ],
  pause: [
    ["rect", { x: "6", y: "5", width: "4", height: "14", rx: "1", fill: "currentColor" }],
    ["rect", { x: "14", y: "5", width: "4", height: "14", rx: "1", fill: "currentColor" }],
  ],
  play: [["path", { d: "M8 5v14l11-7z", fill: "currentColor" }]],
};

function icon(name) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "16");
  svg.setAttribute("height", "16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  for (const [tag, attributes] of ICONS[name]) {
    const part = document.createElementNS(SVG_NS, tag);
    for (const [key, value] of Object.entries(attributes)) part.setAttribute(key, value);
    svg.append(part);
  }
  return svg;
}

function render(snapshot) {
  latest = snapshot;
  renderPairing(snapshot.pending ?? []);
  renderJobs(snapshot.jobs ?? []);
  renderSettings(snapshot.settings);
  // Only when something that changes the list has changed. The snapshot
  // arrives twice a second, and the list is built from the registry as well
  // as from this, so rebuilding it on every tick would be needless work and
  // would close a Manage panel the user was in the middle of reading.
  const signature = JSON.stringify([snapshot.paired, snapshot.settings?.hidden_browsers]);
  if (signature !== browserSignature) {
    browserSignature = signature;
    refreshBrowsers();
  }

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
      // The state, and the one control that belongs with it: pause while
      // it runs, play while it does not. Beside the word rather than down
      // in the row of buttons, because it is the answer to the word.
      const state = document.createElement("div");
      state.className = `job-state ${job.state}`;
      state.textContent =
        job.state === "reconnecting"
          ? typeof job.retry_in_secs === "number"
            ? `reconnecting in ${job.retry_in_secs}s`
            : "reconnecting"
          : job.state;

      const going = job.state === "running" || job.state === "queued";
      const toggle = document.createElement("button");
      toggle.className = "icon";
      // Symbols rather than words: they sit inline with the status and mean
      // the same thing in any language.
      toggle.append(icon(going ? "pause" : "play"));
      toggle.title = going ? "Pause" : "Resume";
      toggle.setAttribute("aria-label", toggle.title);
      toggle.onclick = () => act(going ? "pause_job" : "resume_job", { id: job.id });

      const controls = document.createElement("div");
      controls.className = "job-controls";
      if (job.state !== "completed") controls.append(toggle);
      controls.append(state);
      top.append(name, controls);

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
      // Said plainly: this one does not keep the bytes, and the pause button
      // beside the status is the one that does.
      cancel.title = "Stop and delete what has been downloaded";
      cancel.onclick = () => act("cancel_job", { id: job.id });

      // For a link that expired while the download sat waiting. The bytes are
      // still good; only the way back to the rest of them is stale, so this
      // changes the address and keeps the file.
      const refresh = document.createElement("button");
      refresh.className = "ghost";
      refresh.textContent = "New link";
      refresh.title = "Paste a fresh address and carry on from here";
      refresh.onclick = async () => {
        const url = window.prompt(`A fresh address for ${job.filename}:`, job.url);
        if (url && url.trim()) await act("refresh_link", { id: job.id, url: url.trim() });
      };

      foot.append(size, speed, conns, eta, spacer);
      if (!going) foot.append(refresh);
      foot.append(cancel);

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

function showFolder() {
  $("folder-path").textContent = downloadDir || "Your Downloads folder (default)";
  $("reset-folder").hidden = !downloadDir;
}

function renderSettings(settings) {
  // Always, even mid-interaction: this is the value the next save will
  // carry, and a stale one would revert the folder.
  downloadDir = settings?.download_dir ?? downloadDir;
  hiddenBrowsers = settings?.hidden_browsers ?? hiddenBrowsers;
  if (!settings || document.activeElement?.closest("#panel-settings")) {
    // Not while the user is in the middle of changing something: a tick
    // landing mid-interaction would reset the control under their cursor.
    return;
  }
  $("integration").checked = settings.browser_integration;
  $("min-size").value = String(settings.min_takeover_bytes);
  $("max-concurrent").value = String(settings.max_concurrent_downloads);
  $("speed-limit").value = String(settings.speed_limit_bytes_per_sec ?? 0);
  $("sort-into-folders").checked = settings.sort_into_folders ?? true;
  $("start-with-windows").checked = settings.start_with_windows;
  $("resume-on-start").checked = settings.resume_on_start;
  $("post-action").value = settings.post_action;
  $("countdown").value = String(settings.post_action_countdown_secs);
  showFolder();
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

async function saveSettings(overrides = {}) {
  const settings = {
    download_dir: downloadDir,
    hidden_browsers: hiddenBrowsers,
    browser_integration: $("integration").checked,
    min_takeover_bytes: Number($("min-size").value),
    max_concurrent_downloads: Number($("max-concurrent").value),
    speed_limit_bytes_per_sec: Number($("speed-limit").value),
    sort_into_folders: $("sort-into-folders").checked,
    start_with_windows: $("start-with-windows").checked,
    resume_on_start: $("resume-on-start").checked,
    post_action: $("post-action").value,
    post_action_countdown_secs: Number($("countdown").value),
    ...overrides,
  };
  try {
    await invoke("save_settings", { settings });
    // Only once the engine has accepted it. A folder it refused must not
    // be shown as the current one.
    downloadDir = settings.download_dir;
    hiddenBrowsers = settings.hidden_browsers;
    showFolder();
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
        open.className = "icon";
        open.append(icon("eye"));
        open.title = "Show in folder";
        open.setAttribute("aria-label", "Show in folder");
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
    // A browser may have been installed or paired since this was last looked at.
    if (tab.dataset.tab === "settings") refreshBrowsers();
  });
}

for (const id of [
  "integration",
  "min-size",
  "max-concurrent",
  "speed-limit",
  "sort-into-folders",
  "start-with-windows",
  "resume-on-start",
  "post-action",
  "countdown",
]) {
  // A fresh call, not the listener itself: addEventListener would pass the
  // Event as `overrides`, whose properties would then be spread into the
  // settings.
  $(id).addEventListener("change", () => saveSettings());
}

$("choose-folder").addEventListener("click", async () => {
  const picked = await invoke("choose_folder", { start: downloadDir || null }).catch(() => null);
  // Cancelling is not an error, and changes nothing.
  if (picked) await saveSettings({ download_dir: picked });
});

$("reset-folder").addEventListener("click", () => saveSettings({ download_dir: "" }));

$("clear-history").addEventListener("click", async () => {
  await act("clear_history");
  showHistory();
});

// The engine pushes; the window does not poll. A pull loop would keep the
// renderer busy with nothing to do and put the update rate in the hands of a
// JavaScript timer rather than the side that knows when something changed.
listen("snapshot", (event) => render(event.payload));
window.addEventListener("resize", drawGraph);

// ---------------------------------------------------------------------------
// Browser integration
// ---------------------------------------------------------------------------

// Where the bundled extension lives, or null when it cannot be found. Resolved
// by the app from where it is running, never a fixed path.
let extensionSetup = null;
let browserRows = [];
// Rows whose Manage panel is open, by key, so a rebuild does not close them.
const openPanels = new Set();

async function refreshBrowsers() {
  try {
    browserRows = await invoke("browsers");
  } catch (error) {
    browserRows = [];
    toast(String(error), true);
  }
  renderBrowsers();
}

const STATUS = {
  connected: "Connected",
  off: "Off",
  not_connected: "Not connected",
};

/** An On/Off switch. The word is part of the control, not a separate label. */
function toggleSwitch(checked, label, onChange) {
  const wrap = document.createElement("label");
  wrap.className = "toggle";
  wrap.title = label;

  const input = document.createElement("input");
  input.type = "checkbox";
  input.setAttribute("role", "switch");
  input.setAttribute("aria-label", label);
  input.checked = checked;

  const track = document.createElement("span");
  track.className = "track";
  const state = document.createElement("span");
  state.className = "state";
  state.textContent = checked ? "On" : "Off";

  input.addEventListener("change", () => {
    // The word follows the switch at once; the engine's answer arrives with
    // the next snapshot and corrects it if the change was refused.
    state.textContent = input.checked ? "On" : "Off";
    onChange(input.checked);
  });

  wrap.append(input, track, state);
  return wrap;
}

function folderFor(row) {
  if (!extensionSetup) return null;
  return row.family === "gecko" ? extensionSetup.firefoxManifest : extensionSetup.chromiumDir;
}

function browserRow(row) {
  const wrap = document.createElement("div");
  wrap.className = "browser";

  const line = document.createElement("div");
  line.className = "browser-row";

  const dot = document.createElement("span");
  dot.className = row.state === "connected" ? "dot good" : "dot";
  dot.setAttribute("aria-hidden", "true");

  const name = document.createElement("span");
  name.className = "browser-name";
  name.textContent = row.name;

  const status = document.createElement("span");
  status.className = `browser-status ${row.state}`;
  status.textContent = STATUS[row.state] ?? row.state;

  const spacer = document.createElement("span");
  spacer.className = "spacer";

  line.append(dot, name, status, spacer);

  const paired = row.client_ids.length > 0;
  if (paired) {
    line.append(
      toggleSwitch(row.state === "connected", `Take over downloads from ${row.name}`, (enabled) =>
        act("set_browser_enabled", { clientIds: row.client_ids, enabled }),
      ),
    );

    const manage = document.createElement("button");
    manage.className = "ghost";
    manage.textContent = "Manage";
    manage.setAttribute("aria-expanded", String(openPanels.has(row.key)));
    manage.onclick = () => {
      if (openPanels.has(row.key)) openPanels.delete(row.key);
      else openPanels.add(row.key);
      renderBrowsers();
    };
    line.append(manage);
  } else {
    const install = document.createElement("button");
    install.textContent = "Install";
    install.onclick = () => showInstructions(true);
    line.append(install);
  }

  // Taking it off the list. Always there, because a browser the user never
  // means to connect is exactly the one with nothing else on its row to click.
  const hide = document.createElement("button");
  hide.className = "icon quiet";
  hide.textContent = "\u2715";
  hide.title = `Hide ${row.name} from this list`;
  hide.setAttribute("aria-label", hide.title);
  hide.onclick = () => {
    saveSettings({ hidden_browsers: [...hiddenBrowsers, row.name] });
  };
  line.append(hide);

  wrap.append(line);

  if (paired && openPanels.has(row.key)) {
    const panel = document.createElement("div");
    panel.className = "browser-panel";

    const note = document.createElement("p");
    note.className = "note";
    note.textContent =
      row.state === "off"
        ? `${row.name} is paired but switched off, so its downloads stay with the browser. Switch it on above and it carries on without being approved again.`
        : `${row.name} is paired. Switch it off above and its downloads stay with the browser; it stays paired, so switching it back on needs no approval.`;
    panel.append(note);

    const folder = folderFor(row);
    if (folder) {
      const where = document.createElement("div");
      where.className = "row";
      const path = document.createElement("code");
      path.className = "path";
      path.textContent = folder;
      const open = document.createElement("button");
      open.className = "ghost";
      open.textContent = "Open folder";
      open.onclick = () => invoke("reveal", { path: folder }).catch((e) => toast(String(e), true));
      where.append(path, open);
      panel.append(where);
    }
    wrap.append(panel);
  }
  return wrap;
}

function renderBrowsers() {
  const list = $("browser-list");
  const visible = browserRows.filter((row) => !row.hidden);
  list.replaceChildren(
    ...(visible.length
      ? visible.map(browserRow)
      : [empty("No browsers to show. Use the instructions below to add one.")]),
  );

  // Hidden ones, each one click from coming back.
  const hidden = browserRows.filter((row) => row.hidden);
  const box = $("browser-hidden");
  box.hidden = hidden.length === 0;
  box.replaceChildren();
  if (hidden.length) {
    const label = document.createElement("span");
    label.className = "browser-hidden-label";
    label.textContent = "Hidden:";
    box.append(label);
    for (const row of hidden) {
      const chip = document.createElement("button");
      chip.className = "chip";
      chip.textContent = `${row.name} \u21BA`;
      chip.title = `Show ${row.name} in the list again`;
      chip.onclick = () => {
        saveSettings({
          hidden_browsers: hiddenBrowsers.filter(
            (name) => name.trim().toLowerCase() !== row.name.trim().toLowerCase(),
          ),
        });
      };
      box.append(chip);
    }
  }
}

function showInstructions(show) {
  const panel = $("instructions");
  const button = $("toggle-instructions");
  panel.hidden = !show;
  button.setAttribute("aria-expanded", String(show));
  button.textContent = show ? "Hide Installation Instructions" : "Show Installation Instructions";
  if (show) panel.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

function renderInstructionPaths() {
  const box = $("instruction-paths");
  box.replaceChildren();
  if (!extensionSetup) {
    box.append(
      empty("The extension files were not found next to the app, so there is no folder to show."),
    );
    return;
  }
  const entries = [
    ["Chrome, Edge, Brave and other Chromium browsers", extensionSetup.chromiumDir],
    ["Firefox", extensionSetup.firefoxManifest],
  ];
  for (const [label, path] of entries) {
    if (!path) continue;
    const caption = document.createElement("div");
    caption.className = "path-caption";
    caption.textContent = label;
    const row = document.createElement("div");
    row.className = "row";
    const code = document.createElement("code");
    code.className = "path";
    code.textContent = path;
    const open = document.createElement("button");
    open.className = "ghost";
    open.textContent = "Open folder";
    open.onclick = () => invoke("reveal", { path }).catch((e) => toast(String(e), true));
    row.append(code, open);
    box.append(caption, row);
  }
}

$("toggle-instructions").addEventListener("click", () =>
  showInstructions($("instructions").hidden),
);

// Files the shell opened with this app -- a double-clicked .torrent.
//
// Collected rather than just announced: a double-click can start the app from
// cold, so the file is known before there is a window to tell. The window asks
// when it loads, and again whenever the app says there are more.
async function collectOpenedFiles() {
  let files = [];
  try {
    files = await invoke("opened_files");
  } catch {
    return;
  }
  if (!files.length) return;

  const box = $("notices");
  for (const path of files) {
    const name = path.split(/[\\/]/).pop();

    const note = document.createElement("div");
    note.className = "notice";

    const text = document.createElement("p");
    // textContent: a file name is chosen by whoever made the file.
    const strong = document.createElement("strong");
    strong.textContent = name;
    text.append(
      strong,
      " was opened with Divine IDM, but nothing was started. Divine IDM downloads over HTTP, " +
        "FTP and SFTP; BitTorrent is not supported yet, so it cannot download this.",
    );

    const dismiss = document.createElement("button");
    dismiss.className = "ghost";
    dismiss.textContent = "Dismiss";
    dismiss.onclick = () => {
      note.remove();
      box.hidden = box.childElementCount === 0;
    };

    note.append(text, dismiss);
    box.append(note);
  }
  box.hidden = false;
}

listen("opened-file", () => collectOpenedFiles());
collectOpenedFiles();

// Asked once: the folder is part of the installation and cannot move while the
// app is running.
(async () => {
  extensionSetup = await invoke("browser_setup").catch(() => null);
  renderInstructionPaths();
  // The rows offer an Open folder button only once this is known.
  renderBrowsers();
})();

refresh();
