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
    box.textContent = "Divine IDM is not reachable.";
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
    box.textContent = "Browser integration is switched off inside Divine IDM.";
    $("pair").hidden = true;
  } else if (state.paired === false) {
    box.className = "status warn";
    box.textContent = "Waiting for you to approve this browser in Divine IDM.";
    $("pair").hidden = false;
  } else {
    box.className = "status ok";
    box.textContent = "Connected and paired.";
    $("pair").hidden = true;
  }

  if (settings.enabled !== undefined) $("enabled").checked = settings.enabled;
  if (settings.minSizeBytes !== undefined) {
    $("minSize").value = Math.round(settings.minSizeBytes / (1024 * 1024));
  }
  if (settings.forwardCookies !== undefined) {
    $("forwardCookies").checked = settings.forwardCookies;
  }
}

$("enabled").addEventListener("change", async (event) => {
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

$("pair").addEventListener("click", async () => {
  await ask({ type: "pair" });
  render(await ask({ type: "getStatus" }));
});

ask({ type: "getStatus" }).then(render);
