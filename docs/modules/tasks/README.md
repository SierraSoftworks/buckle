# Tasks

A task is one typed, idempotent operation: it plans by comparing what it wants with what the host
has, applies only when they differ, and knows how to undo itself when it leaves the configuration.

```yaml
tasks:
  - apt.package: { name: nomad, version: '1.9.3-*' }   # the kind, and its fields
    notify: [restart]                                  # reserved keys beside it
```

Every task accepts the reserved keys `id`, `name`, `when`, `notify`, `tags`, `removal` and
`retry` beside its own fields (see [Modules](../README.md#tasks)). Every spec rejects unknown
fields, so a misspelt field is an error naming the task and the field.

The field tables on these pages are generated from the task kinds' own schemas, and each kind's
badges say which platforms it runs on, which lock it holds (tasks sharing a lock never run at the
same time, even in different modules), and what happens on removal.

## Files

| Kind | Use it to |
|---|---|
| [`file.copy`](./files.md#file-copy) | Copy a file from the module's `files/`. |
| [`file.template`](./files.md#file-template) | Render a `.j2` template (secret-aware). |
| [`file.directory`](./files.md#file-directory) | Create a directory with an owner and mode. |
| [`file.symlink`](./files.md#file-symlink) | Point a symlink at a target. |
| [`file.line`](./files.md#file-line) | Ensure one line in a file someone else owns. |
| [`file.block`](./files.md#file-block) | Own a marked block of lines in a file. |
| [`file.absent`](./files.md#file-absent) | Make sure something is gone. |

## Downloads and releases

| Kind | Use it to |
|---|---|
| [`file.download`](./downloads.md#file-download) | Download a URL, verifying its checksum. |
| [`archive.extract`](./downloads.md#archive-extract) | Unpack a tar or zip from a URL, the host or the module. |
| [`github.release`](./downloads.md#github-release) | Install a binary from a GitHub release, by version and architecture. |
| [`docker.image`](./downloads.md#docker-image) | Pull a container image. |
| [`git.repository`](./downloads.md#git-repository) | Clone a git repository and keep it at a branch, tag or commit. |

## Packages

| Kind | Use it to |
|---|---|
| [`apt.repository`](./packages.md#apt-repository) | Add an apt repository and its signing key. |
| [`apt.package`](./packages.md#apt-package) | Install, pin, upgrade or remove packages. |
| [`apt.update`](./packages.md#apt-update) | Refresh the package index when it is stale. |

## Services

| Kind | Use it to |
|---|---|
| [`systemd.unit`](./services.md#systemd-unit) | Enable, start, mask a unit. |
| [`systemd.dropin`](./services.md#systemd-dropin) | Override part of a unit. |
| [`systemd.restart`](./services.md#systemd-restart), [`systemd.reload`](./services.md#systemd-reload), [`systemd.daemon_reload`](./services.md#systemd-daemon-reload) | Handlers. |

## Firewall

| Kind | Use it to |
|---|---|
| [`ufw.default`](./firewall.md#ufw-default) | Set a default policy. |
| [`ufw.rule`](./firewall.md#ufw-rule) | Allow, deny, reject or rate-limit traffic. |
| [`ufw.state`](./firewall.md#ufw-state) | Turn the firewall on, with a lock-out guard. |

## Users and access

| Kind | Use it to |
|---|---|
| [`user.group`](./users.md#user-group) | Ensure a group. |
| [`user.account`](./users.md#user-account) | Ensure an account, its groups and password hash. |
| [`ssh.authorized_key`](./users.md#ssh-authorized-key) | Authorise SSH keys for a user. |

## System

| Kind | Use it to |
|---|---|
| [`sysctl.param`](./system.md#sysctl-param) | Set and persist a kernel parameter. |
| [`swap.file`](./system.md#swap-file) | Create and enable a swap file. |
| [`hostname.set`](./system.md#hostname-set) | Set the hostname. |
| [`cron.job`](./system.md#cron-job) | Schedule a command in `/etc/cron.d`. |

## Commands

| Kind | Use it to |
|---|---|
| [`command.run`](./commands.md#command-run) | Run anything else, guarded so it stays plannable. |
