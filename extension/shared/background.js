// Divine IDM browser integration.
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

const HOST = "com.divine.idm";
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
 */
async function clientId() {
  const stored = await api.storage.local.get("clientId");
  if (stored.clientId) return stored.clientId;
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  const id = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  await api.storage.local.set({ clientId: id });
  return id;
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
  const label = `${browserLabel()} on this PC`;
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

function browserLabel() {
  const agent = navigator.userAgent;
  // Order matters: every Chromium browser also says "Chrome".
  for (const [needle, name] of [
    ["Edg/", "Edge"],
    ["OPR/", "Opera"],
    ["Vivaldi", "Vivaldi"],
    ["Brave", "Brave"],
    ["YaBrowser", "Yandex"],
    ["Firefox", "Firefox"],
    ["Chrome", "Chrome"],
  ]) {
    if (agent.includes(needle)) return name;
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
  // Unknown size is allowed through; the engine probes and decides.
  if (typeof item.fileSize === "number" && item.fileSize > 0) {
    if (item.fileSize < settings.minSizeBytes) return false;
  }
  return true;
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
  const settings = await getSettings();
  if (await shouldTakeOver(item, settings)) {
    await handOver(item);
  }
});

api.runtime.onInstalled.addListener(() => {
  greet();
  try {
    api.contextMenus.removeAll(() => {
      api.contextMenus.create({
        id: "idm-download-link",
        title: "Download with Divine IDM",
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

greet();
