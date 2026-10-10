# gbandit CLI

The command-line client for the [gbandit](https://gbandit.com) game-hosting platform. Use it to deploy and promote games, read logs, query your game's database and manage environment variables.

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
gbandit login
gbandit scaffold <name>
gbandit deploy --message "<what you just changed>"
gbandit promote
gbandit logs <frontend|backend>
gbandit sql "SELECT ..."
gbandit env <set|list|delete>
gbandit project <show|cover|publish|unpublish|delete>
gbandit docs [page]
gbandit whoami
gbandit update
```

Run `gbandit <command> --help` for the details of each command.

`gbandit deploy` builds and rolls out to dev. It takes no environment, because dev is the only one a build can reach. Prod is never built: `gbandit promote` rolls out what dev runs to prod, so prod serves exactly what you tested in dev.

`gbandit update` downloads the latest GitHub release for your OS/architecture and replaces the installed binary. Pin a specific release with `gbandit update --tag vX.Y.Z`.

## Building from source

```sh
cargo build --release
```

The binary lands at `target/release/gbandit`.

## Pointing at a non-prod platform

Set these env vars before invoking the CLI:

```sh
export GBANDIT_AUTH_ORIGIN=http://auth.gbandit.localhost
export GBANDIT_PLATFORM_API_ORIGIN=http://platform.gbandit.localhost/api
export GBANDIT_DOCS_ORIGIN=http://docs.gbandit.localhost
```

Credentials are stored per platform domain (`credentials-<domain>.json`), so logging in to a dev platform doesn't log you out of prod.
