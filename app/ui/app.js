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
  // The three other things a download can have turned out to be. Shapes, not
  // only colours: which of these a row is must survive being looked at by
  // someone who cannot tell the red one from the green one.
  warn: [
    ["path", { d: "M12 4.2 2.6 20.2h18.8z" }],
    ["path", { d: "M12 10v4.4" }],
    ["path", { d: "M12 17.3v.2" }],
  ],
  bang: [
    ["circle", { cx: "12", cy: "12", r: "8.6" }],
    ["path", { d: "M12 7.4v5.4" }],
    ["path", { d: "M12 16.2v.2" }],
  ],
  hollow: [["circle", { cx: "12", cy: "12", r: "8.6" }]],
  more: [
    ["circle", { cx: "12", cy: "5.4", r: "1.5", fill: "currentColor", stroke: "none" }],
    ["circle", { cx: "12", cy: "12", r: "1.5", fill: "currentColor", stroke: "none" }],
    ["circle", { cx: "12", cy: "18.6", r: "1.5", fill: "currentColor", stroke: "none" }],
  ],
  chevron: [["path", { d: "m9.5 5 7 7-7 7" }]],
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
  tile.title = category === "other" ? "File" : category;

  // The shell's own icon where we have it by now, the letters until then
  // and for good where the shell has none -- so the tile is never empty
  // and never waiting on anything to be useful.
  const known = shellIcons.get(extension);
  if (known) {
    const image = document.createElement("img");
    image.src = known;
    image.alt = "";
    tile.append(image);
    tile.classList.add("has-icon");
  } else {
    tile.textContent = extension.slice(0, 4);
    askShellForIcon(extension);
  }
  return tile;
}

/**
 * What Explorer draws for each extension, asked once and kept.
 *
 * A value of `null` means the shell had nothing, which is as worth
 * remembering as an icon: it stops every later row asking again.
 */
const shellIcons = new Map();
const iconAsked = new Set();

function askShellForIcon(extension) {
  if (!extension || extension === "file" || iconAsked.has(extension)) return;
  iconAsked.add(extension);
  invoke("file_icon", { extension })
    .then(({ width, height, rgba }) => {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      canvas
        .getContext("2d")
        .putImageData(new ImageData(new Uint8ClampedArray(rgba), width, height), 0, 0);
      shellIcons.set(extension, canvas.toDataURL());
      // The next snapshot is along in under a second and rebuilds the list
      // anyway, so there is nothing to redraw by hand.
    })
    .catch(() => shellIcons.set(extension, null));
}

function render(snapshot) {
  latest = snapshot;
  takeQueues(snapshot.queues ?? []);
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

  $("engine-text").className = "";
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
    failed: "Failed",
    cancelled: "Cancelled",
  };
  return names[job.state] ?? job.state;
}

