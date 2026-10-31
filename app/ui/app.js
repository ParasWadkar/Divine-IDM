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

// Display speed in bits/s (Mbps) instead of bytes/s (MB/s)
let showSpeedInBits = false;

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

function bits(value) {
  if (!Number.isFinite(value) || value <= 0) return "0 bps";
  const bitValue = value * 8;
  const units = ["bps", "Kbps", "Mbps", "Gbps", "Tbps"];
  const power = Math.min(Math.floor(Math.log(bitValue) / Math.log(1000)), units.length - 1);
  const scaled = bitValue / 1000 ** power;
  return `${scaled >= 100 || power === 0 ? Math.round(scaled) : scaled.toFixed(1)} ${units[power]}`;
}

const rate = (value) => showSpeedInBits ? `${bits(value)}/s` : `${bytes(value)}/s`;

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
  // A problem interrupts a screen reader's queue; a confirmation waits its
  // turn. Both are still spoken without anyone having to be looking at it.
  element.setAttribute("aria-live", bad ? "assertive" : "polite");
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
  pause: [
    ["rect", { x: "6", y: "5", width: "4", height: "14", rx: "1", fill: "currentColor" }],
    ["rect", { x: "14", y: "5", width: "4", height: "14", rx: "1", fill: "currentColor" }],
  ],
  play: [["path", { d: "M8 5v14l11-7z", fill: "currentColor" }]],
  folder: [
    [
      "path",
      {
        d: "M3 6.5A1.5 1.5 0 0 1 4.5 5h4.3a1.5 1.5 0 0 1 1.1.48L11 7h8.5A1.5 1.5 0 0 1 21 8.5v10a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 18.5z",
      },
    ],
  ],
  trash: [
    ["path", { d: "M4 7h16" }],
    ["path", { d: "M9 7V4.5A1.5 1.5 0 0 1 10.5 3h3A1.5 1.5 0 0 1 15 4.5V7" }],
    ["path", { d: "M18.5 7 17.6 19.2a2 2 0 0 1-2 1.8H8.4a2 2 0 0 1-2-1.8L5.5 7" }],
    ["path", { d: "M10 11v6" }],
    ["path", { d: "M14 11v6" }],
  ],
  close: [
    ["path", { d: "M6 6l12 12" }],
    ["path", { d: "M18 6 6 18" }],
  ],
  check: [["path", { d: "m5 12.5 4.5 4.5L19 7.5" }]],
  clock: [
    ["circle", { cx: "12", cy: "12", r: "8.5" }],
    ["path", { d: "M12 7v5l3.5 2" }],
  ],
  link: [
    ["path", { d: "M10 13a5 5 0 0 0 7.1.1l3-3a5 5 0 0 0-7.1-7.1L11.3 4.7" }],
    ["path", { d: "M14 11a5 5 0 0 0-7.1-.1l-3 3a5 5 0 0 0 7.1 7.1l1.7-1.7" }],
  ],
  speed: [
    ["path", { d: "M12 4v11" }],
    ["path", { d: "m7 11 5 5 5-5" }],
    ["path", { d: "M5 20h14" }],
  ],
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

/**
 * A button with an icon and its own visible label, never the icon alone:
 * the same symbol can be read differently by different people, and a title
 * tooltip is invisible until a mouse happens to sit still over it.
 */
function iconButton(iconName, label, { quiet = false } = {}) {
  const button = document.createElement("button");
  button.className = quiet ? "icon quiet" : "icon";
  button.append(icon(iconName), document.createElement("span"));
  button.lastChild.textContent = label;
  button.title = label;
  return button;
}

/**
 * One figure with the thing it measures drawn beside it.
 *
 * The icon here is decoration over a label that is already there in words,
 * which is the one case where a symbol carries no meaning of its own and so
 * cannot be misread.
 */
function fact(iconName, text) {
  const span = document.createElement("span");
  span.className = "fact";
  span.append(icon(iconName));
  const label = document.createElement("span");
  label.textContent = text;
  span.append(label);
  return span;
}

