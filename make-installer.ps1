# Builds the Windows installer.
#
# Three things have to happen in order, and the order is the reason this is a
# script rather than a line in the README. `cargo tauri build` bundles
# whatever is sitting in target/release at the moment it runs: it builds the
# app binary itself, but the CLI, the native messaging host and the built
# extension are resources it only copies. Run it on its own and it will
# happily ship last week's idm.exe, or fail on a target/release that was
# never populated -- and a stale native messaging host is the kind of bug
# that surfaces as "downloads sometimes go to the browser instead", weeks
# later, on someone else's PC.

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "==> Building the extension" -ForegroundColor Cyan
node extension/build.mjs
if ($LASTEXITCODE -ne 0) { throw "extension build failed" }

# The installer registers one specific extension id with every browser. If
# the keypair were regenerated without this file being updated, the
# registration would name an id no browser will ever present, the extension
# would be refused at launch, and nothing would say why. Cheaper to fail here.
Write-Host "==> Checking the pinned extension id" -ForegroundColor Cyan
$pinned = (Get-Content extension/keys/chromium.pub.txt -TotalCount 1).Trim()
$hook = Get-Content app/src-tauri/installer-hooks.nsh -Raw
if ($hook -notmatch '!define\s+IDM_EXTENSION_ID\s+"([a-p]{32})"') {
    throw "could not find IDM_EXTENSION_ID in app/src-tauri/installer-hooks.nsh"
}
$declared = $Matches[1]
if ($declared -ne $pinned) {
    throw @"
the installer would register the wrong extension id.

  installer-hooks.nsh : $declared
  extension/keys      : $pinned

The keypair was regenerated. Update IDM_EXTENSION_ID in
app/src-tauri/installer-hooks.nsh to $pinned and rebuild.
"@
}
Write-Host "    $declared" -ForegroundColor DarkGray

# Before the bundler, because it copies these rather than building them.
Write-Host "==> Building the CLI and the native messaging host" -ForegroundColor Cyan
cargo build --release -p idm-cli -p idm-host
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host "==> Bundling" -ForegroundColor Cyan
cargo tauri build --config app/src-tauri/tauri.conf.json
if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed" }

$installer = Get-ChildItem target/release/bundle/nsis/*.exe -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $installer) { throw "the bundler reported success but produced no installer" }

$mb = [math]::Round($installer.Length / 1MB, 1)
Write-Host ""
Write-Host "Installer: $($installer.FullName)  ($mb MB)" -ForegroundColor Green
Write-Host ""
Write-Host "It is not code-signed, so SmartScreen will warn on first run:" -ForegroundColor Yellow
Write-Host "  More info -> Run anyway." -ForegroundColor Yellow
