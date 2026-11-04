# Facts

Facts are what the agent discovers about its host. Each provider is mounted at
`host.facts.<provider>` and refreshed on its own schedule; selectors, templates and health checks
read them. `buckle facts` prints them all.

```sh
$ buckle facts --provider os
{
  "os": {
    "family": "debian",
    "id": "ubuntu",
    "codename": "noble",
    "version": "24.04",
    "pretty_name": "Ubuntu 24.04.5 LTS",
    "kernel": "6.8.0-48-generic"
  }
}
```

Facts are flattened into dotted keys for expressions, so a list can be tested with `contains` but
not indexed. That is why providers also expose flat keys such as
`host.facts.hardware.disk.root.free_percent` beside their lists.

A provider never fails a run. One that errors keeps its last good value and is marked stale; one
that has never produced a value is *unavailable*, and any module whose `when` reads it is left
alone (neither applied nor removed) until it reports. A provider that answers with `null` (no
cloud, no Tailscale) is a normal value: `host.facts.cloud.provider == "oci"` is simply false.

## os

<KindReference registry="facts" kind="os" />

| Key | Example | Notes |
|---|---|---|
| `family` | `debian` | One of `debian`, `rhel`, `alpine`, `arch`, `macos`, `windows`, `unknown`. |
| `id` | `ubuntu` | The distribution's `ID`. |
| `codename` | `noble` | Handy for apt suites: `'{{ host.facts.os.codename }}'`. |
| `version` | `24.04` | |
| `pretty_name` | `Ubuntu 24.04.5 LTS` | |
| `kernel` | `6.8.0-48-generic` | |

## hardware

<KindReference registry="facts" kind="hardware" />

| Key | Example | Notes |
|---|---|---|
| `arch` | `aarch64` | As `uname -m` reports it: `x86_64`, `aarch64`, `armv7l`. |
| `model` | `Raspberry Pi 5 Model B Rev 1.0` | The device tree's `model` on ARM boards, else DMI `product_name` (`NUC8i5BEH`, `Virtual Machine`); `null` where neither exists. |
| `vendor` | `Raspberry Pi` | DMI `sys_vendor`, or `Raspberry Pi` when the model says so. |
| `raspberry_pi.version` | `5` | The board generation parsed from `model`: `4`, `5`, `400`, `0` for the Zero family. `raspberry_pi` is `null` on anything that is not a Pi, so `host.facts.hardware.raspberry_pi.version != null` selects every Pi. |
| `raspberry_pi.model`, `.compute_module`, `.revision` | `Model B`, `false`, `1.0` | The rest of the name, whether it is a Compute Module, and the board revision. |
| `cpus` | `4` | |
| `mem.total_mb` | `8192` | |
| `swap.total_mb` | `0` | |
| `disks[]` | | `mount`, `fstype`, `total_mb`, `free_mb`, `free_percent` per mounted filesystem. |
| `disk.<name>.*` | `disk.root.free_percent` | The same fields, flat: `root` for `/`, otherwise the mount path with `/` turned into `_` (`disk.var_lib_docker`). |

## net

<KindReference registry="facts" kind="net" />

| Key | Notes |
|---|---|
| `hostname`, `fqdn` | |
| `interfaces[]` | `name`, `state`, `mac`, `mtu`, `addresses[]`. |
| `addresses` | Every global-scope address, flat, for `contains`. |
| `default_route` | The default route, or `null`. |

## tailscale

<KindReference registry="facts" kind="tailscale" />

| Key | Notes |
|---|---|
| `hostname` | The node's MagicDNS name. |
| `ips` | Its tailnet addresses. |
| `tags` | ACL tags, for `host.facts.tailscale.tags contains "tag:server"`. |
| `online`, `backend_state`, `tailnet` | |

`null` when Tailscale is not installed. A module that installs Tailscale should declare
`facts: { provides: [tailscale] }`, so modules that read these facts are planned after it.

## cloud

<KindReference registry="facts" kind="cloud" />

| Provider | Keys |
|---|---|
| Oracle Cloud | `provider: oci`, `region`, `shape`, `id`, `fault_domain`, `display_name` |
| Azure | `provider: azure`, `region`, `sku`, `id`, `name`, `zone` |

Read from the link-local metadata service with a two second timeout per probe; `null` elsewhere.

## users

<KindReference registry="facts" kind="users" />

| Key | Notes |
|---|---|
| `accounts[]` | `name`, `uid`, `gid`, `home`, `shell`. |
| `names` | Every account name, flat: `host.facts.users.names contains "deploy"`. |

## systemd

<KindReference registry="facts" kind="systemd" />

| Key | Notes |
|---|---|
| `version` | |
| `units[]` | `name`, `load`, `active`, `sub`. |
| `active`, `failed` | Unit names, flat: `host.facts.systemd.failed contains "nomad.service"`. |

`null` without systemd.

## nomad

<KindReference registry="facts" kind="nomad" />

`version`, `eligibility`, `drain` and `status` of the local Nomad node; `null` unless `nomad` is on
the `PATH`.

## docker

<KindReference registry="facts" kind="docker" />

`version` and `running`; `null` unless `docker` is on the `PATH`.

## script

<KindReference registry="facts" kind="script" />

Every executable in `<state_dir>/facts.d/` runs (with a 10 second timeout) and its JSON output is
mounted at `host.facts.script.<name>`, named after the file. A script that fails contributes `null`
and a warning.

```sh
#!/bin/sh
# /var/lib/buckle/facts.d/rack
echo '{"row": "b", "unit": 12}'
```

```yaml
when: 'host.facts.script.rack.row == "b"'
```
