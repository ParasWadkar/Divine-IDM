# Registers Divine IDM with your browsers and starts it, for real.
#
# RUN THIS FROM A NORMAL POWERSHELL WINDOW (Start menu -> PowerShell), NOT from
# inside Claude. Claude's desktop app is a packaged (MSIX) app, and anything it
# launches has its writes to AppData and to HKCU redirected into a private copy
# under %LOCALAPPDATA%\Packages\Claude_*\. Browsers never see that copy, so a
# registration made from there reports success and does nothing: Chrome says
# "Specified native messaging host not found" about a key it cannot see.
#
# Add -Rebuild to rebuild the app first (it stops the running copy anyway).
#
# Anyone installing Divine IDM normally is not affected. This only matters while
# developing it from inside Claude.

param([switch]$Rebuild)

$ErrorActionPreference = 'Stop'
$repo  = Split-Path -Parent $MyInvocation.MyCommand.Path
$idm   = Join-Path $repo 'target\release\idm.exe'
$app   = Join-Path $repo 'target\release\divine-idm.exe'
$id    = 'epjaomgmiimpmbipmfbnloinhaehdaie'
$real  = Join-Path $env:LOCALAPPDATA 'DivineIDM'
$virt  = Join-Path $env:LOCALAPPDATA 'Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\DivineIDM'

if ($env:LOCALAPPDATA -like '*\Packages\*') { throw 'This looks like it is running inside a package. Open a normal PowerShell.' }

Write-Host '1. Stopping any copy started from inside Claude'
Get-Process divine-idm -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 2

if ($Rebuild) {
  # The running app locks its own exe, so a rebuild has to come after it is
  # stopped and before it is started again.
  Write-Host '   Rebuilding the app and the command line'
  Push-Location $repo
  try { cargo build --release -p idm-app -p idm-cli; if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' } }
  finally { Pop-Location }
}

Write-Host '2. Bringing your settings, pairings and history over'
New-Item -ItemType Directory -Force $real | Out-Null
foreach ($f in 'settings.json','pairings.json','state.sqlite3','state.sqlite3-shm','state.sqlite3-wal') {
  $from = Join-Path $virt $f; $to = Join-Path $real $f
  if ((Test-Path $from) -and -not (Test-Path $to)) { Copy-Item $from $to; Write-Host "   copied $f" }
}

Write-Host '3. Registering with your browsers'
& $idm --install-host --extension-id $id

Write-Host '4. Starting Divine IDM'
Start-Process $app
Start-Sleep 6

Write-Host '5. Checking'
$key = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.divine.idm'
$manifest = (Get-ItemProperty $key).'(default)'
Write-Host "   Chrome registry key -> $manifest"
Write-Host ("   manifest exists     : " + (Test-Path $manifest))
Write-Host ("   app running         : " + [bool](Get-Process divine-idm -ErrorAction SilentlyContinue))
& $idm --pairings
Write-Host ''
Write-Host 'Now click the Divine IDM icon in Chrome (no restart needed).'
