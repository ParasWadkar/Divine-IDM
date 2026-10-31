# DivinePulse

A download manager for Windows. It splits a file across many connections at
once, keeps a journal so an interrupted download carries on rather than
starting over, and hands downloads over from your browser.

It is a personal project, shared with friends. There is no telemetry, no
account, and nothing phones home.

---

## Installing

1. Run `DivinePulse_0.1.0_x64-setup.exe`.
2. Windows will almost certainly warn you ("Windows protected your PC"). The
   installer is not code-signed, which is what that warning means — not that
   anything is wrong with it. Click **More info**, then **Run anyway**.
3. When it finishes, DivinePulse starts and sits in the system tray.

To uninstall, use Add or Remove Programs like anything else.

## Setting up the browser extension

The browser part is not in the Chrome or Firefox stores yet, so it has to be
loaded by hand. This takes about a minute.

Open DivinePulse, go to **Settings → Browser Integration**, and click
**Install** next to your browser. The card shows the exact folder to point the
browser at, and the steps for that browser. In short:

**Chrome, Edge, Brave, Vivaldi, Opera**

1. Open `chrome://extensions` (or `edge://extensions`, and so on).
2. Turn on **Developer mode**, top right.
3. Click **Load unpacked** and choose the folder the card shows.

**Firefox**

1. Open `about:debugging#/runtime/this-firefox`.
2. Click **Load Temporary Add-on** and choose the `manifest.json` the card
   shows.

Then click the DivinePulse icon in the browser toolbar. It will ask DivinePulse
to pair, and a prompt appears in the app window — approve it there. That
approval is the whole security model: the app will not accept downloads from
an extension you have not approved.

> **A caveat worth knowing.** Chrome disables unpacked extensions every so
> often and will nag you about developer mode on startup. Firefox removes a
> temporary add-on when you close the browser. Until these are published to
> the stores, that is the deal. The app works fine without the extension —
> you can paste links into it directly.

## Using it

- **Paste a link.** Put it in the box at the top and press **Download**.
- **From the browser.** With the extension paired and switched on, downloads
  over 2 MB are handed to DivinePulse automatically. Smaller ones stay in the
  browser, which is usually what you want.
- **Where files go.** Settings → Download folder. Files are sorted into
  subfolders by type (`Programs`, `Archives`, `Video`, and so on).
- **Pause and resume.** The buttons next to a download's status. Closing the
  app or losing your connection is also safe: it picks up where it left off,
  and verifies what it already has before adding to it.
- **A link that has expired.** Big downloads often use links that stop working
  after an hour or two. Click **New link** on the stopped download, paste a
  fresh address for the same file, and it carries on from where it was
  instead of starting again.

## What it does not do

Being straight about this so nothing surprises you:

- **BitTorrent.** Double-clicking a `.torrent` opens DivinePulse, but it will
  tell you it cannot download it. HTTP, HTTPS, FTP and SFTP only for now.
- **DRM-protected video** (Netflix, Prime Video and the like). Not a missing
  feature — it is not possible without the studio's decryption module.
- **Video that comes as separate picture and sound tracks** needs `ffmpeg` on
  your PATH to join them. Single-track video, and every ordinary file
  download, works without it.
- **macOS and Linux.** Windows only.

## If something goes wrong

**The browser says DivinePulse is not reachable.** The app was not running and
could not be started. Open DivinePulse yourself from the Start menu and try the
download again.

**The browser icon says it is not paired.** Click the icon, then approve the
prompt in the app window. If no prompt appears, check Settings → Browser
Integration — the browser may be switched off there.

**A download sits on "Reconnecting".** It is retrying, and the reason it gives
is the server's. It will keep trying; pause it if you would rather stop.

**A download fails with "the server refused the request".** The link has
usually expired. Click **New link** and paste a fresh address.

---

## Building it yourself

Needs the Rust MSVC toolchain and Node.

```powershell
.\make-installer.ps1
```

The installer lands in `target\release\bundle\nsis\`.

For development, `register-browser.ps1` points the installed browsers at your
build in `target\release` instead of the installed copy. Run it from an
ordinary PowerShell window — not from inside a sandboxed terminal, whose
registry and AppData writes go to a private copy the browsers never see.
