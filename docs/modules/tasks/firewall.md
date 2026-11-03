# Firewall

The `ufw.*` tasks manage [ufw](https://launchpad.net/ufw) and share the `ufw` lock. They guard
against locking you out: turning the firewall on, or denying incoming traffic on an active
firewall, is **blocked** unless a rule on the host, or a `ufw.rule` earlier in the same plan, keeps
SSH (the ports in `sshd_config`) or a management interface such as `tailscale0` reachable.
`allow_lockout: true` overrides the guard when you mean it.

A complete firewall module, in the order that keeps a fresh host reachable:

```yaml
name: firewall
when: 'host.facts.os.family == "debian"'
needs: [tailscale]          # tailscale0 must exist before a rule names it
tasks:
  - apt.package: { name: ufw }

  - ufw.default: { direction: incoming, policy: deny }
  - ufw.default: { direction: outgoing, policy: allow }

  # Everything that keeps this host reachable comes first...
  - ufw.rule: { rule: allow, interface: tailscale0, comment: tailnet }
  - ufw.rule: { rule: allow, to_port: 41641, proto: udp, comment: tailscale direct connections }
  - ufw.rule:
      rule: limit
      to_port: 22
      proto: tcp
      from_ip: '{{ vars.firewall_lan_cidr }}'
      comment: ssh from the LAN, rate limited

  # ...and switching the firewall on is the last task.
  - ufw.state: { enabled: true, logging: low }
```

## ufw.default

```yaml
- ufw.default: { direction: incoming, policy: deny }
- ufw.default: { direction: routed, policy: allow }     # a host that forwards container traffic
```

<KindReference registry="tasks" kind="ufw.default" />

Removal restores the policy that was in place before Buckle changed it.

## ufw.rule

```yaml
- ufw.rule: { rule: allow, to_port: 443, proto: tcp, comment: https }
- ufw.rule: { rule: allow, to_port: "8000:8100", proto: tcp, from_ip: 10.0.0.0/8 }
- ufw.rule: { rule: allow, to_port: 5353, proto: udp, to_ip: 224.0.0.251, comment: mDNS }
- ufw.rule:
    rule: allow
    to_port: 32400
    proto: tcp
    comment: media server
  when: 'vars.firewall_allow_media == true'
```

<KindReference registry="tasks" kind="ufw.rule" />

A rule needs at least one selector (a port, an address or an interface), so it can never open
everything by accident; a port list or range needs `proto`. Rules are identified by what they
match, not their action or comment, so changing `allow` to `limit` replaces the rule in place.
Removal deletes exactly this rule (for IPv4 and IPv6) and restores any rule it replaced.

## ufw.state

```yaml
- ufw.state: { enabled: true, logging: low }
```

<KindReference registry="tasks" kind="ufw.state" />

Removal disables ufw if Buckle enabled it, and restores the previous logging level.
