// DivinePulse browser integration.
//
// One source file for every browser. Chromium and Gecko differ only in the
// manifest and in whether this runs as a service worker or an event page, so
// the logic lives here once and the build copies it into both.
//
// # The service worker sleeps
//
// MV3 stops the background script whenever it is idle, which breaks the
// obvious design of "set up state on startup and keep it in a variable".
// Every listener is therefore registered at the top level, on every wake, and
// anything that must survive a nap goes to chrome.storage. A module-level
// variable here is a cache, never the source of truth.
//
// # Cookies are opt-in
//
// Reading your cookies is what makes a login-gated download work, and it is
// also the most invasive thing this extension could do. So `cookies` and host
// access are *optional* permissions, requested only if you turn the feature
// on. Most downloader extensions take them at install time; this one asks.

const HOST = "com.divine.pulse";
const STATE_KEY = "idmState";
const SETTINGS_KEY = "idmSettings";

/** Defaults chosen so the extension is useful but not presumptuous. */
const DEFAULT_SETTINGS = {
  enabled: true,
  minSizeBytes: 2 * 1024 * 1024,
  forwardCookies: false,
  // Types the browser handles perfectly well itself; taking these over just
  // adds a round trip and surprises the user.
  skipExtensions: ["html", "htm", "css", "js", "json", "xml", "txt", "svg", "ico"],
};

const api = globalThis.browser ?? globalThis.chrome;

// ---------------------------------------------------------------------------
// Stored state
// ---------------------------------------------------------------------------

async function getSettings() {
  const stored = await api.storage.local.get(SETTINGS_KEY);
  return { ...DEFAULT_SETTINGS, ...(stored[SETTINGS_KEY] ?? {}) };
}

async function setSettings(update) {
  const merged = { ...(await getSettings()), ...update };
  await api.storage.local.set({ [SETTINGS_KEY]: merged });
  return merged;
}

/**
 * The per-installation id used for pairing.
 *
 * Generated once and kept. It is not a secret and not a credential: it tells
 * the engine *which* extension is asking so a human can approve or revoke it.
 *
 * Single-flight, and that is not a micro-optimisation. The read, the generate
 * and the write are separated by awaits, so two callers that start before
 * either writes -- the top-level greet and the one from onInstalled, which is
 * exactly what happens on a fresh install -- each mint an id and each ask to
 * be paired. The user then sees two prompts, and approving the wrong one
 * pairs an identity the extension has already discarded, leaving it
 * permanently unpaired with no visible reason. Memoising the promise means
 * every caller in this worker waits on the same one.
 */
let pendingClientId = null;

function clientId() {
  pendingClientId ??= (async () => {
    const stored = await api.storage.local.get("clientId");
    if (stored.clientId) return stored.clientId;

    const bytes = crypto.getRandomValues(new Uint8Array(16));
    const id = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

    // Re-check: another worker generation may have written one while this
    // call was awaiting. Deferring to it keeps one identity per installation.
    const again = await api.storage.local.get("clientId");
    if (again.clientId) return again.clientId;

    await api.storage.local.set({ clientId: id });
    return id;
  })().catch((error) => {
    pendingClientId = null; // a failed lookup must not be cached forever
    throw error;
  });
  return pendingClientId;
}

async function setState(update) {
  const current = (await api.storage.session.get(STATE_KEY))[STATE_KEY] ?? {};
  const merged = { ...current, ...update };
  await api.storage.session.set({ [STATE_KEY]: merged });
  await paintIcon(merged);
  return merged;
}

async function getState() {
  return (await api.storage.session.get(STATE_KEY))[STATE_KEY] ?? {};
}

// ---------------------------------------------------------------------------
// Downloads we paused while deciding what to do with them
// ---------------------------------------------------------------------------

const PAUSED_KEY = "idmPausedWhileDeciding";

/**
 * Serialises read-modify-write on the paused set.
 *
 * Two downloads starting together would otherwise each read the set, each add
 * their own id, and each write back -- losing one of them, and with it the
 * record that a download needs releasing.
 */
let pausedQueue = Promise.resolve();

function withPausedIds(mutate) {
  pausedQueue = pausedQueue
    .catch(() => {})
    .then(async () => {
      const stored = (await api.storage.session.get(PAUSED_KEY))[PAUSED_KEY] ?? [];
      const ids = new Set(stored);
      await mutate(ids);
      await api.storage.session.set({ [PAUSED_KEY]: [...ids] });
    });
  return pausedQueue;
}

