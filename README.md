# gbandit CLI

The command-line client for the [gbandit](https://gbandit.com) game-hosting platform. Use it to deploy and promote games, read logs, query your game's database and manage environment variables.

## Install

Linux / macOS:

```sh
curl -fsSL https://gbandit.com/install.sh | sh
```

Drops the binary in `$HOME/.local/bin/gbandit`. Override with `GBANDIT_INSTALL_DIR`.

Windows (PowerShell):

```powershell
irm https://gbandit.com/install.ps1 | iex
```

Drops `gbandit.exe` in `%LOCALAPPDATA%\gbandit\bin` and adds it to your user `PATH`. Override with `$env:GBANDIT_INSTALL_DIR`.

Both install the release gbandit.com currently serves. The binaries are downloaded from this repository's GitHub releases and checked against the SHA-256 checksums gbandit.com publishes for them, so a binary that differs from the one built here is refused. Every archive carries a [build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations) you can check yourself:

```sh
gh attestation verify gbandit-x86_64-unknown-linux-musl.tar.gz --repo gbandit/cli
```

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
gbandit test-token [1-10] [--guest]
gbandit project <show|cover|publish|unpublish|delete>
gbandit docs [page]
gbandit whoami
gbandit update
```

Run `gbandit <command> --help` for the details of each command.

`gbandit deploy` builds and rolls out to dev. It takes no environment, because dev is the only one a build can reach. Prod is never built: `gbandit promote` rolls out what dev runs to prod, so prod serves exactly what you tested in dev.

`gbandit update` installs the release gbandit.com currently serves, checked against the same checksums as the install scripts.

## Building from source

```sh
cargo build --release
```

The binary lands at `target/release/gbandit`.
