# Packages

The `apt.*` tasks manage Debian and Ubuntu packages. They share the `apt` lock, so two modules
installing packages take turns while their other tasks run in parallel. Without `apt-get` (a
minimal container, another distribution) they plan as blocked rather than failing.

## apt.repository

Add a repository as a deb822 `.sources` file, with its signing key.

```yaml
- apt.repository:
    name: hashicorp
    uri: https://apt.releases.hashicorp.com
    suites: ['{{ host.facts.os.codename }}']
    components: [main]
    signed_by: { url: https://apt.releases.hashicorp.com/gpg }       # an ASCII-armoured key
  tags: [network]
```

```yaml
- apt.repository:
    name: tailscale
    uri: 'https://pkgs.tailscale.com/stable/{{ host.facts.os.id }}'
    suites: ['{{ host.facts.os.codename }}']
    signed_by:
      keyring_url: 'https://pkgs.tailscale.com/stable/{{ host.facts.os.id }}/{{ host.facts.os.codename }}.noarmor.gpg'
```

```yaml
# A private repository: the credentials go to /etc/apt/auth.conf.d/ (mode 0600), never into a plan.
- apt.repository:
    name: internal
    uri: https://apt.example.com/debian
    suites: [stable]
    signed_by: { url: https://apt.example.com/key.asc }
    auth: { login: buckle, password: '{{ secrets.APT_TOKEN }}' }
```

<KindReference registry="tasks" kind="apt.repository" />

Applying runs `apt-get update` once, so a following `apt.package` sees the new packages. Removal
deletes the sources file and key (keeping a sources file edited by hand) and updates again.

## apt.package

Install, pin, upgrade or remove packages.

```yaml
- apt.package: { name: nomad, version: '{{ vars.nomad_version }}' }   # "1.9.3-*" pins a minor
```

```yaml
- apt.package:
    names: [ca-certificates, curl, git, gnupg, jq, sudo, unattended-upgrades]
    install_recommends: false
```

```yaml
- apt.package:
    names: [docker-ce, docker-ce-cli, containerd.io, docker-buildx-plugin, docker-compose-plugin]
  removal: leave          # keep Docker installed even if this module stops applying
```

```yaml
- apt.package: { name: cloudflared, deb: 'https://github.com/cloudflare/cloudflared/releases/download/{{ vars.cloudflared_version }}/cloudflared-linux-{{ vars.deb_arch }}.deb' }
- apt.package: { name: snapd, state: absent }
```

<KindReference registry="tasks" kind="apt.package" />

`state: latest` upgrades on every apply when a newer candidate exists. Held packages are reported
as blocked rather than changed. Removing packages (with `state: absent`, or when the task leaves
the configuration) is simulated first: if apt would take other packages along, the step is blocked
with the list unless `remove_dependents: true`. Removal only ever removes packages this task
installed.

## apt.update

Refresh the package index when it is older than `max_age`.

```yaml
- apt.update: { max_age: 6h }
```

<KindReference registry="tasks" kind="apt.update" />

Put it first in a baseline module; `apt.repository` already updates after adding a repository.
