// A small Download button on the videos of one site, when asked for.
//
// # Nothing runs until somebody asks
//
// This is not in `content_scripts` in the manifest, and the build refuses a
// manifest that puts it there. A declared content script runs on every page
// that matches, from the moment the extension is installed, for ever --
// which for a feature almost nobody uses means an extension reading pages
// it was never needed on.
//
// Instead the background registers this with `scripting.registerContentScripts`
// for one origin, after the user has granted access to that origin, because
// they pressed a button in the popup while looking at it. Revoking the
// permission unregisters it. So on a browser where nobody has asked, this
// file is a file in the extension folder and nothing more.
//
// # What it does on a page
//
// Finds <video> elements, works out whether each has an address that could
// actually be downloaded, and puts a button over the ones that do. The
// button sends that one address to the background. It reads nothing else,
// writes nothing to the page's own DOM beyond one container of its own, and
// sends nothing anywhere unless the button is pressed.
//
// # Most video on the web cannot be downloaded this way
//
// A player using Media Source Extensions sets `video.src` to a `blob:` URL
// that exists only inside that page: there is nothing at the other end of
// it for anything else to fetch. That is how YouTube, Netflix and most
// commercial players work, so this will have nothing to offer on them --
// and it says so rather than showing a button that fails. The streams
// feature in the popup, which watches for HLS and DASH manifests, is the
// one that helps there.

