# System

## sysctl.param

Set a kernel parameter live and persist it. The plan reads the live value from `/proc/sys`, so it
needs no `sysctl` binary.

```yaml
name: sysctl
tasks:
  - sysctl.param: { name: net.ipv4.ip_forward, value: 1 }
  - sysctl.param: { name: net.ipv6.conf.all.forwarding, value: 1 }
  - sysctl.param: { name: net.core.rmem_max, value: 7500000 }        # UDP buffers for QUIC and WireGuard
  - sysctl.param: { name: fs.inotify.max_user_watches, value: 524288 }
```

<KindReference registry="tasks" kind="sysctl.param" />

Every parameter of a module is written to one file, `/etc/sysctl.d/90-buckle-<module>.conf`.
Removal deletes the line (and the file once only comments are left) but cannot know the value to
restore live, so the setting returns to the kernel default at the next boot.

## swap.file

One resource owns the swap file, its `/etc/fstab` line and whether it is on.

```yaml
- swap.file: { path: /swapfile, size_mb: 2048, swappiness: 10 }
```

```yaml
# Sized per host by a variable; the same `id` keeps it one resource as the profile changes.
- swap.file: { path: /swapfile, size_mb: 1024, swappiness: 10 }
  id: swapfile
  when: 'vars.swap_profile == "small"'
- swap.file: { path: /swapfile, size_mb: 4096, swappiness: 10 }
  id: swapfile
  when: 'vars.swap_profile == "large"'
```

<KindReference registry="tasks" kind="swap.file" />

## hostname.set

```yaml
- hostname.set: { name: '{{ host.id }}' }
```

<KindReference registry="tasks" kind="hostname.set" />

Set it before installing anything that names the host after its hostname, such as Tailscale.

## cron.job

Schedule a command as its own file in `/etc/cron.d`. For anything that needs logs, retries or
dependencies, prefer a systemd timer (a `file.template` unit plus `systemd.unit`).

```yaml
- cron.job:
    name: certbot-renew
    schedule: "17 3 * * *"
    command: certbot renew --quiet --deploy-hook "systemctl reload nginx"
```

```yaml
- cron.job: { name: tmp-cleanup, user: www-data, schedule: "@daily", command: find /srv/app/tmp -mtime +7 -delete }
```

<KindReference registry="tasks" kind="cron.job" />

## system.reboot

Reboot the host once the run has finished: usually a handler that boot-time changes notify.

```yaml
tasks:
  - file.line: { path: /boot/firmware/config.txt, line: dtoverlay=rpi-poe-plus }
    notify: [reboot]
handlers:
  reboot: { system.reboot: { delay: 5s, timeout: 10m } }
```

```yaml
- system.reboot: { only_if: test -f /run/reboot-required }   # after unattended upgrades
```

<KindReference registry="tasks" kind="system.reboot" />

- **After the run, once.** The run finishes every module, handler and removal, then reports with
  the reboot pending; the agent reboots after the server acknowledges it (or a 60s grace when
  offline), plus `delay`. Health checks the run had not reached yet run in the bake instead.
- **In a rollout.** The host shows as **rebooting**; it bakes once its agent reconnects from a new
  boot (at least 60s, with a fresh report), and fails under the phase's `max_failures` if it is
  not back within `timeout`.
- **Never unattended.** A drift correction does not reboot, and `buckle apply` takes
  `--no-reboot` to only print the reboot. Without `only_if`, a task (not a handler) reboots on
  every apply.