/**
 * Resumes anything left paused by a previous life of this background context.
 *
 * Taking a download over means pausing the browser's copy, asking the engine,
 * then cancelling or resuming. That leaves a window -- a native messaging
 * round trip -- in which an MV3 worker can be suspended or killed. If that
 * happens the download stays paused with nothing left to resume it, and the
 * user sees a download that simply stopped. Reconciling on every wake means
 * the worst case is a pause, not a loss.
 *
 * Only ids this extension paused are listed, and an id is dropped as soon as
 * its decision is made, so anything still here on a fresh wake is orphaned.
 * Resuming a download that is already running throws and is ignored.
 */
function releaseOrphanedPauses() {
  return withPausedIds(async (ids) => {
    for (const id of ids) {
      try {
        await api.downloads.resume(id);
      } catch {
        // Already running, cancelled, or gone.
      }
    }
    ids.clear();
  });
}

/** Reflects connection and pairing state in the toolbar icon. */
async function paintIcon(state) {
  const settings = await getSettings();
  let badge = "";
  let colour = "#2d7d46";
  if (!settings.enabled) {
    badge = "off";
    colour = "#6b7280";
  } else if (state.unavailable) {
    badge = "!";
    colour = "#b91c1c";
  } else if (state.paired === false) {
    badge = "?";
    colour = "#b45309";
  }
  try {
    await api.action.setBadgeText({ text: badge });
    await api.action.setBadgeBackgroundColor({ color: colour });
  } catch {
    // Badge support is cosmetic; never let it break a download.
  }
}

// ---------------------------------------------------------------------------
// Talking to the host
// ---------------------------------------------------------------------------

/**
 * One request, one short-lived host process.
 *
 * `sendNativeMessage` rather than a long-lived port because the service
 * worker can be stopped at any moment, which would drop a port mid-use. A
 * fresh process per message is a few milliseconds and cannot be left dangling.
 */
async function sendToHost(message) {
  // The id travels on every message. A fresh host process handles each one and
  // exits after replying, so an id sent only with a greeting is already gone by
  // the time anything else arrives.
  const withId = { ...message, client_id: await clientId() };
  return new Promise((resolve) => {
    try {
      api.runtime.sendNativeMessage(HOST, withId, (reply) => {
        const error = api.runtime.lastError;
        if (error) {
          resolve({ type: "unavailable", reason: error.message ?? String(error) });
          return;
        }
        resolve(reply ?? { type: "unavailable", reason: "no reply from host" });
      });
    } catch (error) {
      resolve({ type: "unavailable", reason: String(error) });
    }
  });
}

/** Says hello, which doubles as the pairing request. */
async function greet() {
  const label = `${await browserLabel()} on this PC`;
  const reply = await sendToHost({ type: "hello", label });

  switch (reply.type) {
    case "ready":
      return setState({ paired: true, unavailable: false, reason: null });
    case "pairing_pending":
      return setState({ paired: false, unavailable: false, reason: "awaiting approval" });
    case "declined":
      return setState({ paired: false, unavailable: false, reason: reply.reason });
    default:
      return setState({ paired: false, unavailable: true, reason: reply.reason ?? "unavailable" });
  }
}

/** Specific names first: every Chromium browser also says "Chrome". */
const BROWSER_NAMES = [
  ["Edg", "Edge"],
  ["OPR", "Opera"],
  ["Opera", "Opera"],
  ["Vivaldi", "Vivaldi"],
  ["Brave", "Brave"],
  ["YaBrowser", "Yandex"],
  ["Firefox", "Firefox"],
  ["Chromium", "Chromium"],
  ["Chrome", "Chrome"],
];

/**
 * Names the browser for the pairing prompt.
 *
 * The user-agent string alone is not enough. Brave ships Chrome's UA verbatim
 * to resist fingerprinting, so a UA match labels it "Chrome" -- and a prompt
 * that says Chrome when Brave is asking defeats the one thing the label is
 * for. Brave's own API and the UA-CH brand list both name it honestly, so
 * they are consulted first and the UA is only the fallback.
 */
async function browserLabel() {
  try {
    if (await navigator.brave?.isBrave?.()) return "Brave";
  } catch {
    // Not Brave, or the API is unavailable. Fall through.
  }

  for (const { brand } of navigator.userAgentData?.brands ?? []) {
    // Skip the deliberate decoys UA-CH includes, e.g. "Not;A=Brand".
    if (/not[^a-z]*a[^a-z]*brand/i.test(brand)) continue;
    for (const [needle, name] of BROWSER_NAMES) {
      if (brand.includes(needle)) return name;
    }
  }

  for (const [needle, name] of BROWSER_NAMES) {
    if (navigator.userAgent.includes(needle)) return name;
  }
  return "Browser";
}

