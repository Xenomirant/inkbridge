<#
.SYNOPSIS
  Installs a development build of the Inkbridge host for the current user (no admin).

.DESCRIPTION
  Copies target\release\inkbridge-host.exe to %LOCALAPPDATA%\Programs\Inkbridge,
  adds that folder to the *user* PATH (so `inkbridge-host --pair` works in any
  new terminal) and creates Start Menu shortcuts. Stops a running copy first.
  Running from the install folder also keeps the build output unlocked, so
  `cargo build` works while the host runs.

.EXAMPLE
  cargo build --release -p ib-host; .\tools\install-dev-host.ps1
#>
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$src = Join-Path $repo 'target\release\inkbridge-host.exe'
if (-not (Test-Path $src)) { throw "Build first: cargo build --release -p ib-host ($src not found)" }

$dest = Join-Path $env:LOCALAPPDATA 'Programs\Inkbridge'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Get-Process inkbridge-host -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
Copy-Item $src (Join-Path $dest 'inkbridge-host.exe') -Force

# User PATH only (HKCU), no admin rights needed.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @($userPath -split ';' | Where-Object { $_ })
if ($parts -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', (($parts + $dest) -join ';'), 'User')
    "Added to user PATH: $dest (open a new terminal to use it)"
}

$menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'Inkbridge'
New-Item -ItemType Directory -Force -Path $menu | Out-Null
$shell = New-Object -ComObject WScript.Shell
foreach ($s in @(@{ Name = 'Inkbridge.lnk'; Args = '' }, @{ Name = 'Inkbridge - pair a new tablet.lnk'; Args = '--pair' })) {
    $lnk = $shell.CreateShortcut((Join-Path $menu $s.Name))
    $lnk.TargetPath = Join-Path $dest 'inkbridge-host.exe'
    $lnk.Arguments = $s.Args
    $lnk.WorkingDirectory = $dest
    $lnk.Save()
}
"Installed: $(Join-Path $dest 'inkbridge-host.exe')"
