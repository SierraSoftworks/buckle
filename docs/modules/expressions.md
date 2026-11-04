# Selectors and templates

Buckle uses two small languages, each for what it is good at:

- **Selectors** ([filt-rs](https://github.com/SierraSoftworks/filters)) decide *whether*: module and
  task `when`, groups, phase `select`, the `filter` health check, UI and API ACLs, and `--select`.
- **Templates** ([minijinja](https://github.com/mitsuhiko/minijinja), a Jinja2 implementation)
  decide *what text*: `.j2` files and any string field of a task or var that contains `{{` or `{%`.

## Selectors

```yaml
when: 'host.groups contains "nomad-clients" && host.facts.os.family == "debian"'
when: 'host.facts.hardware.arch in ["x86_64", "aarch64"]'
when: 'host.labels.canary == true || host.id like "pi-*"'
when: '!(host.facts.tailscale.tags contains "tag:no-backup")'
when: 'host.facts.hardware.disk.root.free_percent > 10'
```

| Operator | Meaning |
|---|---|
| `&&`, `\|\|`, `!` | And, or, not. |
| `==`, `!=` | Equality; strings compare case-insensitively. |
| `>`, `>=`, `<`, `<=` | Ordering. |
| `contains`, `in` | A string contains a substring, or a list contains a value (`a in b` is `b contains a`). |
| `startswith`, `endswith` | String prefix and suffix tests. |
| `like` | Glob match with `*` and `?`. |
| `matches` | Regular expression match. |
| `contains_cs`, `in_cs`, `startswith_cs`, `endswith_cs`, `like_cs` | Case-sensitive variants. |

A property that does not exist is `null`, which is falsy, so `host.facts.cloud.provider == "oci"`
is simply false on a host without a cloud provider. Write YAML selectors in single quotes so the
double quotes inside them survive.

### What a selector can read

| Key | Value |
|---|---|
| `host.id` | The host's identity. |
| `host.labels.*` | Effective labels: the agent's seed labels, overridden by labels set in the UI. Typed like YAML scalars. |
| `host.groups` | The groups the host belongs to (a list, for `contains`). |
| `host.facts.*` | [Facts](../agent/facts.md), flattened to dotted keys. Lists can be tested with `contains` but not indexed. |
| `cluster.id`, `cluster.kind`, `cluster.labels.*`, `cluster.groups` | The [cluster target](./fleet.md#clusters) being evaluated (`kind` is `nomad` or `kubernetes`), in a [workload](./workloads.md)'s `when` and in groups and phases written for clusters. |
| `vars.*` | The host's variables (scalars and lists). |
| `secrets.NAME` | A secret, comparable but never printable; `secrets.NAME.present` and `secrets.NAME.fingerprint` for "is it set" and "did it rotate". |
| `task.kind`, `task.id`, `task.tags`, `module.name`, `module.tags` | The task being evaluated, for `--select`. |

### Cluster selectors

A selector is evaluated against one kind of target at a time, and the namespace it names says
which: a selector over `host.*` is simply false for a cluster, and one over `cluster.*` is false
for a host. A selector that names neither (`true`, `vars.x == 1`) applies to both. So a module's
`when` must not read `cluster.*` and a workload's must not read `host.*`; `buckle validate`
rejects either, and warns about a group that mixes the two.

## Templates

Task fields are templated only when they contain `{{` or `{%`, so plain values cost nothing:

```yaml
- apt.repository:
    name: hashicorp
    uri: https://apt.releases.hashicorp.com
    suites: ['{{ host.facts.os.codename }}']
- swap.file:
    path: /swapfile
    size_mb: '{{ vars.swap_mb }}'       # rendered, then converted to the number the field expects
```

A rendered field takes the type its schema declares, so `'{{ vars.swap_mb }}'` reaches `swap.file`
as the number `512`. A template can read `host.id`, `host.labels`, `host.groups`, `host.facts`
and `vars`; templates (`.j2` files) can also read `secrets`.

```jinja2
{# modules/nomad/files/nomad.hcl.j2 #}
# {{ buckle_managed }}
datacenter = "{{ vars.nomad_datacenter }}"
data_dir   = "{{ vars.nomad_data_dir }}"
bind_addr  = "{{ host.facts.tailscale.ips | first }}"

server {
  enabled = {{ "true" if "nomad-servers" in host.groups else "false" }}
  encrypt = "{{ secrets.NOMAD_ENCRYPT_KEY }}"
  server_join {
    retry_join = {{ vars.nomad_server_peers | to_json }}
  }
}
```

Undefined variables are errors (strict mode), which fail the plan of the module that uses them and
leave every other module alone. When the missing value is a fact that a module this one `needs`
will provide during the same run, the module is deferred instead.

Templates may `{% include %}` and `{% import %}` other `.j2` files of the same module, by their
path under `files/`.

### Filters

Every [minijinja built-in filter](https://docs.rs/minijinja/latest/minijinja/filters/index.html)
(`default`, `join`, `items`, `map`, `select`, `sort`, `upper`, `replace`, `indent`, `first`, …) is
available, plus:

| Filter | Example | Result |
|---|---|---|
| `to_json` | `vars.peers \| to_json` | Compact JSON. |
| `to_nice_json` | `vars.config \| to_nice_json` | Indented JSON. |
| `to_yaml` | `vars.config \| to_yaml` | YAML. |
| `bool` | `vars.flag \| bool` | `"yes"`, `"on"`, `"1"`, `"true"` → `true`. |
| `ternary` | `vars.enabled \| ternary("on", "off")` | The first value when truthy, else the second. |
| `regex_replace` | `host.id \| regex_replace("-\\d+$", "")` | Replace every match. |
| `regex_search` | `vars.version \| regex_search("^2\\.")` | Whether the pattern matches. |
| `basename`, `dirname` | `vars.path \| basename` | Path components. |
| `comment` | `vars.notice \| comment("// ")` | Prefix every line (default `# `). |

`buckle_managed` is a global holding a "Managed by Buckle. Local changes will be overwritten."
banner for the top of generated files.
