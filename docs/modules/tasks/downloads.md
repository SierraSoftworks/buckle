# Downloads and releases

These tasks fetch things from the network. Tag them (for example `tags: [network]`) so you can
leave them out of a plan with `--select '!(task.tags contains "network")'` on a machine without
internet access.

## file.download

Download a single file, verified against a checksum before anything is written.

```yaml
- file.download:
    url: 'https://raw.githubusercontent.com/rcaloras/bash-preexec/{{ vars.bash_preexec_ref }}/bash-preexec.sh'
    dest: /usr/share/bash-preexec/bash-preexec.sh
    mode: "0644"
  tags: [network]
```

```yaml
- file.download:
    url: https://releases.example.com/tool/1.4.2/tool-linux-amd64
    dest: /usr/local/bin/tool
    mode: "0755"
    checksum: https://releases.example.com/tool/1.4.2/SHA256SUMS
    headers: { Authorization: 'Bearer {{ secrets.RELEASES_TOKEN }}' }
```

<KindReference registry="tasks" kind="file.download" />

## archive.extract

Unpack a `.tar`, `.tar.gz` or `.zip` from a URL, a path on the host, or the module's `files/`.

```yaml
# One task owns the CNI plugin directory. `creates` (relative to dest) keeps plans from
# downloading the archive just to compare it.
- archive.extract:
    src: 'https://github.com/containernetworking/plugins/releases/download/v{{ vars.cni_version }}/cni-plugins-linux-{{ vars.cni_arch }}-v{{ vars.cni_version }}.tgz'
    dest: /opt/cni/bin
    creates: bridge
    owner: root
    group: root
    mode: "0755"
  tags: [network]
  notify: [restart]
```

```yaml
- archive.extract:
    src: dashboards.tar.gz            # from the module's files/
    dest: /var/lib/grafana/dashboards
    strip_components: 1
    include: ["*.json"]
```

<KindReference registry="tasks" kind="archive.extract" />

## github.release

Install a binary from a GitHub release, picking the right asset for the host's architecture and
upgrading it when the version changes. One task replaces the usual "query the API, download,
verify, unpack, install, check the version" sequence.

```yaml
- github.release:
    repo: atuinsh/atuin
    version: '{{ vars.atuin_version }}'
    asset: "atuin-{arch}-unknown-linux-gnu.tar.gz"
    arch_map: { x86_64: x86_64, aarch64: aarch64 }   # anything else (armv7l) is blocked, not failed
    extract: atuin
    dest: /usr/local/bin/atuin
    mode: "0755"
    probe: ["atuin", "--version"]
    probe_regex: 'atuin (\S+)'
    checksum_asset: "atuin-{arch}-unknown-linux-gnu.tar.gz.sha256"
  tags: [network]
```

```yaml
- github.release:
    repo: rustic-rs/rustic
    version: v0.9.5
    asset: "rustic-{tag}-{arch}-unknown-linux-musl.tar.gz"
    arch_map: { x86_64: x86_64, aarch64: aarch64 }
    extract: rustic
    dest: /usr/local/bin/rustic
    mode: "0755"
    probe: ["rustic", "--version"]
    probe_regex: 'rustic (\S+)'
```

<KindReference registry="tasks" kind="github.release" />

Pin a version: `latest` asks the GitHub API on every plan, and the anonymous limit is 60 requests
an hour per address.

## docker.image

Pull a container image (a systemd unit or Nomad job then runs it).

```yaml
- docker.image: { name: 'ghcr.io/example/otel-ebpf-profiler:1.0.0', pull: missing }
  tags: [network]
  notify: [restart]
```

<KindReference registry="tasks" kind="docker.image" />
