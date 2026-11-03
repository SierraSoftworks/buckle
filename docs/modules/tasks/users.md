# Users and access

The `user.*` tasks use the shadow tools (`useradd`, `usermod`, `groupadd` …) and share the `users`
lock. Removing a user or group is flagged as needing care in every plan, and only ever happens for
accounts Buckle created.

```yaml
name: users
needs: [packages]                 # sudo and its group come from the packages module
tasks:
  - user.group: { name: admin }
  - user.account:
      name: alice
      groups: [admin, sudo]
      shell: /bin/bash
      comment: Alice Example
      password: '{{ secrets.ALICE_PASSWORD_HASH }}'
  - ssh.authorized_key: { user: alice, key: '{{ vars.users_alice_key }}' }
  - file.template:
      src: sudoers-admin.j2
      dest: /etc/sudoers.d/90-admin
      mode: "0440"
      validate: ["visudo", "-cf", "{path}"]
```

## user.group

```yaml
- user.group: { name: admin }
- user.group: { name: media, gid: 2001, system: true }
```

<KindReference registry="tasks" kind="user.group" />

## user.account

```yaml
- user.account:
    name: bob
    groups: [admin, sudo]
    shell: /bin/bash
    comment: Bob Example        # key-only: no password hash, so no password login
```

```yaml
# A service account that outlives this module.
- user.account: { name: ops, uid: 2000, shell: /bin/bash, comment: CI deploys }
  removal: leave
```

```yaml
- user.account: { name: prometheus, system: true, shell: /usr/sbin/nologin }
```

<KindReference registry="tasks" kind="user.account" />

## ssh.authorized_key

```yaml
- ssh.authorized_key: { user: alice, key: '{{ vars.users_alice_key }}' }
```

```yaml
# Exactly these keys and no others.
- ssh.authorized_key:
    user: deploy
    exclusive: true
    key: |
      ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILvN72+kQC9crvoSj+y4Jk0LY8CxRqPR0LxcO+c8+FvH laptop
      from="10.0.0.0/8" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJQd/dwimA/99zGl8xjeMU0LZOcxO8RH07dhzat453+o ci
```

<KindReference registry="tasks" kind="ssh.authorized_key" />
