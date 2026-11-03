# Installation

Buckle is one static binary. The same file is the planner, the agent, the server and the web UI.

## Platforms

| Platform | Support |
|---|---|
| Linux (Debian, Ubuntu; amd64, arm64) | Everything: apt, systemd, ufw, sysctl, users, swap and hostname tasks. |
| Other Linux | Everything except the `apt.*` tasks. |
| macOS, Windows | The agent and the platform-neutral tasks: files, commands, downloads, archives and GitHub releases. |

Each task kind lists the platforms it supports in the [task reference](../modules/tasks/README.md).
Planning a task on a platform it does not support is a plan error for its module, so guard such
modules with a `when` on `host.facts.os.family`.

## Homebrew

```sh
brew install sierrasoftworks/tap/buckle
```

## Release binaries

Each [release](https://github.com/SierraSoftworks/buckle/releases) publishes a binary per
platform:

| Asset | Platform |
|---|---|
| `buckle-linux-amd64` | Linux x86_64 (static, musl) |
| `buckle-linux-arm64` | Linux aarch64 (static, musl) |
| `buckle-darwin-amd64` | macOS Intel |
| `buckle-darwin-arm64` | macOS Apple silicon |
| `buckle-windows-amd64.exe` | Windows x86_64 |

```sh
curl -fsSL -o /usr/local/bin/buckle \
  https://github.com/SierraSoftworks/buckle/releases/latest/download/buckle-linux-amd64
chmod +x /usr/local/bin/buckle
buckle --version
```

## Container image

The server is published as a multi-architecture image (amd64 and arm64). Its entrypoint is the
`buckle` binary, its default command is `buckle server`, and it reads `/config/server.yaml`
(`BUCKLE_SERVER_CONFIG`) and keeps its state in the `/data` volume.

```sh
docker run -d --name buckle \
  -v ./server.yaml:/config/server.yaml:ro \
  -v buckle-data:/data \
  -p 7700:7700 -p 7701:7701 \
  ghcr.io/sierrasoftworks/buckle:latest
```

See [Server and agents](./fleet.md) for a complete `server.yaml`.

## Updating

`buckle update` replaces the running binary with the latest release (or `--to VERSION`), after
verifying the download against the SHA-256 digest GitHub publishes for it.

```sh
buckle update
buckle update --to 2.3.1
```

Agents managed by a server can also be upgraded from the UI; see
[Upgrading Buckle](../operations/upgrading.md).
