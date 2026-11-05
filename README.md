# DivinePulse

A download manager for Windows. It splits a file across many connections at
once, keeps a journal so an interrupted download carries on rather than
starting over, and hands downloads over from your browser.

It is a personal project, shared with friends. There is no telemetry, no
account, and nothing phones home.

---

## Installing

1. Run the installer — `DivinePulse_<version>_x64-setup.exe`.
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
- **Torrents and magnet links.** Paste a magnet link in the same box, or
  double-click a `.torrent`. A torrent is usually a folder of files rather than
  one file, so that folder is what appears in the list, and where other
  downloads show connections a torrent shows how many peers it has found. A
  magnet link does not know its own name until the swarm tells it, so a new one
  sits briefly under whatever name the link carried.
- **Video tools.** Settings → **Video tools** says whether `ffmpeg` is
  available and offers to fetch it. What arrives is checked against the exact
  build DivinePulse pins — when it downloads, and again every time before it
  runs — so a copy replaced afterwards is not the one that executes.
- **Downloading only at certain times.** The **Schedule** tab slows downloads
  down, or stops them altogether, between times you choose — so you can hold
  them back during the day and let them run at full speed overnight. While a
  rule applies it decides the speed instead of the limit in Settings, and
  where two rules overlap the stricter one wins. A window whose end is
  earlier than its start runs past midnight.
- **A link that has expired.** Big downloads often use links that stop working
  after an hour or two. Click **New link** on the stopped download, paste a
  fresh address for the same file, and it carries on from where it was
  instead of starting again.

## What it does not do

Being straight about this so nothing surprises you:

- **Seeding.** A torrent uploads while it is downloading, because that is how
  you get peers to send to you. Once it finishes it stops. Leaving your
  connection seeding indefinitely is not something to start doing without
  being asked.
- **DRM-protected video** (Netflix, Prime Video and the like). Not a missing
  feature — it is not possible without the studio's decryption module.
- **Video that comes as separate picture and sound tracks** needs `ffmpeg` to
  join them, and the installer does not include one: it is larger than the
  rest of the app put together, for something most downloads never touch.
  Settings → **Video tools** will fetch it for you, and says how big the
  download is before you agree to it; a copy already on your PATH is used
  instead. Single-track video, and every ordinary file download, work without
  it either way.
- **Hold torrents to a schedule, or to a speed limit changed while they are
  running.** The torrent engine takes its limit once, when it starts, so a
  limit or a schedule rule changed afterwards applies to torrents from the
  next time the app opens. Ordinary downloads follow both at once.
- **Removing a file a virus scanner objected to.** DivinePulse hands each
  finished download to Windows Defender and never lets it delete or
  quarantine anything: a detection fails the download, says what was found,
  and leaves the file exactly where it is. Deleting it is then yours to
  decide, from the download's menu in History. Turn the check off in
  Settings if you would rather it did not run. Files over 8 GB are not
  scanned -- reading a disc image twice is a long wait for a verdict -- and
  that is reported as "not scanned" rather than as a clean bill.
- **Running a command line when a download finishes.** DivinePulse runs a
  program, not a command. Settings takes the full path to a `.exe` and a list
  of arguments, one per line, and hands each line to the program as one
  argument with no shell in between. That is deliberate: a command line has
  to be parsed back into arguments by something, and a server that names its
  file `x & calc.exe` would then have run a command on your PC. For the same
  reason a `.bat`, `.cmd`, `.ps1` or `.lnk` is refused -- each is read by
  another program that would do that parsing. If you want a script, point the
  hook at the interpreter's own `.exe` and pass the script as an argument.
  The program also cannot live in a folder downloads are saved to, since a
  download could then replace it. `{path}`, `{name}` and `{folder}` are
  filled in. Whatever it exits with, the download is unaffected.
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

`tools\fetch-sidecars.ps1` fetches ffmpeg and yt-dlp into `sidecars\` — the
same job Settings → Video tools does at runtime, from the same manifest —
checking each against the SHA-256 its publisher states before unpacking it
and recording the hash of the program itself in `sidecars\sidecars.lock`.
That second hash is checked again every time one of them is about to run,
so a copy replaced after it was installed is not the one that executes. The
two together come to about 130 MB, which is why they are fetched rather than
kept in the repository, and why the installer does not carry them either.

For development, `register-browser.ps1` points the installed browsers at your
build in `target\release` instead of the installed copy. Run it from an
ordinary PowerShell window — not from inside a sandboxed terminal, whose
registry and AppData writes go to a private copy the browsers never see.
