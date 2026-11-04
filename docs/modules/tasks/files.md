# Files

File tasks write atomically, set `mode` (an octal string such as `"0644"`), `owner` and `group`
where given (a field left out is not changed), and show a diff in plans. On removal they only
delete what still matches what Buckle wrote, so a hand-edited file is reported as drift rather
than lost.

## file.copy

Copy a file from the module's `files/` directory.

```yaml
- file.copy:
    src: apt/20auto-upgrades
    dest: /etc/apt/apt.conf.d/20auto-upgrades
    mode: "0644"
```

```yaml
# A sudoers drop-in is checked with visudo automatically; anything else can name its own validator.
- file.copy:
    src: user-ca.pub
    dest: /etc/ssh/user-ca.pub
    mode: "0644"
  notify: [reload-sshd]
```

<KindReference registry="tasks" kind="file.copy" />

## file.template

Render a `.j2` [template](../expressions.md#templates) from the module's `files/` and write it.
Templates can read `host`, `vars` and `secrets`; a new file whose render used a secret and has no
`mode` is created `0600`.

```yaml
- file.template:
    src: nomad.hcl.j2
    dest: /etc/nomad.d/nomad.hcl
    mode: "0640"
    validate: ["nomad", "config", "validate", "{path}"]   # checked before it replaces the real file
  notify: [restart]
```

```yaml
# One fragment per concern: when a host stops matching `when`, its fragment is removed.
- file.template:
    src: conf.d/server.hcl.j2
    dest: /etc/nomad.d/conf.d/server.hcl
    mode: "0640"
  when: 'host.groups contains "nomad-servers"'
  notify: [restart]
```

```yaml
- file.template:
    src: sudoers-admin.j2
    dest: /etc/sudoers.d/90-admin
    mode: "0440"
    owner: root
    group: root
    validate: ["visudo", "-cf", "{path}"]
```

<KindReference registry="tasks" kind="file.template" />

## file.directory

Ensure a directory exists, creating missing parents.

```yaml
- file.directory: { path: /etc/nomad.d/conf.d, mode: "0750" }
- file.directory: { path: '{{ vars.nomad_data_dir }}', mode: "0700" }
- file.directory: { path: /srv/media, owner: media, group: media, recurse: true }
```

<KindReference registry="tasks" kind="file.directory" />

Removal deletes the directory only if Buckle created it and it is empty. A directory that existed
before is adopted: Buckle manages its permissions but never deletes it.

## file.symlink

```yaml
- file.symlink: { path: /usr/local/bin/nomad, target: /opt/nomad/1.9.3/nomad }
- file.symlink: { path: /etc/nginx/sites-enabled/app, target: /etc/nginx/sites-available/app }
  notify: [reload]
```

<KindReference registry="tasks" kind="file.symlink" />

## file.line

Ensure one line in a file that something else owns, such as a distribution's default config.
With `regex`, the last matching line is replaced; without one, `line` is appended unless it is
already present.

```yaml
- file.line:
    path: /etc/ssh/sshd_config
    regex: '^#?PasswordAuthentication\s'
    line: "PasswordAuthentication no"
    validate: ["sshd", "-t", "-f", "{path}"]
  notify: [reload-sshd]
```

```yaml
# Settings in a defaults file, each keyed by its variable so a change edits the line in place.
- file.line: { path: /etc/default/tailscaled, regex: '^PORT=', line: 'PORT="41641"' }
  notify: [restart-tailscaled]
- file.line: { path: /etc/shells, line: /usr/bin/fish, create: true }
```

```yaml
# Insert after an existing line the first time.
- file.line:
    path: /etc/apt/apt.conf.d/50unattended-upgrades
    line: '        "Tailscale:${distro_codename}";'
    insert_after: '"\$\{distro_id\}:\$\{distro_codename\}-security";'
```

<KindReference registry="tasks" kind="file.line" />

Removal reverts the edit: an added line is deleted, a replaced line restored to what it was before
Buckle first changed it. Prefer [`file.block`](#file-block) for more than a line or two, and
[`file.template`](#file-template) when Buckle can own the whole file.

## file.block

Own a block of lines inside a file, between two marker lines.

```yaml
- file.block:
    path: /etc/bash.bashrc
    create: true
    marker: "# {mark} BUCKLE terminal (atuin)"
    block: |
      if [[ $- == *i* ]] && command -v atuin >/dev/null 2>&1; then
          eval "$(atuin init bash)"
      fi
```

```yaml
- file.block:
    path: /etc/hosts
    marker: "# {mark} BUCKLE cluster peers"
    block: |
      {% for peer in vars.cluster_peers %}
      {{ peer.address }} {{ peer.name }}
      {% endfor %}
```

<KindReference registry="tasks" kind="file.block" />

`{mark}` becomes `BEGIN` and `END`. Two blocks in one file need different markers. If a file
already has a block with your marker (from another tool), Buckle takes it over.

## file.ini

Own settings and lines in one `[section]` of an INI-style file, such as a Raspberry Pi's
`config.txt`, `php.ini` or a `.conf` with sections. Everything else in the file is left as it is.

```yaml
name: raspberry-pi
tasks:
  - file.ini:
      path: /boot/firmware/config.txt
      section: all
      settings:                            # one value per key, replaced in place
        enable_uart: 1
        dtparam=poe_fan_temp0: 50000       # a key may hold `=` itself
      lines:                               # repeatable keys, kept as written
        - dtoverlay=rpi-poe-plus
        - dtoverlay=pps-gpio,gpiopin=18
      absent: [dtoverlay=vc4-kms-v3d]      # also matches dtoverlay=vc4-kms-v3d,cma-512
    when: 'host.groups contains "raspberry-pi"'
    notify: [reboot]
  - file.ini:
      path: /boot/firmware/config.txt
      section: pi5
      settings: { pciex1_gen: 3 }
    when: 'host.groups contains "raspberry-pi"'
    notify: [reboot]

handlers:
  reboot: { system.reboot: {} }
```

```yaml
- file.ini:
    path: /etc/php/8.3/fpm/php.ini
    section: PHP
    separator: " = "
    settings: { memory_limit: 512M, upload_max_filesize: 64M }
```

<KindReference registry="tasks" kind="file.ini" />

- **Matching.** A setting replaces the last line in the section that starts with `key=` (spacing
  around the separator is ignored). A line already in the section is adopted, not duplicated.
  New lines go after the section's last line, settings first; a missing section is appended.
- **Reverting.** Buckle records what it added, replaced and deleted. Dropping an entry, or the
  task, reverts just that: a replaced line gets its original text back, a deleted one is put
  back. Lines changed by hand since are left alone.
- **config.txt.** Firmware settings apply at boot, so notify a [`system.reboot`](./system.md#system-reboot)
  handler. A `dtparam` that follows a `dtoverlay` sets that overlay's parameter: give overlay
  parameters on the overlay line (`dtoverlay=pps-gpio,gpiopin=18`). Older images keep the file at
  `/boot/config.txt`; a task for a missing file plans as blocked.

## file.tokens

Ensure tokens in a space-separated list on one line: a kernel command line in `cmdline.txt`, or
the quoted value of an assignment such as `GRUB_CMDLINE_LINUX` in `/etc/default/grub`.

```yaml
- file.tokens:
    path: /boot/firmware/cmdline.txt
    present: [cgroup_enable=cpuset, cgroup_enable=memory, cgroup_memory=1]
    absent: [console=serial0]              # also matches console=serial0,115200
  when: 'host.groups contains "raspberry-pi"'
  notify: [reboot]
```

```yaml
- file.tokens:
    path: /etc/default/grub
    key: GRUB_CMDLINE_LINUX_DEFAULT
    present: [apparmor=1, security=apparmor]
  notify: [update-grub]                    # a command.run handler running update-grub
```

<KindReference registry="tasks" kind="file.tokens" />

- **Matching.** Missing `present` tokens are appended. An `absent` entry matches a token equal to
  it or followed by `=` or `,`, so `console` drops every `console=...`; tokens in `present` are
  never dropped.
- **Shape.** Without `key` the file must be a single line. With `key` the value of the last
  `KEY=...` line is edited and its quotes kept. A missing file or key plans as blocked.
- **Reverting.** Dropping a token from the task, or the task, removes what Buckle appended and puts
  back what it deleted, in place.

## file.absent

Make sure a path does not exist: an old config file, a package's default site.

```yaml
- file.absent: { path: /etc/nginx/sites-enabled/default }
  notify: [reload]
- file.absent: { path: /opt/legacy-agent, recursive: true }
```

<KindReference registry="tasks" kind="file.absent" />
