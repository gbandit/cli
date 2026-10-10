# gbandit CLI installer (Windows)
#
# Usage:
#   irm https://github.com/gbandit/cli/releases/latest/download/install.ps1 | iex
#
# Env vars:
#   GBANDIT_VERSION     Pin a specific tag (e.g. v0.2.0). Defaults to "latest".
#   GBANDIT_INSTALL_DIR Where to drop the binary. Defaults to %LOCALAPPDATA%\gbandit\bin.

$ErrorActionPreference = 'Stop'

$repo = 'gbandit/cli'
$version = if ($env:GBANDIT_VERSION) { $env:GBANDIT_VERSION } else { 'latest' }
$installDir = if ($env:GBANDIT_INSTALL_DIR) {
    $env:GBANDIT_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'gbandit\bin'
}

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}

$target = "$arch-pc-windows-msvc"
$asset = "gbandit-$target.zip"
$url = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download/$asset"
} else {
    "https://github.com/$repo/releases/download/$version/$asset"
}

Write-Host "Installing gbandit ($version) for $target to $installDir"

New-Item -ItemType Directory -Force -Path $installDir | Out-Null

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "gbandit-install-$([guid]::NewGuid())")
try {
    $zipPath = Join-Path $tmp.FullName 'gbandit.zip'
    Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing
    Expand-Archive -Path $zipPath -DestinationPath $tmp.FullName -Force

    $binary = Get-ChildItem -Path $tmp.FullName -Filter 'gbandit.exe' -Recurse | Select-Object -First 1
    if (-not $binary) { throw 'release archive did not contain gbandit.exe' }

    Move-Item -Path $binary.FullName -Destination (Join-Path $installDir 'gbandit.exe') -Force
} finally {
    Remove-Item -Recurse -Force $tmp.FullName -ErrorAction SilentlyContinue
}

Write-Host "Installed: $(Join-Path $installDir 'gbandit.exe')"

Write-Host ""
Write-Host "Get started:"
Write-Host ""
Write-Host "  gbandit login                  # log in with Google"
Write-Host "  gbandit scaffold my-game       # scaffold a game into .\my-game (deploy creates the project)"
Write-Host "  cd my-game; gbandit deploy     # live on your dev URL in one command"
Write-Host ""
Write-Host "Started a game in the browser?"
Write-Host ""
Write-Host "  Link a git remote in its Settings on platform.gbandit.com,"
Write-Host "  then clone the repo and deploy from it:"
Write-Host ""
Write-Host "  git clone <your-remote>; cd <your-repo>"
Write-Host "  gbandit deploy"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = if ($userPath) { $userPath -split ';' } else { @() }
if ($pathEntries -notcontains $installDir) {
    $newPath = if ($userPath) { "$userPath;$installDir" } else { $installDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host ""
    Write-Host "Added $installDir to your user PATH. Open a new terminal for the change to take effect."
}