// ---------------------------------------------------------------------------
// Taking downloads over
// ---------------------------------------------------------------------------

function extensionOf(filename) {
  const base = (filename ?? "").split(/[\\/]/).pop() ?? "";
  const dot = base.lastIndexOf(".");
  return dot === -1 ? "" : base.slice(dot + 1).toLowerCase();
}

/** Whether this download is worth handing to the engine. */
async function shouldTakeOver(item, settings) {
  if (!settings.enabled) return false;
  // Only ordinary web downloads: blob: and data: have no URL to re-fetch.
  if (!/^https?:/i.test(item.url ?? "")) return false;
  if (item.byExtensionId) return false; // started by another extension
  if (settings.skipExtensions.includes(extensionOf(item.filename))) return false;

  // A stream manifest is a few hundred bytes and stands for a video of
  // gigabytes, so the size rule must not be applied to it. Measuring the
  // playlist would leave every stream to the browser, which cannot fetch
  // the segments in parallel and cannot join them.
  if (isManifest(item.url ?? "") || isManifest(item.filename ?? "")) return true;

  // Unknown size is allowed through; the engine probes and decides.
  if (typeof item.fileSize === "number" && item.fileSize > 0) {
    if (item.fileSize < settings.minSizeBytes) return false;
  }
  return true;
}

