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

## One-line installer

The installer downloads the release binary for the host's platform, verifies it against the
SHA-256 digest GitHub publishes, installs it, and then runs whatever Buckle command follows its own
options. This is how [Add an agent](../operations/bootstrapping.md) bootstraps a new host in one
paste.

```sh
# Linux and macOS: installs /usr/local/bin/buckle
curl -fsSL https://github.com/SierraSoftworks/buckle/releases/latest/download/install.sh | sudo sh -s --

# …and enrol the host in the same step
curl -fsSL https://github.com/SierraSoftworks/buckle/releases/latest/download/install.sh | sudo sh -s -- \
  up --server http://buckle.example.ts.net:7700 --insecure-plaintext --label role=worker
```

```powershell
# Windows, from an elevated PowerShell: installs $env:ProgramFiles\Buckle\buckle.exe
& ([scriptblock]::Create((irm https://github.com/SierraSoftworks/buckle/releases/latest/download/install.ps1))) `
  up --server http://buckle.example.ts.net:7700 --insecure-plaintext --label role=worker
```

| Option (`install.sh` / `install.ps1`) | Effect |
|---|---|
| `--version` / `-Version` | Install this release instead of the latest. |
| `--install-dir` / `-InstallDir` | Install somewhere else. |
| `--tailscale-auth-key` / `-TailscaleAuthKey` (or `TS_AUTHKEY`) | Join the tailnet first: installs Tailscale when it is missing (Linux and Windows; on macOS install the app first), then `tailscale up` with this key. Skipped when the node is already on a tailnet. |
| `--tailscale-hostname` / `-TailscaleHostname` | The node name to join as. |
| `--tailscale-tags` / `-TailscaleTags` | ACL tags to advertise, comma-separated. |
| `--tailscale-arg` / `-TailscaleArg` | Another argument for `tailscale up`, repeatable. |

Each release also carries its own copy (`releases/download/v2.4.0/install.sh`), which is what the
server's generated join commands use.

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

Or by hand:

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
