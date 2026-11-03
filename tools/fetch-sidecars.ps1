# Fetches the external programs this project runs, and refuses anything that
# is not what sidecars/manifest.json says it should be.
#
# These two are the only things DivinePulse executes that it did not build,
# and what they get handed -- URLs, titles, filenames -- came from a web
# page. So the one rule here is that nothing is taken on trust: the bytes
# that arrive are hashed before they are unpacked, and a mismatch deletes
# the download rather than keeping it to look at.
#
# Not run automatically on a developer's machine, and never by the app. It
# is called by make-installer.ps1, because what ships has to be the pinned
# build, and otherwise by hand.
#
#   pwsh tools/fetch-sidecars.ps1            # fetch what is missing
#   pwsh tools/fetch-sidecars.ps1 -Force     # fetch again regardless

[CmdletBinding()]
param(
    # Fetch even if the stamp says this exact build is already here.
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$sidecars = Join-Path $root "sidecars"
$manifestPath = Join-Path $sidecars "manifest.json"

if (-not (Test-Path $manifestPath)) {
    throw "no manifest at $manifestPath"
}
$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json

# TLS 1.2 explicitly: Windows PowerShell 5.1 still defaults to older
# protocols that these hosts refuse, and the failure reads as a connection
# error rather than anything to do with TLS.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Get-Stamp([string]$name) {
    $path = Join-Path $sidecars "$name.stamp"
    if (Test-Path $path) { (Get-Content $path -Raw).Trim() } else { "" }
}

foreach ($tool in $manifest.tools) {
    $wanted = $tool.sha256.ToLowerInvariant()
    $targets = @($tool.take | ForEach-Object { Join-Path $sidecars (Split-Path $_ -Leaf) })
    $present = ($targets | Where-Object { Test-Path $_ }).Count -eq $targets.Count

    if ($present -and -not $Force -and (Get-Stamp $tool.name) -eq $wanted) {
        Write-Host "==> $($tool.name) $($tool.version) is already here" -ForegroundColor DarkGray
        continue
    }

    Write-Host "==> Fetching $($tool.name) $($tool.version)" -ForegroundColor Cyan
    Write-Host "    $($tool.url)"

    $temp = Join-Path ([IO.Path]::GetTempPath()) ("divinepulse-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        $download = Join-Path $temp (Split-Path $tool.url -Leaf)
        # curl.exe rather than Invoke-WebRequest. Windows PowerShell 5.1
        # buffers an entire response in memory before it writes a byte, so a
        # ninety-megabyte fetch shows nothing at all for minutes and then
        # appears instantly -- indistinguishable from a hang, and it was
        # taken for one. curl streams to disk, shows real progress, retries
        # a dropped connection and follows the publisher's redirect.
        & curl.exe --fail --location --retry 3 --retry-delay 2 `
            --connect-timeout 30 --speed-limit 1024 --speed-time 60 `
            --progress-bar --output $download $tool.url
        if ($LASTEXITCODE -ne 0) {
            throw "could not download $($tool.name): curl exited with $LASTEXITCODE"
        }

        $actual = (Get-FileHash -Path $download -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $wanted) {
            # Deleted rather than kept: an unverified executable left lying
            # in a temp folder is the thing this script exists to avoid.
            Remove-Item $download -Force -ErrorAction SilentlyContinue
            throw @"
$($tool.name) does not match its pinned checksum.

  expected  $wanted
  found     $actual
  from      $($tool.url)

Either the publisher replaced the build at that address, or something
replaced it in transit. Check $($tool.source) before changing the manifest.
"@
        }
        Write-Host "    sha256 ok" -ForegroundColor Green

        if ($tool.archive -eq "zip") {
            $unpacked = Join-Path $temp "unpacked"
            Expand-Archive -Path $download -DestinationPath $unpacked -Force
            foreach ($want in $tool.take) {
                # Matched by its tail, because the archive's top-level folder
                # carries the version and would have to be spelled out again
                # here on every bump.
                $leaf = $want -replace "/", "\"
                $found = Get-ChildItem -Path $unpacked -Recurse -File |
                    Where-Object { $_.FullName.EndsWith("\$leaf", [StringComparison]::OrdinalIgnoreCase) } |
                    Select-Object -First 1
                if (-not $found) { throw "$want was not in $($tool.name)'s archive" }
                Copy-Item $found.FullName (Join-Path $sidecars $found.Name) -Force
                Write-Host "    $($found.Name)"
            }
        } else {
            $name = Split-Path $tool.take[0] -Leaf
            Copy-Item $download (Join-Path $sidecars $name) -Force
            Write-Host "    $name"
        }

        # Written last, so an interrupted fetch does not look finished.
        Set-Content -Path (Join-Path $sidecars "$($tool.name).stamp") -Value $wanted -Encoding ascii
    } finally {
        Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# The hash of each program as it will actually be run, which is not the hash
# in the manifest: that one is the publisher's, and it is of the archive.
# This is the other end of the same chain -- archive checked against the
# publisher, these taken out of that archive -- and it is what lets the app
# check the file in front of it before running it.
#
# Written outside the loop, from whatever is on disk, so a run that skipped
# every download because it was already here still produces it.
# Lines rather than JSON, so the engine can read it without a parser and
# so a change to it reads as a change in a diff.
$lines = @(
    "# Generated by tools/fetch-sidecars.ps1. Do not edit by hand.",
    "#",
    "# The SHA-256 of each sidecar as it will be run, which is not the hash",
    "# in manifest.json: that one is the publisher's, and it is of the",
    "# archive. This is the other end of the same chain.",
    ""
)
foreach ($tool in $manifest.tools) {
    foreach ($want in $tool.take) {
        $name = Split-Path $want -Leaf
        $path = Join-Path $sidecars $name
        if (-not (Test-Path $path)) { continue }
        $hash = (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines += "{0} {1} {2}" -f $name, $hash, $tool.version
    }
}
$lockPath = Join-Path $sidecars "sidecars.lock"
Set-Content -Path $lockPath -Value $lines -Encoding ascii

Write-Host ""
Write-Host "Sidecars in $sidecars" -ForegroundColor Cyan
Get-ChildItem $sidecars -Filter *.exe | ForEach-Object {
    "{0,-14} {1,10:N0} bytes" -f $_.Name, $_.Length
}
Write-Host "Recorded in $(Split-Path $lockPath -Leaf)" -ForegroundColor DarkGray