/** The six groupings the engine already sorts downloads into. */
const CATEGORIES = [
  ["Video", "mp4 mkv avi mov wmv flv webm m4v mpg mpeg ts m2ts vob ogv 3gp"],
  ["Audio", "mp3 flac wav aac m4a ogg oga opus wma aiff ape"],
  ["Documents", "pdf doc docx odt rtf txt xls xlsx ods csv ppt pptx odp epub mobi azw3 djvu"],
  ["Programs", "exe msi msix appx dmg pkg deb rpm appimage apk jar"],
  ["Archives", "zip rar 7z tar gz bz2 xz zst tgz iso img cab"],
  ["Images", "jpg jpeg png gif webp bmp tiff tif svg heic avif"],
];

function categoryOf(filename) {
  const extension = filename.includes(".") ? filename.split(".").pop().toLowerCase() : "";
  if (!extension) return { category: "other", extension: "file" };
  const found = CATEGORIES.find(([, list]) => list.split(" ").includes(extension));
  return { category: found ? found[0] : "other", extension };
}

/**
 * The tile at the left of a download, carrying the file's own extension.
 *
 * A drawn glyph for "archive" is a guess at the word the reader already
 * uses; the three letters they named it are not. Colour groups it, the
 * letters say it, so neither is doing the job alone.
 */
