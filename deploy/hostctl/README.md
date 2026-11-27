# hostctl: installing Sothoth's (the router's) execution channel on a GPU host

Names ([docs/NAMES.md](../../docs/NAMES.md)): `abhoth-router` below is the old name of `abhoth-sothoth`;
either binary works, so a host installed with `abhoth-router` needs no change. The steps keep
the old name to match existing installs.

The router controls a remote GPU host over SSH. The key it uses can do one thing: start
`abhoth-router hostctl serve`, which runs a request only if the host's own allowlist permits it
(see `crates/orca-router/src/control/hostctl.rs`). Nothing here is installed automatically.
An admin runs these steps by hand, as root, after reviewing them.

Placeholders used below:

| placeholder | meaning |
|---|---|
| `<apps>` | a pool that survives OS updates (on TrueNAS, the apps pool; **not** `/etc`, which updates replace) |
| `<router-host>` | the address the router connects from |
| `<server>` | the server id in `router.toml` (`[servers.<server>]`) |

## 1. A user that can only run hostctl

```sh
useradd --system --create-home --shell /bin/sh abhoth-exec
install -d -o root -g root -m 0755 <apps>/abhoth <apps>/abhoth/bin <apps>/abhoth/etc
install -o root -g root -m 0755 abhoth-router <apps>/abhoth/bin/abhoth-router
```

The binary and every directory above it must be owned by root and writable by no one else.
Otherwise the sudo rule in step 4 could run something else.

## 2. The router's key, forced to hostctl

On the router side, generate a key for this server and keep it as a file secret:

```sh
ssh-keygen -t ed25519 -N '' -C 'abhoth router -> <server>' -f /run/secrets/ssh-<server>
```

On the host, add a line to `~abhoth-exec/.ssh/authorized_keys` (owner root, mode 0644; the
`.ssh` directory owner root, mode 0755, so `abhoth-exec` cannot edit it):

```
restrict,command="<apps>/abhoth/bin/abhoth-router hostctl serve --allowlist <apps>/abhoth/etc/hostctl.toml",from="<router-host>" ssh-ed25519 AAAA… abhoth router -> <server>
```

`restrict` turns off forwarding, the pty and agent access. `command=` ignores whatever the
client asks to run.

Pin the host key in `router.toml`. Either the fingerprint or the full key line works:

```sh
ssh-keyscan -t ed25519 <host> | ssh-keygen -lf -      # SHA256:… -> servers.<server>.ssh.host_key
```

## 3. The allowlist

Generate it from the config, **read every command**, then install it:

```sh
abhoth-router hostctl export --config router.toml --server <server> --out hostctl.toml
install -o root -g root -m 0644 hostctl.toml <apps>/abhoth/etc/hostctl.toml
```

What the file holds:
- `[[command]]`: each command the router may send, by id and **exact text**, with
  `root = true` where it needs root (docker). The router renders the same text from
  `router.toml`. After a config change that alters a command (a new layout, model or
  option), the host refuses the changed text with "re-export and install it" until the new
  export is installed. Switches that only use existing commands keep working.
- `[builtins]`: the builtins allowed (`allow`) and those allowed as root (`root`).
- `[paths]`: path prefixes builtins may read or write (`json-set` files, model directories
  for restores, lock files). Narrow them to what you expect.
- `[hostd]` (optional): the host daemon's socket and allowed methods (telemetry, fans,
  clocks: SUPERVISOR.md).
- `audit_log`: every request, with its result, one JSON line each.
- `mode = "trusted"` skips the exact-text check. Use it only on development hosts; the
  router's validation warns about it.

## 4. The single sudo entry

Root requests are re-run through one sudo rule that checks the request against the same
root-owned allowlist again. The rule is
`/etc/sudoers.d/abhoth` (on TrueNAS, set it through the UI's sudo commands for the user, so it
survives updates):

```
abhoth-exec ALL=(root) NOPASSWD: <apps>/abhoth/bin/abhoth-router hostctl exec-root --allowlist <apps>/abhoth/etc/hostctl.toml
```

`exec-root` refuses to start when the allowlist is not owned by root, or when group or
others can write it.

## 5. Check from the router

```sh
abhoth sothoth config-check --config router.toml
```

The router's first status poll shows the server as reachable, or gives the SSH error: key,
host key, `from=` or allowlist.

## Removing it

Delete the `authorized_keys` line and the sudo rule. The router then reports the server
unreachable and changes nothing.
