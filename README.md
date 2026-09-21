# gbandit CLI

The command-line client for the [gbandit](https://gbandit.com) game-hosting platform. Used to deploy projects, tail logs, run SQL against tenant databases, and manage env vars.

## Install

Linux / macOS:

```sh
curl -fsSL https://github.com/gbandit/cli/releases/latest/download/install.sh | sh
```

Drops the binary in `$HOME/.local/bin/gbandit`. Override with `GBANDIT_INSTALL_DIR` or pin a version with `GBANDIT_VERSION=v0.2.0`.

Windows (PowerShell):

```powershell
irm https://github.com/gbandit/cli/releases/latest/download/install.ps1 | iex
```

Drops `gbandit.exe` in `%LOCALAPPDATA%\gbandit\bin` and adds it to your user `PATH`. Override with `$env:GBANDIT_INSTALL_DIR` or pin a version with `$env:GBANDIT_VERSION = 'v0.2.0'`.

Prebuilt binaries are published for:

- Linux x86_64 / aarch64
- macOS x86_64 / aarch64
- Windows x86_64 / aarch64

## Usage

```sh
gbandit deploy --message "<what you just changed>"
gbandit promote
gbandit logs [frontend|backend]
gbandit sql "SELECT ..."
gbandit env [set|list|delete]
gbandit update
```

`gbandit deploy` builds and rolls out to `dev`; it takes no environment, because dev is the only one a build can reach. Prod never builds: `gbandit promote` points prod at the Release dev is running and rolls it out, so prod serves exactly what you verified in dev.

`gbandit update` downloads the latest GitHub release for your OS/architecture and replaces the installed binary. Pin a specific release with `gbandit update --tag vX.Y.Z`.

## Building from source

```sh
cargo build --release
```

The binary lands at `target/release/gbandit`.

## Pointing at a non-prod platform

Set both env vars before invoking the CLI:

```sh
export GBANDIT_AUTH_ORIGIN=http://auth.gbandit.localhost
export GBANDIT_PLATFORM_API_ORIGIN=http://platform.gbandit.localhost/api
```

When `GBANDIT_AUTH_ORIGIN` resolves to a localhost URL the CLI keeps a separate `credentials-dev.json` so dev tokens don't clobber prod ones.

## Releases

Tagging `vX.Y.Z` on `main` triggers `.github/workflows/release.yml`, which cross-compiles for all supported targets and attaches the archives (and `install.sh` / `install.ps1`) to a GitHub release.