function fileTile(filename) {
  const { category, extension } = categoryOf(filename);
  const tile = document.createElement("div");
  tile.className = `job-icon ${category}`;
  tile.textContent = extension.slice(0, 4);
  tile.title = category === "other" ? "File" : category;
  return tile;
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

  const all = snapshot.jobs ?? [];
  const running = all.filter((job) => job.state === "running");
  const total = running.reduce((sum, job) => sum + (job.bytes_per_sec ?? 0), 0);
  const connections = running.reduce((sum, job) => sum + (job.connections ?? 0), 0);

  // Four numbers, each one the engine measures. Nothing here uploads, so
  // there is no upload figure to show -- a meter pinned at zero reads as
  // broken rather than as idle.
  $("speed-now").textContent = rate(total);
  $("speed-conns").textContent = String(connections);
  $("stat-active").textContent = String(all.filter((job) => !settled(job)).length);
  $("stat-done").textContent = String(all.filter((job) => job.state === "completed").length);

  history.push(total);
  while (history.length > SAMPLES) history.shift();
  const peak = Math.max(...history, 0);
  $("stat-peak").textContent = rate(peak);
  drawGraph();

  $("engine-dot").className = "dot good";
  $("engine-text").textContent = running.length ? `${running.length} downloading` : "";

  notifyOnStateChange(snapshot.jobs ?? []);
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

/**
 * What a download's state is called.
 *
 * One state, one name: History calls a finished download "Finished", so
 * calling the same thing "Complete" over here left two words for one state
 * and no way to tell they meant it. The rest were the enum as the engine
 * spells it, which is why they alone were lower case.
 */
function stateLabel(job) {
  if (job.state === "reconnecting") {
    return typeof job.retry_in_secs === "number"
      ? `Reconnecting in ${job.retry_in_secs}s`
      : "Reconnecting";
  }
  const names = {
    completed: "Finished",
    running: "Running",
    queued: "Queued",
    paused: "Paused",
  };
  return names[job.state] ?? job.state;
}

/** Whether a download has stopped for good, one way or the other. */
const settled = (job) => job.state === "completed" || job.state === "failed";

/** How many of the ones that landed are worth repeating outside History. */
const RECENT_DONE = 5;

/** What the list is filtered and ordered by. A view, so the window owns it. */
let listFilter = "";
let listOrder = "recent";

const matchesFilter = (job) => !listFilter || job.filename.toLowerCase().includes(listFilter);

function ordered(jobs) {
  const list = [...jobs];
  const share = (job) => (job.total > 0 ? job.downloaded / job.total : 0);
  if (listOrder === "name") list.sort((a, b) => a.filename.localeCompare(b.filename));
  else if (listOrder === "size") list.sort((a, b) => (b.total ?? 0) - (a.total ?? 0));
  else if (listOrder === "progress") list.sort((a, b) => share(b) - share(a));
  // "recent" is the order the engine hands them over in already.
  return list;
}

function jobCard(job) {
  const card = document.createElement("div");
  card.className = "job";
  card.append(fileTile(job.filename));

  const main = document.createElement("div");
  main.className = "job-main";

  const name = document.createElement("div");
  name.className = "job-name";
  name.textContent = job.filename;
  name.title = job.url;

  // An unknown total means the server never told us; a moving indeterminate
  // bar is honest where a percentage would be invented.
  const known = typeof job.total === "number" && job.total > 0;
  const fraction = known ? Math.min(job.downloaded / job.total, 1) : 0;

  const progress = document.createElement("div");
  progress.className = "job-progress";

  const size = document.createElement("span");
  size.className = "job-size";
  size.textContent = known ? `${bytes(job.downloaded)} / ${bytes(job.total)}` : bytes(job.downloaded);

  const bar = document.createElement("div");
  bar.className = known ? "bar" : "bar indeterminate";
  const fill = document.createElement("span");
  fill.style.width = `${(fraction * 100).toFixed(1)}%`;
  bar.append(fill);

  const pct = document.createElement("span");
  pct.className = "job-pct";
  pct.textContent = known ? `${Math.floor(fraction * 100)}%` : "";

  progress.append(size, bar, pct);

  // While it runs, the three things that are moving. While it does not, the
  // one thing that matters is why -- so the same row carries the state word
  // instead, rather than three figures frozen at their last value.
  const foot = document.createElement("div");
  foot.className = "job-foot";
  if (job.state === "running") {
    foot.append(fact("speed", rate(job.bytes_per_sec)));
    if (known && job.bytes_per_sec > 0) {
      const left = (job.total - job.downloaded) / job.bytes_per_sec;
      foot.append(fact("clock", `${duration(left)} remaining`));
    }
    if (job.connections) foot.append(fact("link", `${job.connections} connections`));
  } else {
    const state = document.createElement("span");
    state.className = `job-state ${job.state}`;
    state.textContent = stateLabel(job);
    foot.append(state);
  }

  const spacer = document.createElement("span");
  spacer.className = "spacer";
  foot.append(spacer);

  const going = job.state === "running" || job.state === "queued";

  // For a link that expired while the download sat waiting. The bytes are
  // still good; only the way back to the rest of them is stale, so this
  // changes the address and keeps the file.
  if (!going) {
    const refresh = document.createElement("button");
    refresh.className = "ghost";
    refresh.textContent = "New link";
    refresh.title = "Paste a fresh address and carry on from here";
    refresh.onclick = async () => {
      const url = window.prompt(`A fresh address for ${job.filename}:`, job.url);
      if (url && url.trim()) await act("refresh_link", { id: job.id, url: url.trim() });
    };
    foot.append(refresh);
  }

  main.append(name, progress, foot);

  if (job.error) {
    const error = document.createElement("div");
    error.className = "job-error";
    error.textContent = job.error;
    main.append(error);
  }

  const controls = document.createElement("div");
  controls.className = "job-controls";

  const toggle = iconButton(going ? "pause" : "play", going ? "Pause" : "Resume");
  toggle.onclick = () => act(going ? "pause_job" : "resume_job", { id: job.id });

  const cancel = iconButton("close", "Cancel", { quiet: true });
  // It cannot be undone, and the tooltip saying so is invisible until a
  // mouse happens to rest on it -- while clearing history, which deletes no
  // file at all, has always asked first.
  cancel.title = "Stop and delete what has been downloaded";
  cancel.onclick = async () => {
    const sure = await confirmDialog(
      `Stop ${job.filename} and delete the ${bytes(job.downloaded)} downloaded so far? Pausing keeps it.`,
      { confirm: "Stop and delete", rememberKey: "skip-cancel-download-confirm" },
    );
    if (sure) act("cancel_job", { id: job.id });
  };

  controls.append(toggle, cancel);
  card.append(main, controls);
  return card;
}

/** One that landed: what it was, how big, and the way back to it. */
function doneRow(job) {
  const row = document.createElement("div");
  row.className = "done-row";

  const tick = icon("check");
  tick.classList.add("tick");

  const name = document.createElement("span");
  name.className = "name";
  name.textContent = job.filename;
  name.title = job.url;

  const meta = document.createElement("span");
  meta.className = "meta";
  meta.textContent = bytes(job.downloaded);

  const open = iconButton("folder", "Open folder");
  open.onclick = () =>
    invoke("reveal", { path: `${job.output_dir}\\${job.filename}` }).catch((error) =>
      toast(String(error), true),
    );

  row.append(tick, name, meta, open);
  return row;
}

function renderJobs(jobs) {
  // A cancelled or failed download has nothing left to show moving: showing
  // it here any longer is how a stale progress bar is left looking alive.
  // It belongs in History now, with the Resume and Remove it offers there.
  const live = ordered(jobs.filter((job) => !settled(job)).filter(matchesFilter));
  const landed = jobs.filter((job) => job.state === "completed").filter(matchesFilter);

  // With nothing running, the panel below already says to drop a file or
  // paste a URL, so saying it again here would be the same sentence twice.
  // A search that found nothing is the one case it cannot explain.
  $("jobs").replaceChildren(
    ...(live.length
      ? live.map(jobCard)
      : listFilter
        ? [empty("Nothing running goes by that name.")]
        : []),
  );
  $("active-count").textContent = live.length ? `(${live.length})` : "";

  // The last few that landed, where you were already looking. History keeps
  // the whole record, so repeating all of it here would make two lists of
  // one thing.
  const recent = landed.slice(-RECENT_DONE).reverse();
  $("done-wrap").hidden = recent.length === 0;
  $("done-count").textContent = `(${recent.length})`;
  $("done-list").replaceChildren(...recent.map(doneRow));
}

function showFolder() {
  const where = downloadDir || "Your Downloads folder (default)";
  $("folder-path").textContent = where;
  $("sidebar-folder-path").textContent = where;
  $("sidebar-folder").title = `${where} — click to change it`;
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
  // Don't reset speed-in-bits: it's UI-only state, not persisted
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

/** Shows the instant speed at the sample nearest the cursor, as in Steam's graph. */
function showGraphTip(event) {
  const canvas = $("graph");
  if (!history.length) return;

  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const step = width / (SAMPLES - 1);
  // The same right-aligned mapping drawGraph uses, so the tooltip always
  // lands on the point it looks like it is pointing at.
  const left = width - (history.length - 1) * step;

  const rect = canvas.getBoundingClientRect();
  const mouseX = event.clientX - rect.left;
  const index = Math.max(0, Math.min(history.length - 1, Math.round((mouseX - left) / step)));
  const value = history[index];

  const peak = Math.max(...history, 1);
  const x = left + index * step;
  const y = height - 2 - (value / peak) * (height - 8);

  const tip = $("graph-tip");
  tip.textContent = rate(value);
  tip.style.left = `${canvas.offsetLeft + x}px`;
  tip.style.top = `${canvas.offsetTop + y}px`;
  tip.hidden = false;
}

function hideGraphTip() {
  $("graph-tip").hidden = true;
}

// ---------------------------------------------------------------------------
// Desktop notifications
// ---------------------------------------------------------------------------

// What each job's state was on the previous snapshot, so a transition can be
// told apart from "still finished from before" on every tick -- otherwise a
// download that was already finished when the window opened would fire a
// notification for something that happened before anyone was watching.
const lastJobState = new Map();

function requestNotificationPermission() {
  if (typeof Notification === "undefined") return;
  if (Notification.permission === "default") Notification.requestPermission();
}

function notifyOnStateChange(jobs) {
  const canNotify = typeof Notification !== "undefined" && Notification.permission === "granted";

  const seen = new Set();
  for (const job of jobs) {
    seen.add(job.id);
    const was = lastJobState.get(job.id);
    lastJobState.set(job.id, job.state);
    if (!canNotify || was === undefined || was === job.state) continue;

    if (job.state === "completed") {
      new Notification("Download finished", {
        body: `${job.filename} — ${bytes(job.total ?? job.downloaded)}`,
      });
    } else if (job.state === "failed" && job.error !== "cancelled") {
      // Cancelling is something the person just chose to do; it is not news.
      new Notification("Download stopped", {
        body: `${job.filename} — ${job.error ?? "stopped unexpectedly"}`,
      });
    }
  }

  // A job the engine no longer mentions at all should not haunt this map
  // forever.
  for (const id of lastJobState.keys()) {
    if (!seen.has(id)) lastJobState.delete(id);
  }
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
  showSpeedInBits = $("speed-in-bits").getAttribute("aria-pressed") === "true";
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

        // The word, not just the colour: a badge that says what happened,
        // so telling two rows apart never depends on being able to see red
        // from green.
        const finished = job.state === "completed";
        const badge = document.createElement("span");
        badge.className = `hrow-badge ${job.state}`;
        badge.textContent = finished ? "Finished" : "Incomplete";

        row.append(name, badge, meta);

        if (finished) {
          // Finished: the only thing left to do with it is find it again.
          const open = iconButton("folder", "Open folder");
          open.onclick = () =>
            invoke("reveal", { path: `${job.output_dir}\\${job.filename}` }).catch((error) =>
              toast(String(error), true),
            );
          row.append(open);
        } else {
          // Incomplete — cancelled or genuinely failed. Either way the bytes
          // on disk are still there to carry on from, or to simply forget.
          if (job.error) {
            const reason = document.createElement("div");
            reason.className = "meta reason";
            reason.textContent = job.error;
            // It is the first thing cut short when the window is narrow, so
            // the whole sentence has to stay reachable somewhere.
            reason.title = job.error;
            row.append(reason);
          }

          const resume = iconButton("play", "Resume");
          resume.title = "Resume — carries on from the bytes already on disk";
          resume.onclick = async () => {
            try {
              await invoke("resume_from_history", { id: job.id });
              toast("Resuming — check the Downloads tab");
              showHistory();
            } catch (error) {
              toast(String(error), true);
            }
          };

          const remove = iconButton("trash", "Remove", { quiet: true });
          remove.title = "Remove from history";
          remove.onclick = async () => {
            try {
              await invoke("remove_history_row", { id: job.id });
              showHistory();
            } catch (error) {
              toast(String(error), true);
            }
          };

          row.append(resume, remove);
        }
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

const tabs = [...document.querySelectorAll(".nav-item")];

function selectTab(tab, giveFocus = false) {
  for (const other of tabs) {
    const active = other === tab;
    other.classList.toggle("is-active", active);
    // The state a screen reader announces, not only the one shown in paint.
    other.setAttribute("aria-selected", String(active));
  }
  for (const name of ["active", "history", "settings"]) {
    $(`panel-${name}`).hidden = name !== tab.dataset.tab;
  }
  if (tab.dataset.tab === "history") showHistory();
  // A browser may have been installed or paired since this was last looked at.
  if (tab.dataset.tab === "settings") refreshBrowsers();
  if (giveFocus) tab.focus();
}

for (const tab of tabs) {
  tab.addEventListener("click", () => selectTab(tab));
  // Left/Right move the selection too, as the standard tab widget does —
  // Tab alone would still work, but an arrow key is the shortcut people who
  // use tabs every day already reach for.
  tab.addEventListener("keydown", (event) => {
    const index = tabs.indexOf(tab);
    // Up/Down now the list runs down the side; Left/Right still work, since
    // someone used to them should not have to find that out.
    const next = ["ArrowDown", "ArrowRight"].includes(event.key);
    const back = ["ArrowUp", "ArrowLeft"].includes(event.key);
    if (next) selectTab(tabs[(index + 1) % tabs.length], true);
    else if (back) selectTab(tabs[(index - 1 + tabs.length) % tabs.length], true);
  });
}

// --------------------------------------------------------------- the list

$("search").addEventListener("input", (event) => {
  listFilter = event.target.value.trim().toLowerCase();
  renderJobs(latest.jobs ?? []);
});

$("sort").addEventListener("change", (event) => {
  listOrder = event.target.value;
  renderJobs(latest.jobs ?? []);
});

// Pasting is the whole reason the field is there, and reaching for the
// keyboard to do it is a step this can simply take.
$("paste").addEventListener("click", async () => {
  try {
    const text = (await navigator.clipboard.readText()).trim();
    if (!text) return toast("Nothing on the clipboard", true);
    $("url").value = text;
    $("url").focus();
  } catch (error) {
    toast(String(error), true);
  }
});

$("sidebar-folder").addEventListener("click", () => {
  selectTab($("tab-settings"));
  $("choose-folder").focus();
});

// ---------------------------------------------------------- dropped files

// A .torrent or a shortcut dropped on the window, rather than its address
// typed in. Only the window as a whole has to accept it; the dashed panel
// says where, but anywhere is as good.
const dropzone = $("dropzone");

for (const name of ["dragenter", "dragover"]) {
  window.addEventListener(name, (event) => {
    event.preventDefault();
    dropzone.classList.add("is-over");
  });
}

for (const name of ["dragleave", "drop"]) {
  window.addEventListener(name, (event) => {
    event.preventDefault();
    if (name === "dragleave" && event.relatedTarget) return;
    dropzone.classList.remove("is-over");
  });
}

window.addEventListener("drop", async (event) => {
  const text = event.dataTransfer?.getData("text")?.trim();
  if (!text) return;
  try {
    await invoke("add_download", { url: text, filename: null });
    refresh();
  } catch (error) {
    toast(String(error), true);
  }
});

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

$("speed-in-bits").addEventListener("click", () => {
  const btn = $("speed-in-bits");
  const isPressed = btn.getAttribute("aria-pressed") === "true";
  btn.setAttribute("aria-pressed", !isPressed);
  btn.querySelector(".toggle-label").textContent = isPressed ? "MB/s" : "Mbps";
  showSpeedInBits = !isPressed;
  saveSettings();
});

// ---------------------------------------------------------------------------
// Confirmation
// ---------------------------------------------------------------------------

/**
 * Shows the shared confirmation modal; resolves to whether the person
 * confirmed. Skipped entirely once they have ticked "Remember my choice"
 * for this `rememberKey`, kept in localStorage since it is a preference
 * about this window, not a setting the engine needs to know.
 *
 * `confirm` names the action on the button that performs it. Someone who
 * reads nothing but the button they are about to press should still know
 * what it does, which a dialog with one fixed word on it cannot tell them.
 */
function confirmDialog(message, { confirm = "Confirm", rememberKey } = {}) {
  try {
    if (rememberKey && localStorage.getItem(rememberKey) === "1") return Promise.resolve(true);
  } catch {
    // A locked-down profile can refuse storage; falling through just means
    // asking every time instead of skipping a destructive action unseen.
  }

  return new Promise((resolve) => {
    const overlay = $("confirm-modal");
    const remember = $("confirm-remember");
    const cancelBtn = $("confirm-cancel");
    const okBtn = $("confirm-ok");

    $("confirm-message").textContent = message;
    okBtn.textContent = confirm;
    remember.checked = false;
    overlay.hidden = false;

    const finish = (confirmed) => {
      overlay.hidden = true;
      cancelBtn.removeEventListener("click", onCancel);
      okBtn.removeEventListener("click", onOk);
      if (confirmed && rememberKey && remember.checked) {
        try {
          localStorage.setItem(rememberKey, "1");
        } catch {
          // Nothing to do about it; it only means asking again next time.
        }
      }
      resolve(confirmed);
    };
    const onCancel = () => finish(false);
    const onOk = () => finish(true);
    cancelBtn.addEventListener("click", onCancel);
    okBtn.addEventListener("click", onOk);
  });
}

$("clear-history").addEventListener("click", async () => {
  const sure = await confirmDialog(
    "Clear the whole history? Finished downloads will no longer show in this list — the files on disk are untouched.",
    { confirm: "Clear all", rememberKey: "skip-clear-all-history-confirm" },
  );
  if (!sure) return;
  await act("clear_history");
  showHistory();
});

$("clear-incomplete").addEventListener("click", async () => {
  const sure = await confirmDialog(
    "Clear incomplete downloads from history? The files on disk are untouched.",
    { confirm: "Clear incomplete", rememberKey: "skip-clear-incomplete-history-confirm" },
  );
  if (!sure) return;
  await act("clear_incomplete_history");
  showHistory();
});

// The engine pushes; the window does not poll. A pull loop would keep the
// renderer busy with nothing to do and put the update rate in the hands of a
// JavaScript timer rather than the side that knows when something changed.
listen("snapshot", (event) => render(event.payload));
window.addEventListener("resize", drawGraph);
$("graph").addEventListener("mousemove", showGraphTip);
$("graph").addEventListener("mouseleave", hideGraphTip);

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
  hide.append("\u2715", document.createElement("span"));
  hide.lastChild.textContent = "Hide";
  hide.title = `Hide ${row.name} from this list`;
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
    const copy = document.createElement("button");
    copy.className = "ghost";
    copy.textContent = "Copy";
    // A browser's "Load unpacked" dialog opens to a folder picker, not a
    // file explorer the path can be pasted into from Explorer -- so the
    // path itself, ready to paste there, is more useful than opening it.
    copy.onclick = async () => {
      try {
        await navigator.clipboard.writeText(path);
        toast("Path copied");
      } catch (error) {
        toast(String(error), true);
      }
    };
    row.append(code, copy);
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

requestNotificationPermission();
refresh();