/** Whether an address names a stream manifest rather than a file. */
function isManifest(target) {
  const path = String(target).split(/[?#]/)[0].toLowerCase();
  return path.endsWith(".m3u8") || path.endsWith(".m3u") || path.endsWith(".mpd");
}

/** Collects cookies for a URL, only if the user opted in and granted access. */
async function cookieHeaderFor(url, settings) {
  if (!settings.forwardCookies) return undefined;
  try {
    const granted = await api.permissions.contains({
      permissions: ["cookies"],
      origins: [new URL(url).origin + "/*"],
    });
    if (!granted) return undefined;
    const jar = await api.cookies.getAll({ url });
    if (!jar.length) return undefined;
    return jar.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return undefined;
  }
}

async function handOver(item) {
  const settings = await getSettings();
  const payload = {
    type: "download",
    url: item.finalUrl || item.url,
    filename: (item.filename ?? "").split(/[\\/]/).pop() || undefined,
    referrer: item.referrer || undefined,
    size_hint: typeof item.fileSize === "number" && item.fileSize > 0 ? item.fileSize : undefined,
    mime: item.mime || undefined,
    user_agent: navigator.userAgent,
    cookies: await cookieHeaderFor(item.finalUrl || item.url, settings),
  };

  const reply = await sendToHost(payload);
  if (reply.type === "accepted") {
    // Only now cancel the browser's own copy. Cancelling first and then
    // failing to hand over would lose the download entirely.
    try {
      await api.downloads.cancel(item.id);
      await api.downloads.erase({ id: item.id });
    } catch {
      // Already finished or gone; nothing to undo.
    }
    await setState({ unavailable: false });
    return true;
  }

  if (reply.type === "declined") {
    // The engine understood and said no, so the browser keeps it.
    await setState({ paired: !/not paired/i.test(reply.reason ?? ""), reason: reply.reason });
  } else {
    await setState({ unavailable: true, reason: reply.reason ?? "unavailable" });
  }
  return false;
}

// ---------------------------------------------------------------------------
// Listeners, registered at the top level so they survive a worker restart
// ---------------------------------------------------------------------------

api.downloads.onCreated.addListener(async (item) => {
  // Pause before doing anything else, including reading settings.
  //
  // Handing over means asking the engine and only cancelling the browser's
  // copy once it has agreed -- which is a race against the download itself.
  // Reading settings and round-tripping to the host costs a few hundred
  // milliseconds, and a small file from a fast origin is finished well
  // inside that, so the browser keeps its copy and the engine fetches a
  // second one: the same bytes downloaded twice, saved twice. Observed with
  // Firefox on a 6 MiB file from localhost; Chromium only avoided it by
  // winning the same race. Pausing first removes the race rather than
  // narrowing it.
  let paused = false;
  try {
    await api.downloads.pause(item.id);
    paused = true;
    // Recorded straight after, so that if this context dies before the
    // decision is made, the next wake knows to let the download go.
    await withPausedIds((ids) => ids.add(item.id));
  } catch (error) {
    // Already finished, or not pausable. Carry on: cancel may still work,
    // and if it does not the browser simply keeps the download.
    console.debug("DivinePulse: could not pause download", item.id, error);
  }

  let taken = false;
  try {
    const settings = await getSettings();
    if (await shouldTakeOver(item, settings)) {
      taken = await handOver(item);
    }
  } finally {
    // Anything we paused and did not take must be let go again, including
    // when the decision threw. A download left paused by a failure here
    // would look to the user like the extension had silently eaten it.
    if (paused && !taken) {
      try {
        await api.downloads.resume(item.id);
      } catch (error) {
        // Not silent: a download we paused and then could not resume is a
        // download the user has lost, and swallowing this is what made that
        // hard to see. Fall back to asking the browser for it again.
        console.warn("DivinePulse: could not resume download", item.id, error);
        try {
          await api.downloads.download({ url: item.finalUrl || item.url });
        } catch (retryError) {
          console.warn("DivinePulse: could not restart download either", retryError);
        }
      }
    }
    if (paused) {
      await withPausedIds((ids) => ids.delete(item.id));
    }
  }
});

api.runtime.onInstalled.addListener(() => {
  greet();
  try {
    api.contextMenus.removeAll(() => {
      api.contextMenus.create({
        id: "idm-download-link",
        title: "Download with DivinePulse",
        contexts: ["link", "image", "video", "audio"],
      });
    });
  } catch {
    // Context menus are a convenience, not a requirement.
  }
});

api.runtime.onStartup?.addListener(() => {
  greet();
});

api.contextMenus?.onClicked.addListener(async (info) => {
  if (info.menuItemId !== "idm-download-link") return;
  const url = info.linkUrl || info.srcUrl;
  if (!url) return;
  const settings = await getSettings();
  await sendToHost({
    type: "download",
    url,
    referrer: info.pageUrl || undefined,
    user_agent: navigator.userAgent,
    cookies: await cookieHeaderFor(url, settings),
  });
});

/** Messages from the popup. */
api.runtime.onMessage.addListener((message, _sender, respond) => {
  (async () => {
    switch (message?.type) {
      case "getStatus": {
        const reply = await sendToHost({ type: "get_state" });
        if (reply.type === "state") {
          await setState({
            paired: reply.paired,
            integrationEnabled: reply.integration_enabled,
            unavailable: false,
          });
        } else {
          await setState({ unavailable: true, reason: reply.reason });
        }
        respond({ state: await getState(), settings: await getSettings() });
        break;
      }
      case "pair": {
        await greet();
        respond({ state: await getState() });
        break;
      }
      case "setSettings": {
        const updated = await setSettings(message.settings ?? {});
        await paintIcon(await getState());
        respond({ settings: updated });
        break;
      }
      case "requestCookieAccess": {
        // Asked for only when the user turns cookie forwarding on, so the
        // install prompt stays minimal.
        const granted = await api.permissions.request({
          permissions: ["cookies"],
          origins: ["<all_urls>"],
        });
        if (granted) await setSettings({ forwardCookies: true });
        respond({ granted });
        break;
      }
      default:
        respond({ error: "unknown message" });
    }
  })();
  return true; // reply arrives asynchronously
});

const RECONNECT_ALARM = "idm-reconnect";

// A browser started before the app finds nothing listening, and an MV3
// background context then sleeps with "unavailable" as the last thing it
// knew. Downloads still recover on their own, because handing one over wakes
// the context and reaches the host then -- but the toolbar badge stays wrong,
// and an extension that has never been paired has no way to raise its
// pairing prompt until the user happens to start a download. An alarm is the
// one timer that survives the context being suspended.
api.alarms?.create(RECONNECT_ALARM, { periodInMinutes: 1 });

api.alarms?.onAlarm.addListener(async (alarm) => {
  if (alarm.name !== RECONNECT_ALARM) return;
  const state = await getState();

  if (state.unavailable) {
    // Nothing was listening last time. Greeting again both reconnects and,
    // if this extension has never been paired, asks to be.
    await greet();
    return;
  }

  if (state.paired === false) {
    // Reachable but not paired: ask only whether that has changed. Greeting
    // here would raise a fresh pairing prompt every minute, including right
    // after the user declined one -- turning a refusal into a nag.
    const reply = await sendToHost({ type: "get_state" });
    if (reply.type === "state") {
      await setState({ paired: reply.paired, unavailable: false });
    } else if (reply.type === "unavailable") {
      await setState({ unavailable: true, reason: reply.reason });
    }
  }
});

// Every wake: say hello, and let go of anything a previous wake left paused.
greet();
releaseOrphanedPauses();