(() => {
  // Registered scripts are re-injected on every navigation, and a
  // single-page app can navigate without a reload. Without this guard a
  // second copy would start a second observer and a second set of buttons.
  if (window.__divinepulseOverlay) return;
  window.__divinepulseOverlay = true;

  const api = globalThis.browser ?? globalThis.chrome;
  if (!api?.runtime?.sendMessage) return;

  /** How often a burst of DOM changes may trigger a rescan. */
  const RESCAN_MS = 400;
  /** Smaller than this and the button would cover the video. */
  const SMALLEST = 160;

  // One container for everything this adds, in a shadow root: the page
  // cannot restyle what it cannot select, and nothing here can leak into
  // the page's own styles.
  const host = document.createElement("div");
  host.style.cssText = "position:absolute;top:0;left:0;width:0;height:0;z-index:2147483647";
  const shade = host.attachShadow({ mode: "closed" });
  const sheet = document.createElement("style");
  sheet.textContent = `
    .pill {
      position: absolute;
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 6px 10px;
      color: #fff;
      background: rgba(17, 20, 26, 0.86);
      border: 1px solid rgba(255, 255, 255, 0.18);
      border-radius: 8px;
      font: 500 12px/1 system-ui, sans-serif;
      /* The host is a zero-width anchor, so without this the pill
         shrink-wraps to the narrowest box the words will fit in and the
         label comes out one word per line. */
      white-space: nowrap;
      cursor: pointer;
      pointer-events: auto;
    }
    .pill[disabled] { cursor: default; opacity: 0.75; }
    .pill:not([disabled]):hover { background: rgba(37, 99, 235, 0.92); }
  `;
  shade.append(sheet);

  /**
   * The address of a video, or why there isn't one.
   *
   * `currentSrc` rather than the `src` attribute: a <video> with <source>
   * children has no src of its own, and the browser has already decided
   * which child it is playing.
   */
  function addressOf(video) {
    const src = video.currentSrc || video.src || "";
    if (!src) return { why: "nothing is loaded yet" };
    // See the header: there is nothing at the other end of these for
    // anything outside the page to fetch.
    if (src.startsWith("blob:") || src.startsWith("data:") || src.startsWith("mediasource:")) {
      return { why: "this player streams in pieces" };
    }
    if (!/^https?:/i.test(src)) return { why: "not a web address" };
    return { url: src };
  }

  /**
   * Where a pill sits: the top-left inside of its video, in page
   * coordinates.
   *
   * Page coordinates rather than viewport ones, which is what makes
   * scrolling free: `getBoundingClientRect().top` falls by exactly as much
   * as `scrollY` rises, so the sum does not change while the page scrolls
   * and the pill travels with its video without anything being told to
   * move it.
   */
  function place(pill, video) {
    const box = video.getBoundingClientRect();
    pill.style.left = `${Math.round(box.left + window.scrollX + 10)}px`;
    pill.style.top = `${Math.round(box.top + window.scrollY + 10)}px`;
    // Hidden rather than removed when the video is off screen or collapsed,
    // so scrolling back does not mean waiting for the next rescan.
    pill.hidden = box.width < SMALLEST || box.height < 60;
  }

  const pills = new Map();

  function pillFor(video) {
    let pill = pills.get(video);
    if (!pill) {
      pill = document.createElement("button");
      pill.className = "pill";
      pill.type = "button";
      shade.append(pill);
      pills.set(video, pill);
    }
    return pill;
  }

  function scan() {
    const seen = new Set();
    for (const video of document.querySelectorAll("video")) {
      seen.add(video);
      const found = addressOf(video);
      const pill = pillFor(video);

      if (found.why) {
        pill.textContent = `DivinePulse: ${found.why}`;
        pill.disabled = true;
        pill.onclick = null;
      } else {
        pill.textContent = "Download with DivinePulse";
        pill.disabled = false;
        pill.onclick = async (event) => {
          // The page may have its own click handler on the player.
          event.preventDefault();
          event.stopPropagation();
          pill.disabled = true;
          pill.textContent = "Sending…";
          try {
            const reply = await api.runtime.sendMessage({
              type: "grabVideo",
              url: found.url,
              pageUrl: location.href,
            });
            pill.textContent = reply?.ok ? "Sent to DivinePulse" : (reply?.reason ?? "Refused");
          } catch {
            pill.textContent = "DivinePulse is not running";
          }
          // Back to an offer after a moment: the same video may well be
          // worth sending again after a seek or a quality change.
          setTimeout(() => scan(), 2500);
        };
      }
      place(pill, video);
    }

    for (const [video, pill] of pills) {
      if (!seen.has(video)) {
        pill.remove();
        pills.delete(video);
      }
    }

    // Attached only once there is something to show. A page with no video
    // ends up with nothing added to it at all.
    if (pills.size && !host.isConnected) document.body?.append(host);
    if (!pills.size && host.isConnected) host.remove();
  }

  let pending = null;
  function soon() {
    if (pending) return;
    pending = setTimeout(() => {
      pending = null;
      scan();
    }, RESCAN_MS);
  }

  /** Whether a batch of mutations could have changed which videos exist. */
  function touchedAVideo(records) {
    for (const record of records) {
      for (const nodes of [record.addedNodes, record.removedNodes]) {
        for (const node of nodes) {
          if (node.nodeType !== 1) continue;
          if (node.tagName === "VIDEO" || node.querySelector?.("video")) return true;
        }
      }
    }
    return false;
  }

  // Throttled, and then filtered: a video page mutates constantly -- a
  // player rewrites its own controls several times a second -- so a
  // rescan on every batch would be this extension's own fault for being
  // slow. Almost none of those batches add or remove a video, and
  // checking that is a walk over a usually-empty node list.
  new MutationObserver((records) => {
    if (touchedAVideo(records)) soon();
  }).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });
  // There is no scroll handler, deliberately. There was one, and it
  // recomputed a position that scrolling cannot change -- a
  // `getBoundingClientRect` read and a style write per pill, forcing a
  // reflow each time round, dozens of times a second, on somebody else's
  // page, to arrive at the number already there. A resize does move a
  // video within the page, and fires rarely enough to answer directly.
  window.addEventListener(
    "resize",
    () => {
      for (const [video, pill] of pills) place(pill, video);
    },
    { passive: true },
  );

  scan();
})();
