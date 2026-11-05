// Popup: shows what the engine thinks, and edits the few settings that live
// on the browser side. Anything that matters is decided by the engine, so
// this never pretends to be the authority.

const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);

function ask(message) {
  return new Promise((resolve) => api.runtime.sendMessage(message, resolve));
}

function render({ state = {}, settings = {} }) {
  const box = $("status");
  if (state.unavailable) {
    box.className = "status bad";
    // Usually the app simply is not running, so that stays the headline.
    // But the browser's own reason is shown underneath, because when the
    // app IS running this message is actively wrong and sends people to
    // check the one thing that is already fine.
    //
    // It cost hours once: the app was running the whole time, the engine,
    // host, registry entry and manifest all checked out by hand, and this
    // line still said "not running". The real reason -- the string Chrome
    // puts in runtime.lastError, such as "Specified native messaging host
    // not found" -- was known to the extension and shown to nobody.
    box.textContent = "DivinePulse is not reachable.";
    if (state.reason) {
      const detail = document.createElement("div");
      detail.className = "detail";
      // textContent: this string comes from the browser, and the popup must
      // not become a place where it can write markup.
      detail.textContent = state.reason;
      box.append(detail);
    }
    $("pair").hidden = true;
  } else if (state.integrationEnabled === false) {
    box.className = "status warn";
    box.textContent = "Browser integration is switched off inside DivinePulse.";
    $("pair").hidden = true;
  } else if (state.paired === false) {
    box.className = "status warn";
    box.textContent = "Waiting for you to approve this browser in DivinePulse.";
    $("pair").hidden = false;
  } else {
    box.className = "status ok";
    box.textContent = "Connected and paired.";
    $("pair").hidden = true;
  }

  if (settings.enabled !== undefined) {
    $("enabled").checked = settings.enabled;
    $("enabled-state").textContent = settings.enabled ? "On" : "Off";
  }
  if (settings.minSizeBytes !== undefined) {
    $("minSize").value = Math.round(settings.minSizeBytes / (1024 * 1024));
  }
  if (settings.forwardCookies !== undefined) {
    $("forwardCookies").checked = settings.forwardCookies;
  }
  if (settings.findStreams !== undefined) {
    $("findStreams").checked = settings.findStreams;
  }
}

/** The last part of an address, which is the only readable part of most. */
function fileOf(url) {
  try {
    return new URL(url).pathname.split("/").filter(Boolean).pop() || url;
  } catch {
    return url;
  }
}

/**
 * Lists the streams noticed on the page in front of you.
 *
 * Every string here came off a web page, so all of it goes in through
 * `textContent`. The popup is a privileged context and must not become a
 * place where a page's own text can write markup.
 */
function renderStreams({ streams = [], pageUrl } = {}) {
  const list = $("stream-list");
  list.replaceChildren();
  $("streams").hidden = streams.length === 0;

  for (const stream of streams) {
    const row = document.createElement("li");

    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = stream.kind;

    const where = document.createElement("span");
    where.className = "where";
    where.textContent = fileOf(stream.url);
    where.title = stream.url;

    const grab = document.createElement("button");
    grab.className = "grab";
    grab.textContent = "Grab";
    grab.addEventListener("click", async () => {
      grab.disabled = true;
      grab.textContent = "…";
      const reply = await ask({ type: "grabStream", url: stream.url, pageUrl });
      // Said on the button itself: the popup usually closes before any
      // other kind of notice could be read.
      grab.textContent = reply?.ok ? "Sent" : "Failed";
      if (!reply?.ok) grab.title = reply?.reason ?? "";
    });

    row.append(kind, where, grab);
    list.append(row);
  }
}

$("enabled").addEventListener("change", async (event) => {
  // The word follows the switch immediately, before the round trip to the
  // background worker, so the control never says one thing while showing
  // another.
  $("enabled-state").textContent = event.target.checked ? "On" : "Off";
  await ask({ type: "setSettings", settings: { enabled: event.target.checked } });
});

$("minSize").addEventListener("change", async (event) => {
  const mb = Math.max(0, Number(event.target.value) || 0);
  await ask({ type: "setSettings", settings: { minSizeBytes: mb * 1024 * 1024 } });
});

$("forwardCookies").addEventListener("change", async (event) => {
  if (event.target.checked) {
    // Permission is requested at the moment of use, not at install.
    const { granted } = await ask({ type: "requestCookieAccess" });
    event.target.checked = Boolean(granted);
    if (!granted) return;
  } else {
    await ask({ type: "setSettings", settings: { forwardCookies: false } });
  }
});

$("findStreams").addEventListener("change", async (event) => {
  if (event.target.checked) {
    // Seeing page requests is asked for here, not at install time.
    const { granted } = await ask({ type: "requestStreamAccess" });
    event.target.checked = Boolean(granted);
    if (!granted) return;
    renderStreams(await ask({ type: "getStreams" }));
  } else {
    await ask({ type: "setSettings", settings: { findStreams: false } });
  }
});

/**
 * The overlay switch, which is about this one site rather than a setting.
 *
 * Disabled where there is no site to switch it on for -- a new tab, a
 * settings page, a PDF -- because a checkbox that silently does nothing is
 * worse than one that is visibly not available.
 */
async function renderOverlay(reply) {
  const box = $("overlayHere");
  const hint = $("overlay-hint");
  box.checked = Boolean(reply?.on);
  box.disabled = !reply?.pattern;
  hint.textContent = reply?.pattern
    ? "Asks for access to this one site. Nothing runs on any other."
    : "There is no site in this tab to switch this on for.";
}

$("overlayHere").addEventListener("change", async (event) => {
  if (event.target.checked) {
    const reply = await ask({ type: "overlayHere" });
    event.target.checked = Boolean(reply?.ok);
    if (!reply?.ok && reply?.reason) $("overlay-hint").textContent = reply.reason;
  } else {
    await ask({ type: "overlayOff" });
  }
  renderOverlay(await ask({ type: "overlayState" }));
});

$("pair").addEventListener("click", async () => {
  await ask({ type: "pair" });
  render(await ask({ type: "getStatus" }));
});

ask({ type: "getStatus" }).then(render);
ask({ type: "getStreams" }).then(renderStreams);
ask({ type: "overlayState" }).then(renderOverlay);
