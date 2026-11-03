# Services

The `systemd.*` tasks manage units on Linux. They share the `systemd` lock. Without systemd as
PID 1 (most containers) they plan as blocked with the reason, so a plan still reads cleanly.

## systemd.unit

Enable, start, stop or mask a unit. Each of `enabled`, `active` and `masked` is managed only when
you set it.

```yaml
- systemd.unit: { name: nomad, enabled: true, active: true }
```

```yaml
# A unit whose file this module writes: reload systemd before enabling it.
- file.template:
    src: rustic-backup.timer.j2
    dest: /etc/systemd/system/rustic-backup.timer
  notify: [daemon-reload]
- systemd.unit:
    name: rustic-backup.timer
    enabled: true
    active: true
    daemon_reload: true
```

```yaml
- systemd.unit: { name: snapd.service, masked: true }
```

<KindReference registry="tasks" kind="systemd.unit" />

Starting a unit is not restarting it: when its configuration changes, notify a
[`systemd.restart`](#systemd-restart) handler. Removal undoes only what Buckle did: it stops a unit
Buckle started, disables one Buckle enabled, and unmasks one Buckle masked.

## systemd.dropin

Override part of a unit with a drop-in file, without owning the unit file itself.

```yaml
- systemd.dropin:
    unit: nomad.service
    content: |
      [Service]
      LimitNOFILE=65536
      Restart=on-failure
      RestartSec=5s
  notify: [restart]
```

```yaml
# Replace ExecStart (the empty line clears the packaged one first).
- systemd.dropin:
    unit: nomad.service
    name: exec.conf
    content: |
      [Service]
      ExecStart=
      ExecStart=/usr/bin/nomad agent -config {{ vars.nomad_config_dir }} -config {{ vars.nomad_config_dir }}/conf.d
  notify: [restart]
```

<KindReference registry="tasks" kind="systemd.dropin" />

The drop-in is written to `/etc/systemd/system/<unit>.d/<name>` and systemd is reloaded; the unit
itself is not restarted, so notify a handler.

## systemd.restart

A handler that restarts a unit. In one module, a restart also covers a reload of the same unit.

```yaml
handlers:
  restart: { systemd.restart: { name: nomad } }
```

<KindReference registry="tasks" kind="systemd.restart" />

## systemd.reload

A handler that reloads a unit's configuration, restarting it if it cannot reload. Prefer it for
services where a restart drops connections, such as `ssh`.

```yaml
handlers:
  reload-sshd: { systemd.reload: { name: ssh } }
```

<KindReference registry="tasks" kind="systemd.reload" />

## systemd.daemon_reload

A handler that runs `systemctl daemon-reload`, always before the module's restarts and reloads.

```yaml
handlers:
  daemon-reload: { systemd.daemon_reload: {} }
```

<KindReference registry="tasks" kind="systemd.daemon_reload" />