/** Whether a download has stopped for good, one way or the other. */
const settled = (job) =>
  job.state === "completed" || job.state === "failed" || job.state === "cancelled";

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
    if (job.connections) {
      // The same field, but not the same quantity: for a torrent nothing on
      // this side chose the number, the swarm did, and "connections" would
      // suggest a setting that governs it.
      const unit = isTorrent(job.url) ? "peers" : "connections";
      foot.append(fact("link", `${job.connections} ${unit}`));
    }
  } else {
    const state = document.createElement("span");
    state.className = `job-state ${job.state}`;
    state.textContent = stateLabel(job);
    foot.append(state);
  }

  // Both the answer to "which queue is this in" and the way to change it.
  // One control rather than a chip plus a menu: the chip would have to be
  // clicked to find out there was a menu at all.
  if (queues.length > 0) {
    const pick = document.createElement("select");
    pick.className = "job-queue";
    pick.title = "Which queue holds this download";
    fillQueueOptions(pick, job.queue ?? null);
    pick.onchange = () =>
      queueAction("assign_queue", {
        id: job.id,
        queue: pick.value === "" ? null : Number(pick.value),
      });
    foot.append(pick);
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
      // Not window.prompt: the webview draws that one itself, titled
      // "tauri.localhost says", in the system's light colours -- the app
      // looking for a moment like a web page that got loose.
      const url = await askFor(`A fresh address for ${job.filename}:`, {
        value: job.url,
        placeholder: "https://…",
        confirm: "Use this address",
      });
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

  // Not an error, and styled so it does not read as one: the download is
  // fine. This is where a second address that turned out to serve different
  // bytes gets said out loud, rather than being dropped in silence.
  if (job.note) {
    const note = document.createElement("div");
    note.className = "job-note";
    note.textContent = job.note;
    main.append(note);
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

  const check = iconButton("check", "Verify", { quiet: true });
  check.title = "Check it against the hash its source published";
  check.onclick = () => verifyDownload(job);

  const open = iconButton("folder", "Open folder");
  open.onclick = () =>
    invoke("reveal", { path: `${job.output_dir}\\${job.filename}` }).catch((error) =>
      toast(String(error), true),
    );

  row.append(tick, name, meta, check, open);
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

function renderSettings(settings, force = false) {
  // Always, even mid-interaction: this is the value the next save will
  // carry, and a stale one would revert the folder.
  downloadDir = settings?.download_dir ?? downloadDir;
  hiddenBrowsers = settings?.hidden_browsers ?? hiddenBrowsers;
  if (!settings || (!force && document.activeElement?.closest("#panel-settings"))) {
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
  $("proxy-mode").value = settings.proxy ?? "system";
  $("proxy-address").value = settings.proxy_address ?? "";
  showProxyAddress();
  showFolder();
}

/** The address box is only meaningful for the one choice that uses it. */
function showProxyAddress() {
  // Three modes, three shapes. `manual` and `pac` share the address box
  // because they both need one; what they need is not the same thing, so
  // the label and the note beside it change with the mode rather than
  // leaving somebody to guess which is wanted.
  const mode = $("proxy-mode").value;
  const manual = mode === "manual";
  const script = mode === "pac";
  $("proxy-address-row").hidden = !manual && !script;
  $("proxy-address-label").textContent = script ? "Script address" : "Proxy address";
  $("proxy-address").placeholder = script ? "http://wpad/wpad.dat" : "http://127.0.0.1:3128";
  $("proxy-note").hidden = !manual;
  // Shown for both automatic modes: it explains what is happening, and
  // `auto` has no box of its own to explain itself with.
  $("proxy-script-note").hidden = !script && mode !== "auto";
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

/**
 * Whether a snapshot has been seen yet.
 *
 * The first one describes whatever was already going when the window opened,
 * including anything the engine picked up from a previous run. None of that
 * is news, so it is recorded without being announced.
 */
let snapshotsSeen = false;

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
    const fresh = was === undefined;
    if (was === job.state) continue;

    // A failed download is dropped from the list above the moment it fails,
    // because a stopped progress bar is worse than no bar. That left the
    // panel the person was watching with nothing on it and nothing said --
    // a pasted link that failed looked like a link that was never taken.
    // So the toast is not conditional on desktop notifications being
    // allowed: it is the only word they get in the window itself.
    //
    // A job seen for the first time already failed counts. A link that is
    // refused outright fails well inside the half second between snapshots,
    // so there is no earlier state to have changed from -- and that is
    // exactly the case worth saying something about. Only on the opening
    // snapshot is it held back, where it would be old news.
    if (job.state === "failed" && (!fresh || snapshotsSeen)) {
      toast(`${job.filename} — ${job.error ?? "stopped unexpectedly"}`, true);
    }

    if (!canNotify || fresh) continue;

    if (job.state === "completed") {
      new Notification("Download finished", {
        body: `${job.filename} — ${bytes(job.total ?? job.downloaded)}`,
      });
    } else if (job.state === "failed") {
      // Cancelling is something the person just chose to do, so it is not
      // news -- and it no longer has to be recognised by the word in the
      // error field, because it is a state of its own now.
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

  snapshotsSeen = true;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Queues
// ---------------------------------------------------------------------------

// The queues as the engine last reported them. Kept here because three
// separate things draw from it — the picker in the add form, the panel, and
// the select on every job card — and each of those would otherwise ask the
// engine for itself and show a list from a different instant.
let queues = [];

/** Re-reads the queues and redraws everything that shows them. */
async function refreshQueues() {
  try {
    queues = await invoke("queues");
  } catch (error) {
    // Not a toast. The engine runs without its database on purpose, and
    // somebody who never opens this panel should not be told about it.
    queues = [];
  }
  showQueuePicker();
  renderQueues();
  // The job list draws a queue select of its own, and it is built from this
  // list. Without redrawing here, the cards rendered before the queues
  // arrived keep no select at all -- which is every card on screen the first
  // time the window opens.
  refresh();
}

/**
 * What the pickers were last built from, so they are only rebuilt when the
 * thing they show changed.
 *
 * The snapshot arrives twice a second and the running and waiting counts
 * move with it, but none of those numbers appear on an option. Rebuilding a
 * `<select>`'s options closes it if it happens to be open, which on a list
 * that changes twice a second means a dropdown that cannot be used at all.
 */
let pickerSignature = "";

/**
 * Takes the queues the snapshot carried and redraws what shows them.
 *
 * The panel is redrawn whenever anything changed, counts included -- that is
 * what the panel is for. The pickers are left alone unless a name, a limit
 * or an on/off changed, since nothing else reaches an option's label.
 */
function takeQueues(fresh) {
  queues = fresh;

  const signature = JSON.stringify(
    fresh.map((queue) => [queue.id, queue.name, queue.max_concurrent, queue.enabled]),
  );
  if (signature !== pickerSignature) {
    pickerSignature = signature;
    // Every job card carries one of these too, but those are rebuilt with
    // the card, so only the one in the top bar needs saying here.
    showQueuePicker();
  }
  // Unconditionally, like the job list beside it. Skipping this when the
  // queues themselves had not changed left the panel showing whatever it
  // last managed to draw: a queue added while the Add button still held
  // focus was blocked once by the guard below and then never redrawn,
  // because nothing had "changed" by the time focus moved away.
  renderQueues();
}

/** Fills a select with the queues, keeping `chosen` selected if it is still there. */
function fillQueueOptions(select, chosen) {
  select.replaceChildren();
  const none = document.createElement("option");
  none.value = "";
  none.textContent = "No queue";
  select.append(none);
  for (const queue of queues) {
    const option = document.createElement("option");
    option.value = String(queue.id);
    // Said on the option rather than only in the panel, because this is
    // where the choice is actually made.
    option.textContent = queue.enabled
      ? `${queue.name} (${queue.max_concurrent} at once)`
      : `${queue.name} (off)`;
    select.append(option);
  }
  select.value = chosen == null ? "" : String(chosen);
  // A queue that has been removed since: fall back rather than leave the
  // select showing a blank with a value behind it.
  if (select.value !== (chosen == null ? "" : String(chosen))) select.value = "";
}

function showQueuePicker() {
  const wrap = $("queue-pick-wrap");
  const select = $("queue-pick");
  const chosen = select.value;
  wrap.hidden = queues.length === 0;
  fillQueueOptions(select, chosen === "" ? null : Number(chosen));
}

function renderQueues() {
  const list = $("queue-list");
  // Mid-edit, leave it be. This redraws twice a second now, and each card
  // holds a number field: rebuilding one while somebody is typing a new
  // limit into it takes the cursor away mid-digit.
  //
  // A field, not the whole panel. A button keeps focus after it is clicked,
  // so guarding on the panel meant pressing Add queue froze the very list
  // the press had just added to.
  const editing = document.activeElement;
  if (editing?.tagName === "INPUT" && editing.closest("#panel-queues")) return;
  list.replaceChildren();

  if (queues.length === 0) {
    const empty = document.createElement("p");
    empty.className = "queues-empty";
    empty.textContent = "No queues yet. Name one above to start holding downloads back.";
    list.append(empty);
    return;
  }

  for (const queue of queues) {
    const card = document.createElement("div");
    card.className = queue.enabled ? "queue" : "queue is-off";

    const name = document.createElement("div");
    name.className = "queue-title";
    name.textContent = queue.name;

    const counts = document.createElement("div");
    counts.className = "queue-counts";
    // Both numbers always, including the zeroes: "0 waiting" is the answer
    // to "is anything stuck in here", and a field that disappears when it is
    // zero cannot give it.
    counts.textContent = `${queue.running} running · ${queue.waiting} waiting`;

    const atOnce = document.createElement("label");
    atOnce.className = "queue-at-once";
    const atOnceText = document.createElement("span");
    atOnceText.textContent = "at once";
    const atOnceInput = document.createElement("input");
    atOnceInput.type = "number";
    atOnceInput.min = "1";
    atOnceInput.max = "16";
    atOnceInput.step = "1";
    atOnceInput.value = String(queue.max_concurrent);
    atOnceInput.onchange = async () => {
      const wanted = Number(atOnceInput.value);
      if (!Number.isFinite(wanted) || wanted < 1) {
        atOnceInput.value = String(queue.max_concurrent);
        return;
      }
      // Set by name, which is how the engine identifies a queue to change:
      // the same request creates one and changes one.
      await queueAction("set_queue", { name: queue.name, maxConcurrent: wanted });
    };
    atOnce.append(atOnceText, atOnceInput);

    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "ghost";
    toggle.textContent = queue.enabled ? "Switch off" : "Switch on";
    toggle.title = queue.enabled
      ? "Stop starting anything more from this queue"
      : "Let this queue start downloads again";
    toggle.onclick = () =>
      queueAction("enable_queue", { id: queue.id, enabled: !queue.enabled });

    const remove = document.createElement("button");
    remove.type = "button";
    // Ghost, not the solid red Cancel wears: removing a queue keeps every
    // download in it, so dressing it as destruction would overstate it.
    remove.className = "ghost queue-remove";
    remove.textContent = "Remove";
    remove.title = "Remove the queue. Its downloads are kept.";
    remove.onclick = async () => {
      const sure = await confirmDialog(
        `Remove the queue “${queue.name}”? Its downloads are kept and become ` +
          "unqueued, so anything waiting in it will start.",
        { confirm: "Remove" },
      );
      if (sure) await queueAction("remove_queue", { id: queue.id });
    };

    const controls = document.createElement("div");
    controls.className = "queue-controls";
    controls.append(atOnce, toggle, remove);

    card.append(name, counts, controls);
    list.append(card);
  }
}

/** Sends a queue change and takes the fresh list straight from the reply. */
async function queueAction(command, args = {}) {
  try {
    queues = await invoke(command, args);
    showQueuePicker();
    renderQueues();
    // The list of downloads changes too: a queue change can start something.
    refresh();
  } catch (error) {
    toast(String(error), true);
  }
}

$("queue-add").addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = $("queue-name").value.trim();
  if (!name) {
    $("queue-name").focus();
    return;
  }
  await queueAction("set_queue", {
    name,
    maxConcurrent: Math.max(1, Number($("queue-at-once").value) || 1),
  });
  $("queue-name").value = "";
});

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
    $("engine-text").className = "bad";
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
    proxy: $("proxy-mode").value,
    proxy_address: $("proxy-address").value.trim(),
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
    // Put every control back to what the engine actually holds.
    //
    // This save carried the whole settings object, built from the panel, so
    // one field the engine refuses takes the rest down with it. Leaving the
    // refused value sitting in the control meant the next unrelated change
    // -- a checkbox three groups away -- rebuilt the same bad object and was
    // refused for the same reason, with an error naming a field the person
    // had not touched. Meanwhile every control showed a state the engine had
    // never accepted. Snapping back costs the text they just typed and is
    // worth it: the window should not show settings that are not in force.
    renderSettings(latest?.settings, true);
  }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/**
 * History is an archive, not a second download queue.
 *
 * It keeps every download for good, including the ones that worked: the
 * reason to look something up months later is almost always a file you did
 * get -- where it went, what its link was, whether to fetch it again. So the
 * rows are built to be read and found, not acted on. A row carries a glyph, a
 * name and one line of context; everything you can do to it is behind an
 * explicit menu, and clicking the row itself only selects it.
 *
 * It had two words for four outcomes before this -- "Finished" and
 * "Incomplete" -- which put a download the user cancelled, one that ran out
 * of disk and one they stopped for lunch under the same label.
 */

/** The four things a download can have turned out to be. */
const OUTCOMES = {
  completed: { label: "Completed", icon: "check" },
  incomplete: { label: "Incomplete", icon: "bang" },
  failed: { label: "Failed", icon: "warn" },
  cancelled: { label: "Cancelled", icon: "hollow" },
};

/**
 * Which of the four a stored row is.
 *
 * Read from the state alone. Cancelling used to be recorded as a failure
 * whose error text happened to read "cancelled", so telling a decision from
 * a fault meant comparing an English word against a field the server also
 * writes to.
 */
function outcomeOf(job) {
  switch (job.state) {
    case "completed":
      return "completed";
    case "failed":
      // Rows written before cancelling had a state of its own: a failure
      // whose whole error text is the word. Reading it here is the one
      // place that comparison is still right, because these rows cannot be
      // reclassified -- nothing knows now what they were then beyond this.
      return job.error === "cancelled" ? "cancelled" : "failed";
    case "cancelled":
      return "cancelled";
    // Paused: stopped without finishing and without anything going wrong.
    default:
      return "incomplete";
  }
}

/** The rows as last fetched. Filtering and searching never refetch. */
let historyRows = [];
let historyOutcome = "all";
let historyQuery = "";
/** The selected row, which is all a click on one does. */
let historySelected = null;
/**
 * What the user has opened or closed by hand.
 *
 * Two sets rather than one, because "not closed" and "opened" are different
 * from the default, and a rebuild has to leave both alone.
 */
const daysClosed = new Set();
const daysOpened = new Set();
const openDetails = new Set();
/** The one open overflow menu, so anything else closes it. */
let openMenu = null;

const DAY = 86_400_000;

function startOfDay(ms) {
  const date = new Date(Number(ms));
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/**
 * The heading over a day's downloads.
 *
 * Words where words are what people use. "TODAY" and "YESTERDAY" are how the
 * last two days are referred to, and a date is only worth spelling out once
 * it is far enough away to need spelling out.
 */
function dayHeading(dayMs) {
  const today = startOfDay(Date.now());
  const date = new Date(dayMs);
  const thisYear = date.getFullYear() === new Date().getFullYear();
  const spelled = date
    .toLocaleDateString(
      [],
      thisYear
        ? { day: "numeric", month: "long" }
        : { day: "numeric", month: "long", year: "numeric" },
    )
    .toUpperCase();
  if (dayMs === today) return `TODAY \u00B7 ${spelled}`;
  if (dayMs === today - DAY) return `YESTERDAY \u00B7 ${spelled}`;
  return spelled;
}

/** The time of day. The date is already on the group it sits in. */
function clockOf(ms) {
  if (!ms) return "";
  return new Date(Number(ms)).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function dayIsOpen(dayMs) {
  if (daysClosed.has(dayMs)) return false;
  if (daysOpened.has(dayMs)) return true;
  // Today and yesterday open, everything older closed. A year of history
  // should not have to be scrolled past to reach this morning.
  return dayMs >= startOfDay(Date.now()) - DAY;
}

/**
 * The one line under the filename.
 *
 * A failed row spends it on the reason rather than on the word "failed",
 * which the glyph has already said. Repeating the status there would use the
 * only line the row has to tell you nothing.
 */
function metaFor(job, outcome) {
  const time = clockOf(job.at_ms);
  if (outcome === "completed") {
    return `Completed \u00B7 ${bytes(job.total ?? job.downloaded)} \u00B7 ${time}`;
  }
  if (outcome === "failed") {
    return `${job.error_summary || job.error || "Failed"} \u00B7 ${time}`;
  }
  const label = outcome === "cancelled" ? "Cancelled" : "Incomplete";
  const size = job.total
    ? `${bytes(job.downloaded)} of ${bytes(job.total)}`
    : `${bytes(job.downloaded)} downloaded`;
  return `${label} \u00B7 ${size} \u00B7 ${time}`;
}

/**
 * An icon with no word beside it.
 *
 * `iconButton` refuses to make one, for good reasons that still hold: a
 * symbol can be read differently by different people and a tooltip is
 * invisible until a mouse sits still on it. A history row carries two of
 * these and the words would crowd out the filename, so the label moves into
 * `aria-label` and a hidden span instead of being dropped. The control is
 * still named -- it is unnamed only to the eye, which has the tooltip.
 */
function iconOnlyButton(iconName, label) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "icon-only";
  button.append(icon(iconName));
  const said = document.createElement("span");
  said.className = "sr-only";
  said.textContent = label;
  button.append(said);
  button.title = label;
  button.setAttribute("aria-label", label);
  return button;
}

function closeMenu() {
  if (!openMenu) return;
  openMenu.menu.hidden = true;
  openMenu.button.setAttribute("aria-expanded", "false");
  openMenu = null;
}

/**
 * The row's actions, behind one button.
 *
 * Laying them out flat is what made the old rows look like a queue: four
 * controls each, on every row, for things you do to one row a month.
 */
function overflowMenu(items) {
  const wrap = document.createElement("div");
  wrap.className = "hmenu-wrap";

  const button = iconOnlyButton("more", "More actions");
  button.setAttribute("aria-haspopup", "true");
  button.setAttribute("aria-expanded", "false");

  const menu = document.createElement("div");
  menu.className = "hmenu";
  menu.hidden = true;
  menu.setAttribute("role", "menu");

  for (const item of items) {
    if (!item) {
      const line = document.createElement("div");
      line.className = "hmenu-line";
      menu.append(line);
      continue;
    }
    const entry = document.createElement("button");
    entry.type = "button";
    // Not `danger`: that is a filled red button elsewhere in this window,
    // and `button.danger` would win the background off a menu item.
    entry.className = item.danger ? "hmenu-item is-danger" : "hmenu-item";
    entry.setAttribute("role", "menuitem");
    entry.textContent = item.label;
    entry.onclick = () => {
      closeMenu();
      item.run();
    };
    menu.append(entry);
  }

  button.onclick = (event) => {
    // Otherwise the document handler below closes it in the same click, and
    // the row underneath takes the click as a selection.
    event.stopPropagation();
    const wasOpen = !menu.hidden;
    closeMenu();
    if (wasOpen) return;
    menu.hidden = false;
    button.setAttribute("aria-expanded", "true");
    openMenu = { menu, button };
  };
  // A click inside the menu is the menu's, not the row's.
  menu.addEventListener("click", (event) => event.stopPropagation());

  wrap.append(button, menu);
  return wrap;
}

function historyPath(job) {
  return `${job.output_dir}\\${job.filename}`;
}

async function copyLink(url) {
  try {
    await navigator.clipboard.writeText(url);
    toast("Link copied");
  } catch (error) {
    toast(String(error), true);
  }
}

/** Carries on from the bytes already on disk, where there are any. */
async function retryFromHistory(job) {
  try {
    await invoke("resume_from_history", { id: job.id });
    toast("Started \u2014 check the Downloads tab");
    showHistory();
  } catch (error) {
    toast(String(error), true);
  }
}

/**
 * Picks it up again from a different address, keeping the bytes it has.
 *
 * The same thing Try again does, with one field more. The engine treats a
 * replacement address exactly as it treats a new download's -- it is no more
 * trustworthy for having been typed in here -- and then carries on from what
 * is already on disk, which it verifies against the journal's block
 * checksums before adding to.
 */
async function relinkFromHistory(job) {
  const url = await askFor(`A fresh address for ${job.filename}:`, {
    value: job.url,
    placeholder: "https://\u2026",
    confirm: "Use this address",
  });
  if (!url || !url.trim()) return;
  try {
    await invoke("resume_from_history", { id: job.id, url: url.trim() });
    toast("Started \u2014 check the Downloads tab");
    showHistory();
  } catch (error) {
    toast(String(error), true);
  }
}

/**
 * Fetches it again from scratch.
 *
 * A finished download has nothing to resume, so this is a new job from the
 * old address rather than a retry of the old one.
 */
async function downloadAgain(job) {
  try {
    await invoke("add_download", { url: job.url, filename: null });
    toast("Added \u2014 check the Downloads tab");
  } catch (error) {
    toast(String(error), true);
  }
}

async function removeFromHistory(job) {
  try {
    await invoke("remove_history_row", { id: job.id });
    if (historySelected === job.id) historySelected = null;
    showHistory();
  } catch (error) {
    toast(String(error), true);
  }
}

/** Where an unfinished download is actually managed. */
function showInDownloads(job) {
  const search = $("search");
  search.value = job.filename;
  search.dispatchEvent(new Event("input"));
  selectTab($("tab-active"));
}

/** Everything the error knew, for the one person in a hundred who opens it. */
function technicalDetails(job) {
  const lines = [];
  if (job.error_kind) lines.push(`Kind: ${job.error_kind}`);
  if (job.error) lines.push(`Error: ${job.error}`);
  lines.push(`URL: ${job.url}`);
  lines.push(`Folder: ${job.output_dir}`);
  lines.push(
    job.total
      ? `Progress: ${bytes(job.downloaded)} of ${bytes(job.total)}`
      : `Downloaded: ${bytes(job.downloaded)}`,
  );
  lines.push(`When: ${new Date(Number(job.at_ms)).toLocaleString()}`);
  return lines.join("\n");
}

function historyMenuItems(job, outcome) {
  const path = historyPath(job);
  const shell = (command) => () =>
    invoke(command, { path }).catch((error) => toast(String(error), true));
  const items = [];

  if (outcome === "incomplete") {
    // Still live work. The engine is holding this one and the Downloads tab
    // is already showing it, so a second set of controls here would be two
    // ways to drive one download.
    items.push({ label: "Show in Downloads", run: () => showInDownloads(job) });
  } else if (outcome === "completed") {
    items.push({ label: "Open file", run: shell("open_file") });
  } else {
    items.push({
      label: outcome === "failed" ? "Try again" : "Download again",
      run: () => retryFromHistory(job),
    });
    // The reason most big downloads fail is that their address stopped
    // working, and Try again on its own fails the same way for the same
    // reason. New link exists for exactly this and could not be reached
    // from here: the button that offers it lives on a download's card, and
    // a failed download is taken off that list the moment it fails.
    if (outcome === "failed") {
      // A site that wants a password needs one, not a fresh address, so
      // this comes first when that is the reason. Matched on the kind
      // rather than on the message, because matching on prose makes the
      // wording load-bearing: rewording it would quietly remove the entry.
      if (job.error_kind === "auth.required") {
        items.push({ label: "Sign in…", run: () => signInFor(job) });
      }
      items.push({ label: "New link…", run: () => relinkFromHistory(job) });
    }
  }

  items.push({ label: "Open folder", run: shell("reveal") });
  items.push({ label: "Copy link", run: () => copyLink(job.url) });

  if (outcome === "completed") {
    items.push({ label: "Check against a hash\u2026", run: () => verifyDownload(job) });
    items.push({ label: "Download again", run: () => downloadAgain(job) });
  }

  if (outcome !== "incomplete") {
    // Removing a paused row would delete a download that can still be
    // resumed, which is not what tidying a list should do.
    items.push(null);
    items.push({
      label: "Remove from history",
      danger: true,
      run: () => removeFromHistory(job),
    });
  }
  return items;
}

function historyRow(job) {
  const outcome = outcomeOf(job);
  const row = document.createElement("div");
  row.className = historySelected === job.id ? "hrow is-selected" : "hrow";

  const status = document.createElement("span");
  status.className = `hstatus ${outcome}`;
  status.append(icon(OUTCOMES[outcome].icon));
  // The glyph is the primary cue, so it carries the word for anyone not
  // reading it by eye.
  const said = document.createElement("span");
  said.className = "sr-only";
  said.textContent = OUTCOMES[outcome].label;
  status.append(said);
  status.title = OUTCOMES[outcome].label;

  const main = document.createElement("div");
  main.className = "hrow-main";

  const name = document.createElement("div");
  name.className = "name";
  name.textContent = job.filename;
  name.title = job.url;

  const meta = document.createElement("div");
  meta.className = "meta";
  meta.textContent = metaFor(job, outcome);
  // The first thing cut short in a narrow window, so the whole line has to
  // stay reachable somewhere.
  meta.title = meta.textContent;

  main.append(name, meta);

  if (job.error) {
    const details = document.createElement("details");
    details.className = "hdetails";
    details.open = openDetails.has(job.id);

    const summary = document.createElement("summary");
    const chev = icon("chevron");
    chev.classList.add("chev");
    const label = document.createElement("span");
    label.textContent = "Technical details";
    summary.append(chev, label);
    details.append(summary);

    const pre = document.createElement("pre");
    pre.textContent = technicalDetails(job);
    details.append(pre);

    // Survives the rebuild that a filter change or a retry causes.
    details.addEventListener("toggle", () => {
      if (details.open) openDetails.add(job.id);
      else openDetails.delete(job.id);
    });
    main.append(details);
  }

  const actions = document.createElement("div");
  actions.className = "hrow-actions";
  const folder = iconOnlyButton("folder", "Open folder");
  folder.onclick = (event) => {
    event.stopPropagation();
    invoke("reveal", { path: historyPath(job) }).catch((error) => toast(String(error), true));
  };
  actions.append(folder, overflowMenu(historyMenuItems(job, outcome)));

  row.append(status, main, actions);
  row.addEventListener("click", (event) => {
    // A click in the details pane is someone reading it, not picking a row.
    if (event.target.closest(".hdetails")) return;
    historySelected = historySelected === job.id ? null : job.id;
    renderHistory();
  });
  return row;
}

function renderHistory() {
  closeMenu();
  const box = $("history");

  if (!historyRows.length) {
    box.replaceChildren(empty("Nothing has been downloaded yet."));
    return;
  }

  const shown = historyRows.filter((job) => {
    if (historyOutcome !== "all" && outcomeOf(job) !== historyOutcome) return false;
    if (!historyQuery) return true;
    // The address as well as the name: a file saved under a name the server
    // chose is often remembered by where it came from.
    return (
      job.filename.toLowerCase().includes(historyQuery) ||
      job.url.toLowerCase().includes(historyQuery)
    );
  });

  if (!shown.length) {
    box.replaceChildren(empty("Nothing here matches."));
    return;
  }

  const days = new Map();
  for (const job of shown) {
    const key = startOfDay(job.at_ms);
    if (!days.has(key)) days.set(key, []);
    days.get(key).push(job);
  }

  const groups = [...days.entries()]
    .sort(([a], [b]) => b - a)
    .map(([day, jobs]) => {
      const group = document.createElement("details");
      group.className = "hgroup";
      group.open = dayIsOpen(day);

      const summary = document.createElement("summary");
      const chev = icon("chevron");
      chev.classList.add("chev");
      const heading = document.createElement("span");
      heading.textContent = dayHeading(day);

      // Said only while the rows cannot be seen: a count beside a list you
      // are looking at is a number you can already see.
      const count = document.createElement("span");
      count.className = "hcount";
      count.textContent = `${jobs.length} download${jobs.length === 1 ? "" : "s"}`;
      count.hidden = group.open;

      summary.append(chev, heading, count);
      group.append(summary);

      const rows = document.createElement("div");
      rows.className = "hgroup-rows";
      rows.append(...jobs.map(historyRow));
      group.append(rows);

      // Hiding the count rather than rebuilding: a re-render here would take
      // the focus off the heading the user just pressed.
      group.addEventListener("toggle", () => {
        count.hidden = group.open;
        if (group.open) {
          daysOpened.add(day);
          daysClosed.delete(day);
        } else {
          daysClosed.add(day);
          daysOpened.delete(day);
        }
      });
      return group;
    });

  box.replaceChildren(...groups);
}

async function showHistory() {
  try {
    // The archive is kept for good, so this is a ceiling rather than a page:
    // five hundred rows is more than the window can usefully scroll and the
    // search field is the way past it.
    historyRows = await invoke("history", { limit: 500 });
    renderHistory();
  } catch (error) {
    toast(String(error), true);
  }
}

// ------------------------------------------------------- looking inside

/**
 * Whether an address looks like an archive this can list.
 *
 * On the address alone, because the whole point is deciding before
 * fetching anything -- asking the server what it is would be a request,
 * and one that tells it somebody is interested.
 */
function looksLikeAnArchive(url) {
  try {
    return new URL(url).pathname.toLowerCase().endsWith(".zip");
  } catch {
    return false;
  }
}

/** Whether an address is a web page this could read for links. */
function looksLikeAWebAddress(url) {
  try {
    return ["http:", "https:"].includes(new URL(url).protocol);
  } catch {
    return false;
  }
}

function showPeekButton() {
  const url = $("url").value.trim();
  $("peek").hidden = !looksLikeAnArchive(url);
  $("grab").hidden = !looksLikeAWebAddress(url);
}

// ------------------------------------------------------- the link picker

/**
 * The last grab, and which of its links are ticked.
 *
 * Kept outside the render so that changing the filter, or going a level
 * deeper, does not lose what was already ticked.
 */
let grabbed = { links: [], chosen: new Set(), url: "", pages: 0, note: null };

$("grab").addEventListener("click", () => {
  const url = $("url").value.trim();
  if (!url) return;
  grabbed = { links: [], chosen: new Set(), url, pages: 0, note: null };
  runGrab(url, "this_page", $("grab"));
});

$("links-deeper").addEventListener("click", () => {
  // The ticks survive, because the deeper grab is a superset: everything
  // from the first page is found again.
  runGrab(grabbed.url, "and_what_it_links_to", $("links-deeper"));
});

async function runGrab(url, follow, button) {
  const label = button.querySelector("span") ?? button;
  const was = label.textContent;
  label.textContent = follow === "this_page" ? "Reading…" : "Reading the pages…";
  button.disabled = true;

  try {
    const result = await invoke("grab_links", { url, follow });
    grabbed.url = url;
    grabbed.links = result.links ?? [];
    grabbed.pages = result.pages ?? 0;
    grabbed.note = result.note ?? null;
    // Only the ones still present stay ticked.
    const present = new Set(grabbed.links.map((link) => link.url));
    grabbed.chosen = new Set([...grabbed.chosen].filter((url) => present.has(url)));
    // A deeper grab reads nothing new when the page linked to no pages, and
    // offering it again would just read them again.
    $("links-deeper").hidden = follow !== "this_page";
    openLinks();
  } catch (error) {
    toast(String(error), true);
  } finally {
    label.textContent = was;
    button.disabled = false;
  }
}

/** Which kind a link is filed under, for the filter and the row. */
function kindOf(link) {
  return link.category ?? "Other";
}

function openLinks() {
  const kinds = [...new Set(grabbed.links.map(kindOf))].sort();
  const picker = $("links-kind");
  const keep = kinds.includes(picker.value) ? picker.value : "";
  picker.replaceChildren();
  const all = document.createElement("option");
  all.value = "";
  all.textContent = "Everything";
  picker.append(all);
  for (const kind of kinds) {
    const option = document.createElement("option");
    option.value = kind;
    option.textContent = kind;
    picker.append(option);
  }
  picker.value = keep;

  renderLinks();

  const overlay = $("links-modal");
  if (!overlay.hidden) return;

  const close = $("links-close");
  const add = $("links-add");
  const every = $("links-every");

  const finish = () => {
    overlay.hidden = true;
    close.removeEventListener("click", onClose);
    add.removeEventListener("click", onAdd);
    every.removeEventListener("change", onEvery);
    picker.removeEventListener("change", renderLinks);
    overlay.removeEventListener("keydown", onKey);
  };
  const onClose = () => finish();
  const onEvery = () => {
    for (const link of shownLinks()) {
      if (every.checked) grabbed.chosen.add(link.url);
      else grabbed.chosen.delete(link.url);
    }
    renderLinks();
  };
  const onAdd = async () => {
    const urls = [...grabbed.chosen];
    if (urls.length === 0) {
      toast("Nothing is ticked.", true);
      return;
    }
    add.disabled = true;
    try {
      const queue = $("queue-pick").value;
      const queued = await invoke("add_links", {
        urls,
        queue: queue === "" ? null : Number(queue),
      });
      finish();
      toast(`Started ${queued} download${queued === 1 ? "" : "s"}.`);
      // The address box held a page, not a download. Leaving it there
      // invites pressing Download on it next.
      $("url").value = "";
      showPeekButton();
    } catch (error) {
      toast(String(error), true);
    } finally {
      add.disabled = false;
    }
  };
  const onKey = (event) => {
    if (event.key === "Escape") onClose();
  };

  close.addEventListener("click", onClose);
  add.addEventListener("click", onAdd);
  every.addEventListener("change", onEvery);
  picker.addEventListener("change", renderLinks);
  overlay.addEventListener("keydown", onKey);
  overlay.hidden = false;
  close.focus();
}

/** The links the filter is letting through. */
function shownLinks() {
  const kind = $("links-kind").value;
  return kind === "" ? grabbed.links : grabbed.links.filter((link) => kindOf(link) === kind);
}

function renderLinks() {
  const list = $("links-list");
  list.replaceChildren();
  const shown = shownLinks();

  for (const link of shown) {
    const row = document.createElement("label");
    row.className = "link-row";

    const tick = document.createElement("input");
    tick.type = "checkbox";
    tick.checked = grabbed.chosen.has(link.url);
    tick.addEventListener("change", () => {
      if (tick.checked) grabbed.chosen.add(link.url);
      else grabbed.chosen.delete(link.url);
      updateLinkCount();
    });

    const what = document.createElement("span");
    what.className = "link-what";
    const text = document.createElement("span");
    text.className = "link-text";
    // The address when the link had no words of its own, which is most
    // image sources and plenty of anchors.
    text.textContent = link.text || link.url;
    const url = document.createElement("span");
    url.className = "link-url";
    url.textContent = link.url;
    what.append(text, url);

    const kind = document.createElement("small");
    kind.className = "link-kind";
    kind.textContent = kindOf(link);

    row.append(tick, what, kind);
    list.append(row);
  }

  if (shown.length === 0) {
    list.append(empty("No links of that kind."));
  }

  const pages = grabbed.pages === 1 ? "page" : "pages";
  const counted = `${grabbed.links.length} link${grabbed.links.length === 1 ? "" : "s"}`;
  $("links-message").textContent = grabbed.note
    ? `${counted} on ${grabbed.pages} ${pages} — ${grabbed.note}.`
    : `${counted} on ${grabbed.pages} ${pages}.`;

  // Ticked when everything shown is ticked, and only then.
  $("links-every").checked =
    shown.length > 0 && shown.every((link) => grabbed.chosen.has(link.url));
  updateLinkCount();
}

function updateLinkCount() {
  const count = grabbed.chosen.size;
  $("links-add").textContent =
    count === 0 ? "Download the ticked ones" : `Download ${count}`;
  $("links-every").checked =
    shownLinks().length > 0 && shownLinks().every((link) => grabbed.chosen.has(link.url));
}

$("url").addEventListener("input", showPeekButton);

$("peek").addEventListener("click", async () => {
  const url = $("url").value.trim();
  if (!url) return;

  const button = $("peek");
  const label = button.querySelector("span");
  const was = label.textContent;
  label.textContent = "Looking…";
  button.disabled = true;

  try {
    const inside = await invoke("peek_archive", { url });
    showArchive(url, inside);
  } catch (error) {
    toast(String(error), true);
  } finally {
    label.textContent = was;
    button.disabled = false;
  }
});

function showArchive(url, inside) {
  const list = $("archive-list");
  list.replaceChildren();

  const entries = inside.entries ?? [];
  for (const entry of entries) {
    const row = document.createElement("div");
    row.className = "archive-row";

    const name = document.createElement("span");
    name.className = entry.is_directory ? "archive-name is-folder" : "archive-name";
    name.textContent = entry.name;

    const spacer = document.createElement("span");
    spacer.className = "spacer";

    const size = document.createElement("small");
    // A folder has no size of its own, and showing "0 B" for one reads as
    // an empty file rather than as a folder.
    size.textContent = entry.is_directory ? "" : bytes(entry.uncompressed);

    row.append(name, spacer, size);
    list.append(row);
  }

  if (entries.length === 0) {
    list.append(empty("There is nothing in this archive."));
  }

  const files = entries.filter((entry) => !entry.is_directory).length;
  $("archive-message").textContent = inside.truncated
    ? `The first ${files} files in this archive — there are more — ${bytes(inside.total_uncompressed)} unpacked so far.`
    : `${files} file${files === 1 ? "" : "s"} in this archive, ${bytes(inside.total_uncompressed)} unpacked.`;

  const overlay = $("archive-modal");
  const close = $("archive-close");
  const download = $("archive-download");

  const finish = () => {
    overlay.hidden = true;
    close.removeEventListener("click", onClose);
    download.removeEventListener("click", onDownload);
    overlay.removeEventListener("keydown", onKey);
  };
  const onClose = () => finish();
  const onDownload = () => {
    finish();
    // Straight into the ordinary path, with the address already in the
    // box: having just looked inside, being made to find the button again
    // would be absurd.
    $("add").requestSubmit();
  };
  const onKey = (event) => {
    if (event.key === "Escape") onClose();
  };

  close.addEventListener("click", onClose);
  download.addEventListener("click", onDownload);
  overlay.addEventListener("keydown", onKey);
  overlay.hidden = false;
  close.focus();
  // Named so the dialog says which archive, since the address box behind
  // it is covered.
  void url;
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

$("add").addEventListener("submit", async (event) => {
  event.preventDefault();
  const field = $("url");
  const url = field.value.trim();
  if (!url) return;
  const box = $("mirrors");
  const mirrors = box.value
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  try {
    const chosen = $("queue-pick").value;
    const queued = await invoke("add_download", {
      url,
      filename: null,
      mirrors,
      queue: chosen === "" ? null : Number(chosen),
    });
    field.value = "";
    // The box is empty now, so the button for what was in it should go
    // with it rather than offering to look inside nothing.
    showPeekButton();
    // Cleared along with the address, since they belonged to it. Left
    // behind, they would silently attach to whatever was typed next.
    box.value = "";
    $("mirrors-wrap").open = false;
    // Said only when one address stood for several, since otherwise the
    // download appearing in the list below says it already.
    if (queued > 1) toast(`Queued ${queued} downloads`);
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
  for (const name of ["active", "queues", "schedule", "history", "settings"]) {
    $(`panel-${name}`).hidden = name !== tab.dataset.tab;
  }
  if (tab.dataset.tab === "history") showHistory();
  // The counts move as downloads start and finish, so they are read when the
  // panel is opened rather than left at whatever they were last time.
  if (tab.dataset.tab === "queues") refreshQueues();
  // Read when the panel is opened rather than on the snapshot: rules change
  // rarely, and what is in force only changes on a boundary.
  if (tab.dataset.tab === "schedule") refreshSchedule();
  // A browser may have been installed or paired since this was last looked
  // at, and ffmpeg may have arrived on the PATH from somewhere else.
  if (tab.dataset.tab === "settings") {
    refreshBrowsers();
    refreshSidecars();
    refreshCredentials();
  }
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
    showPeekButton();
  } catch (error) {
    toast(String(error), true);
  }
});

$("sidebar-folder").addEventListener("click", () => {
  selectTab($("tab-settings"));
  $("choose-folder").focus();
});

// Whether an address is a torrent, which changes a couple of words in the
// list. Deliberately a rough test: the engine makes the real decision, and
// the worst a wrong guess does here is label a row "connections" instead of
// "peers".
function isTorrent(url) {
  if (!url) return false;
  const address = url.trim();
  if (/^magnet:/i.test(address)) return true;
  return /\.torrent$/i.test(address.split(/[?#]/)[0]);
}

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

// Not in the list above: choosing a mode that needs an address reveals a box
// that is still empty, and the engine rightly refuses an empty one. Saving on
// the spot would answer a half-finished action with an error, so the save
// waits until there is something to save. Both "manual" and "pac" are in that
// position -- one wants a proxy, the other a script, and neither has it yet.
const NEEDS_ADDRESS = ["manual", "pac"];

$("proxy-mode").addEventListener("change", () => {
  showProxyAddress();
  if (NEEDS_ADDRESS.includes($("proxy-mode").value) && !$("proxy-address").value.trim()) {
    $("proxy-address").focus();
    return;
  }
  saveSettings();
});

$("proxy-address").addEventListener("change", () => {
  if (!NEEDS_ADDRESS.includes($("proxy-mode").value) || !$("proxy-address").value.trim()) return;
  saveSettings();
});

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
    // Only when there is somewhere to keep the answer. The box was shown
    // either way, so ticking it on a one-off question silently did nothing.
    $("confirm-remember-row").hidden = !rememberKey;
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

/**
 * Asks for a value and resolves to it, or to null if it was not given.
 *
 * The window's own dialog rather than the browser's prompt box, which
 * cannot be styled, cannot say what shape the answer should take, and is
 * a poor place to paste sixty-four characters into.
 */
function askFor(message, { placeholder = "", value = "", confirm = "OK" } = {}) {
  return new Promise((resolve) => {
    const overlay = $("ask-modal");
    const field = $("ask-input");
    const okBtn = $("ask-ok");
    const cancelBtn = $("ask-cancel");

    $("ask-message").textContent = message;
    field.placeholder = placeholder;
    field.value = value;
    okBtn.textContent = confirm;
    overlay.hidden = false;
    field.focus();
    field.select();

    const finish = (answer) => {
      overlay.hidden = true;
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      field.removeEventListener("keydown", onKey);
      resolve(answer);
    };
    const onOk = () => finish(field.value.trim() || null);
    const onCancel = () => finish(null);
    // Enter and Escape, because a dialog with one field is a dialog
    // nobody should have to reach for the mouse to finish.
    const onKey = (event) => {
      if (event.key === "Enter") onOk();
      else if (event.key === "Escape") onCancel();
    };

    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    field.addEventListener("keydown", onKey);
  });
}

/** Just the host of an address, which is the part worth reading aloud. */
function sourceLabel(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}

/**
 * Checks a finished file against a hash published where the link was.
 *
 * Worth saying plainly either way: a match is the only evidence there is
 * that the bytes are the publisher's and not a mirror's older build, and
 * a mismatch is worth more than a red toast -- so it reports what the
 * file actually hashes to.
 *
 * The hash is looked for before it is asked for. Being made to go and find
 * the figure by hand is the whole reason this check normally goes unused,
 * and it is usually published a few metres from the link. Where it was found
 * is always said, because a hash is worth exactly what its source is worth
 * and a figure compared against silently could have come from anywhere.
 */
async function verifyDownload(job) {
  const path = `${job.output_dir}\\${job.filename}`;

  toast("Looking for a published hash…");
  let found = null;
  try {
    found = await invoke("published_hash", {
      url: job.url,
      filename: job.filename,
      referrer: job.referrer ?? null,
    });
  } catch {
    // A lookup that fell over is not a reason to refuse the check; the
    // paste path below does not need it.
  }

  const expected =
    found?.hash ??
    (await askFor(`Paste the hash published for ${job.filename}:`, {
      placeholder: "SHA-256, SHA-1 or MD5",
      confirm: "Check it",
    }));
  if (!expected) return;

  const where = found ? sourceLabel(found.source) : null;
  toast(where ? `Found one at ${where}. Checking…` : "Checking…");
  try {
    const verdict = await invoke("verify_file", { path, expected });
    const algorithm = verdict.algorithm.toUpperCase();
    if (verdict.matched) {
      toast(where ? `Matches the ${algorithm} published at ${where}.` : `Matches the published ${algorithm}.`);
    } else {
      toast(`Does not match. This file is ${verdict.actual}`, true);
    }
  } catch (error) {
    toast(String(error), true);
  }
}

for (const chip of document.querySelectorAll(".filter-chip")) {
  chip.addEventListener("click", () => {
    historyOutcome = chip.dataset.outcome;
    for (const other of document.querySelectorAll(".filter-chip")) {
      other.setAttribute("aria-pressed", String(other === chip));
    }
    renderHistory();
  });
}

$("history-search").addEventListener("input", (event) => {
  historyQuery = event.target.value.trim().toLowerCase();
  renderHistory();
});

// One menu open at a time, and anything else closes it.
document.addEventListener("click", closeMenu);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeMenu();
});

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

/**
 * Saves a password for a stopped download's site, then starts it again.
 *
 * The site is filled in from the job, because the person is looking at the
 * download rather than at a list of sites and should not have to copy an
 * address between the two. The resume is part of the same action for the
 * same reason: having just supplied what was missing, being left with a
 * still-failed download and a second button to find would be absurd.
 */
async function signInFor(job) {
  // The site, not the address. One password covers the whole site, which is
  // what the engine stores and what the list will show, so naming the exact
  // URL here would promise something narrower than what is saved.
  let site = job.url;
  try {
    site = new URL(job.url).origin;
  } catch {
    // An address this cannot parse is one the engine will refuse anyway,
    // and showing it whole is better than showing nothing.
  }
  const answer = await askForCredential({
    scope: { kind: "site", url: job.url },
    username: "",
    label: site,
    // Nothing is being replaced: this is the first password for this site,
    // reached from a download that has just been refused one.
    replacing: false,
  });
  if (!answer) return;
  try {
    credentialRows = await invoke("save_credential", answer);
  } catch (error) {
    toast(String(error), true);
    return;
  }
  renderCredentials();
  // From the History row rather than a live job id: a download is taken off
  // the active list the moment it fails, so by the time anybody can reach
  // this there is no live job left to resume. The bytes and the journal are
  // still on disk, so this carries on rather than starting over.
  try {
    await invoke("resume_from_history", { id: job.id });
    toast(`Signed in and picked ${job.filename} up again`);
  } catch (error) {
    // The password was still saved, which is worth saying: the next
    // attempt will use it even though this one could not start.
    toast(`Password saved, but it could not be started again: ${error}`, true);
  }
  await showHistory();
}

// --------------------------------------------------------------- schedule

// Monday first, matching the bitmask the engine uses: bit 0 is Monday,
// because a rule somebody writes is nearly always about a working week.
const DAY_NAMES = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

let scheduleRules = [];
let scheduleNow = null;

async function refreshSchedule() {
  try {
    const reply = await invoke("bandwidth_rules");
    scheduleRules = reply.rules ?? [];
    scheduleNow = reply.now ?? null;
  } catch (error) {
    scheduleRules = [];
    scheduleNow = null;
    toast(String(error), true);
  }
  renderSchedule();
}

/** Minutes past midnight as somebody would read them back. */
function timeOfDay(minute) {
  return `${String(Math.floor(minute / 60)).padStart(2, "0")}:${String(minute % 60).padStart(2, "0")}`;
}

/** The days a mask names, shortened where they run together. */
function daysOf(mask) {
  const on = DAY_NAMES.filter((_, index) => mask & (1 << index));
  if (on.length === 7) return "Every day";
  if (mask === 0b0001_1111) return "Weekdays";
  if (mask === 0b0110_0000) return "Weekends";
  return on.join(", ");
}

function renderSchedule() {
  const now = $("schedule-now");
  now.classList.toggle("is-stopped", Boolean(scheduleNow?.stopped));
  now.replaceChildren();

  if (scheduleNow) {
    const headline = document.createElement("div");
    headline.textContent = scheduleNow.stopped
      ? "Downloads are being held back right now."
      : scheduleNow.bytes_per_sec
        ? `Downloads are limited to ${bytes(scheduleNow.bytes_per_sec)}/s right now.`
        : "Downloads are running at full speed right now.";

    const why = document.createElement("small");
    const parts = [
      scheduleNow.from_a_rule ? "Decided by a rule below." : "No rule applies, so Settings decides.",
    ];
    // Only when something is actually scheduled: with no rules at all this
    // would be a promise that nothing is going to happen.
    if (scheduleNow.changes_in != null) {
      parts.push(`Changes in ${describeMinutes(scheduleNow.changes_in)}.`);
    }
    why.textContent = parts.join(" ");

    now.append(headline, why);
  }

  const list = $("schedule-list");
  list.replaceChildren(
    ...(scheduleRules.length
      ? scheduleRules.map(ruleRow)
      : [empty("Nothing scheduled. Downloads use the speed limit in Settings.")]),
  );
}

/** A gap in minutes, said the way a person would say it. */
function describeMinutes(minutes) {
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 ? `${hours} h ${minutes % 60} min` : `${hours} h`;
  const days = Math.floor(hours / 24);
  return days === 1 ? "a day" : `${days} days`;
}

function ruleRow(rule) {
  const wrap = document.createElement("div");
  wrap.className = "browser";

  const line = document.createElement("div");
  line.className = "browser-row";

  const dot = document.createElement("span");
  dot.className = rule.stop ? "dot" : "dot good";
  dot.setAttribute("aria-hidden", "true");

  const when = document.createElement("span");
  when.className = "browser-name";
  // An end before a start runs past midnight, which the engine reads the
  // same way; saying so here saves somebody wondering if it is a typo.
  const overnight = rule.end_minute < rule.start_minute ? " (overnight)" : "";
  when.textContent = `${daysOf(rule.days)}  ${timeOfDay(rule.start_minute)}–${timeOfDay(rule.end_minute)}${overnight}`;

  const what = document.createElement("span");
  what.className = "browser-status";
  what.textContent = rule.stop
    ? "do not download"
    : rule.bytes_per_sec
      ? `${bytes(rule.bytes_per_sec)}/s`
      : "full speed";

  const spacer = document.createElement("span");
  spacer.className = "spacer";
  line.append(dot, when, what, spacer);

  const edit = document.createElement("button");
  edit.className = "ghost";
  edit.textContent = "Edit";
  edit.onclick = () => editRule(rule);
  line.append(edit);

  const remove = document.createElement("button");
  remove.className = "ghost";
  remove.textContent = "Remove";
  remove.onclick = () => removeRule(rule);
  line.append(remove);

  wrap.append(line);
  return wrap;
}

/** The editor. Resolves to null when cancelled. */
function askForRule(existing) {
  return new Promise((resolve) => {
    const overlay = $("rule-modal");
    const start = $("rule-start");
    const end = $("rule-end");
    const action = $("rule-action");
    const speed = $("rule-speed");
    const okBtn = $("rule-ok");
    const cancelBtn = $("rule-cancel");

    // Rebuilt each time rather than wired once, so the ticked days are
    // whatever this rule has rather than whatever the last one left.
    const days = $("rule-days");
    days.replaceChildren();
    const boxes = DAY_NAMES.map((name, index) => {
      const label = document.createElement("label");
      const box = document.createElement("input");
      box.type = "checkbox";
      box.checked = Boolean((existing?.days ?? 0b0001_1111) & (1 << index));
      label.classList.toggle("is-on", box.checked);
      box.addEventListener("change", () => label.classList.toggle("is-on", box.checked));
      label.append(box, document.createTextNode(name));
      days.append(label);
      return box;
    });

    start.value = timeOfDay(existing?.start_minute ?? 9 * 60);
    end.value = timeOfDay(Math.min(existing?.end_minute ?? 17 * 60, 23 * 60 + 59));
    action.value = existing ? (existing.stop ? "stop" : existing.bytes_per_sec ? "limit" : "open") : "limit";
    if (existing?.bytes_per_sec) speed.value = String(existing.bytes_per_sec);
    $("rule-message").textContent = existing ? "Edit this rule" : "Add a rule";

    const showAction = () => {
      $("rule-speed-row").hidden = action.value !== "limit";
      const from = minutesOf(start.value);
      const to = minutesOf(end.value);
      $("rule-overnight").hidden = !(from != null && to != null && to < from);
    };
    showAction();
    action.addEventListener("change", showAction);
    start.addEventListener("change", showAction);
    end.addEventListener("change", showAction);

    overlay.hidden = false;

    const finish = (answer) => {
      overlay.hidden = true;
      action.removeEventListener("change", showAction);
      start.removeEventListener("change", showAction);
      end.removeEventListener("change", showAction);
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      overlay.removeEventListener("keydown", onKey);
      resolve(answer);
    };
    const onOk = () => {
      const mask = boxes.reduce((into, box, index) => into | (box.checked << index), 0);
      // Camel case, because that is what Tauri expects a command's
      // arguments to be called however they are spelled in Rust. The rules
      // coming back the other way are plain serde and stay snake case,
      // which is why only this one direction is converted.
      finish({
        id: existing?.id ?? null,
        days: mask,
        startMinute: minutesOf(start.value) ?? 0,
        endMinute: minutesOf(end.value) ?? 0,
        bytesPerSec: action.value === "limit" ? Number(speed.value) : 0,
        stop: action.value === "stop",
      });
    };
    const onCancel = () => finish(null);
    const onKey = (event) => {
      if (event.key === "Enter") onOk();
      else if (event.key === "Escape") onCancel();
    };

    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    overlay.addEventListener("keydown", onKey);
  });
}

/** "09:30" as minutes past midnight, or null when it is not a time. */
function minutesOf(value) {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value ?? "");
  if (!match) return null;
  return Number(match[1]) * 60 + Number(match[2]);
}

async function editRule(existing) {
  const answer = await askForRule(existing);
  if (!answer) return;
  try {
    const reply = await invoke("set_bandwidth_rule", answer);
    scheduleRules = reply.rules ?? [];
    scheduleNow = reply.now ?? null;
  } catch (error) {
    toast(String(error), true);
  }
  renderSchedule();
}

async function removeRule(rule) {
  const sure = await confirmDialog(
    `Remove the rule for ${daysOf(rule.days)}, ${timeOfDay(rule.start_minute)}–${timeOfDay(rule.end_minute)}?`,
    { confirm: "Remove it" },
  );
  if (!sure) return;
  try {
    const reply = await invoke("forget_bandwidth_rule", { id: rule.id });
    scheduleRules = reply.rules ?? [];
    scheduleNow = reply.now ?? null;
  } catch (error) {
    toast(String(error), true);
  }
  renderSchedule();
}

$("add-rule").addEventListener("click", () => editRule(null));

// -------------------------------------------------------- saved passwords

// What the engine says is saved. Never contains a password: the reply type
// has no field for one.
let credentialRows = [];

async function refreshCredentials() {
  try {
    credentialRows = await invoke("credentials");
  } catch (error) {
    credentialRows = [];
    toast(String(error), true);
  }
  renderCredentials();
}

function renderCredentials() {
  const list = $("password-list");
  list.replaceChildren(
    ...(credentialRows.length
      ? credentialRows.map(credentialRow)
      : [empty("No passwords saved. Sites and proxies that ask for one can be added here.")]),
  );
}

function credentialRow(entry) {
  const wrap = document.createElement("div");
  wrap.className = "browser";

  const line = document.createElement("div");
  line.className = "browser-row";

  const dot = document.createElement("span");
  dot.className = "dot good";
  dot.setAttribute("aria-hidden", "true");

  const name = document.createElement("span");
  name.className = "browser-name";
  name.textContent = entry.username;

  const where = document.createElement("span");
  where.className = "browser-status";
  where.textContent = entry.label;

  const spacer = document.createElement("span");
  spacer.className = "spacer";
  line.append(dot, name, where, spacer);

  const replace = document.createElement("button");
  replace.className = "ghost";
  replace.textContent = "Replace";
  replace.title = `Save a different password for ${entry.label}`;
  // Pre-filled with where and who, but never with the password: this window
  // has never been told it and asking the engine for it would be the one
  // thing this whole design is arranged to avoid.
  replace.onclick = () => addCredential(entry);
  line.append(replace);

  const forget = document.createElement("button");
  forget.className = "ghost";
  forget.textContent = "Forget";
  forget.onclick = () => forgetCredential(entry);
  line.append(forget);

  wrap.append(line);
  return wrap;
}

async function forgetCredential(entry) {
  const sure = await confirmDialog(
    `Forget the password for ${entry.label}? It is removed from Windows' Credential Manager.`,
    { confirm: "Forget it" },
  );
  if (!sure) return;
  try {
    // The reply is the fresh list, for the same reason the sidecar fetch
    // works that way: one line of it changed and nothing else did.
    credentialRows = await invoke("forget_credential", { scope: entry.scope });
    toast(`Forgot the password for ${entry.label}`);
  } catch (error) {
    toast(String(error), true);
  }
  renderCredentials();
}

/** The three-field dialog. Resolves to null when cancelled. */
function askForCredential(existing) {
  return new Promise((resolve) => {
    const overlay = $("credential-modal");
    const kind = $("credential-kind");
    const where = $("credential-where");
    const user = $("credential-user");
    const secret = $("credential-secret");
    const okBtn = $("credential-ok");
    const cancelBtn = $("credential-cancel");

    const scope = existing?.scope;
    kind.value = scope?.kind === "proxy" ? "proxy" : "site";
    where.value = scope ? (scope.url ?? scope.address ?? "") : "";
    user.value = existing?.username ?? "";
    // Always empty, on a replace as much as on a new one. There is nothing
    // to prefill it with, and a dialog that looked prefilled would be
    // claiming to show a password it does not have.
    secret.value = "";
    $("credential-message").textContent =
      existing?.replacing === false
        ? `Sign in to ${existing.label}`
        : existing
          ? `Replace the password for ${existing.label}`
          : "Save a password";

    const placeholder = () =>
      (where.placeholder =
        kind.value === "proxy" ? "http://127.0.0.1:3128" : "https://example.com");
    placeholder();
    kind.addEventListener("change", placeholder);

    overlay.hidden = false;
    (existing ? secret : where).focus();

    const finish = (answer) => {
      overlay.hidden = true;
      // Cleared on the way out, not left sitting in a detached input where
      // a later `hidden = false` would show it again.
      secret.value = "";
      kind.removeEventListener("change", placeholder);
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      overlay.removeEventListener("keydown", onKey);
      resolve(answer);
    };
    const onOk = () =>
      finish({
        scope:
          kind.value === "proxy"
            ? { kind: "proxy", address: where.value.trim() }
            : { kind: "site", url: where.value.trim() },
        username: user.value.trim(),
        password: secret.value,
      });
    const onCancel = () => finish(null);
    const onKey = (event) => {
      if (event.key === "Enter") onOk();
      else if (event.key === "Escape") onCancel();
    };

    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    overlay.addEventListener("keydown", onKey);
  });
}

async function addCredential(existing) {
  const answer = await askForCredential(existing);
  if (!answer) return;
  try {
    credentialRows = await invoke("save_credential", answer);
    toast("Saved in Windows' Credential Manager");
  } catch (error) {
    toast(String(error), true);
  }
  renderCredentials();
}

$("add-password").addEventListener("click", () => addCredential(null));

// ------------------------------------------------------------ video tools

// The external programs the engine can run, as it last described them.
let sidecarRows = [];
// Names currently being fetched, so the button stays busy and a second
// click cannot start the same hundred-megabyte download twice.
const fetching = new Set();

async function refreshSidecars() {
  // Said before the round trip, not after. Answering this means asking
  // each tool what version it is, which is a process spawn apiece -- and
  // while that was in flight the card rendered as a heading with nothing
  // under it, which reads as a broken panel rather than a busy one.
  if (sidecarRows.length === 0) {
    $("tools-list").replaceChildren(empty("Checking what is installed…"));
  }
  try {
    sidecarRows = await invoke("sidecars");
  } catch (error) {
    sidecarRows = [];
    toast(String(error), true);
  }
  renderSidecars();
}

function renderSidecars() {
  const list = $("tools-list");
  list.replaceChildren(
    ...(sidecarRows.length
      ? sidecarRows.map(sidecarRow)
      : [empty("The engine did not say which tools it can use.")]),
  );
}

function sidecarRow(tool) {
  const wrap = document.createElement("div");
  wrap.className = "browser";

  const line = document.createElement("div");
  line.className = "browser-row";

  const dot = document.createElement("span");
  dot.className = tool.present ? "dot good" : "dot";
  dot.setAttribute("aria-hidden", "true");

  const name = document.createElement("span");
  name.className = "browser-name";
  name.textContent = tool.name;

  const status = document.createElement("span");
  status.className = `browser-status ${tool.present ? "connected" : "not_connected"}`;
  // What it is, not a yes or no: a present copy may be the user's own build
  // at a different version, and that is worth seeing rather than hiding
  // behind a tick.
  status.textContent = tool.present ? (tool.detail ?? "Installed") : "Not installed";

  const spacer = document.createElement("span");
  spacer.className = "spacer";
  line.append(dot, name, status, spacer);

  const busy = fetching.has(tool.name);
  const button = document.createElement("button");
  button.className = tool.present ? "ghost" : "";
  button.disabled = busy;
  button.textContent = busy ? "Fetching…" : tool.present ? "Fetch again" : `Get it (${bytes(tool.bytes)})`;
  // The size is in the label rather than a tooltip, because it is the whole
  // of what somebody is agreeing to and a tooltip is not an agreement.
  button.title = tool.present
    ? `Download ${tool.name} ${tool.version} again and replace what is here`
    : `Download ${bytes(tool.bytes)} from the publisher, checked against the build DivinePulse pins`;
  button.onclick = () => getSidecar(tool);
  line.append(button);

  wrap.append(line);

  const why = document.createElement("div");
  why.className = "browser-note";
  // Both halves: what it is for, and where the copy in use came from. An
  // absent tool's `detail` is the refusal, which is the useful line there.
  why.textContent = tool.present && tool.path ? `For ${tool.why}. ${tool.path}` : `For ${tool.why}.`;
  wrap.append(why);

  return wrap;
}

async function getSidecar(tool) {
  if (fetching.has(tool.name)) return;

  const sure = await confirmDialog(
    `Download ${tool.name} ${tool.version} — ${bytes(tool.bytes)} — and install it for DivinePulse to use? ` +
      `It is fetched from its publisher and refused unless it matches the build this app pins.`,
    { confirm: "Download it" },
  );
  if (!sure) return;

  fetching.add(tool.name);
  renderSidecars();
  try {
    // The reply is the fresh list: a fetch changes exactly one line of it,
    // and asking again separately would show a list from another instant.
    sidecarRows = await invoke("fetch_sidecar", { name: tool.name });
    toast(`${tool.name} is installed`);
  } catch (error) {
    toast(String(error), true);
  } finally {
    fetching.delete(tool.name);
    renderSidecars();
  }
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

  // Started, not announced. This used to raise a notice saying BitTorrent
  // was not supported; now the only reason a double-clicked `.torrent` does
  // not simply begin is that something went wrong, and that is what the
  // notice is for.
  const box = $("notices");
  for (const path of files) {
    const name = path.split(/[\\/]/).pop();
    try {
      await invoke("add_download", { url: path, filename: null });
      continue;
    } catch (error) {
      const note = document.createElement("div");
      note.className = "notice";

      const text = document.createElement("p");
      const strong = document.createElement("strong");
      // textContent: a file name is chosen by whoever made the file.
      strong.textContent = name;
      text.append(strong, ` could not be started: ${error}`);

      const dismiss = document.createElement("button");
      dismiss.className = "ghost";
      dismiss.textContent = "Dismiss";
      dismiss.onclick = () => {
        note.remove();
        box.hidden = box.childElementCount === 0;
      };

      note.append(text, dismiss);
      box.append(note);
      box.hidden = false;
    }
  }
  refresh();
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
// Read at startup, not only when the panel is opened: the picker in the add
// form and the select on every job card both need the list before anybody
// visits the Queues tab.
refreshQueues();
