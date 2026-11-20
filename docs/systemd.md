# systemd user service

An **optional** unit for keeping `openrelay daemon` running under `systemd --user`.
It is not a system package and not a privileged service: it runs with the UID
that enables it, opens no administrative ports, installs no files by itself and
does not require root. The daemon accepts connections started from Android (a
local Unix socket for the CLI and, if provisioned, inbound QUIC); it never
connects outbound.

The canonical file is [../packaging/systemd/openrelay.service](../packaging/systemd/openrelay.service).
`openrelay service print-unit` prints its exact contents, and
`bash scripts/package.sh --check` checks that the two match.

## Requirements

- An active `systemd --user` session (`systemctl --user status`).
- The binary at `~/.local/bin/openrelay` (the unit uses `%h/.local/bin/openrelay`).
- A valid `XDG_RUNTIME_DIR`: systemd defines it for the user session and the
  daemon creates `openrelay/control.sock` there (directory 0700, socket 0600).
- Binary libraries resolvable by the loader. If `openrelay` links a staged
  PipeWire (not installed on the system), export `LD_LIBRARY_PATH` to the
  directory that contains `libpipewire-0.3.so.0` before running the binary,
  `package.sh` or the unit. The unit does not set that variable; for the
  service, add a drop-in with `Environment=LD_LIBRARY_PATH=...`.

## Install and enable by hand

```sh
install -d ~/.local/bin
install -m 0755 <path-to-binary>/openrelay ~/.local/bin/openrelay

install -d ~/.config/systemd/user
install -m 0644 packaging/systemd/openrelay.service ~/.config/systemd/user/openrelay.service

systemctl --user daemon-reload
systemctl --user enable --now openrelay.service
systemctl --user status openrelay.service
```

Functional check and stop:

```sh
openrelay status
systemctl --user stop openrelay.service
```

Process messages go to the user journal:

```sh
journalctl --user -u openrelay.service -e
```

`enable` creates the link in `default.target`, so the unit starts with the user
session. Surviving logout would need `loginctl enable-linger`, which is a system
policy decision and may require the administrator; this unit does not enable it.

## Equivalence with the foreground process

`openrelay daemon` is already a foreground process: it has no `--foreground`
flag and does not fork. The unit does not change the binary's behaviour, it only
supervises it:

| Foreground | Unit |
|---|---|
| `openrelay daemon` in a terminal | `ExecStart=%h/.local/bin/openrelay daemon` |
| Ctrl-C to stop it | `systemctl --user stop openrelay.service` |
| socket `$XDG_RUNTIME_DIR/openrelay/control.sock` | the same, inherited from systemd |
| exits 0 on SIGTERM | `Restart=on-failure` does not restart it |

`Restart=on-failure` restarts the process if it fails, not after a clean stop.
No `--socket` or `--quic` is passed: the defaults are the same as in the
foreground. On first start the daemon provisions its identity in
`~/.config/openrelay` and listens for QUIC sessions on `0.0.0.0:47000`; the unit
allows writing to that directory.

## Hardening and why it does not break startup

- `NoNewPrivileges=yes`: the daemon does not elevate privileges.
- `ProtectSystem=strict`: `/usr`, `/etc`, `/boot` and the rest of the system stay
  read-only. `ReadWritePaths=%t %h/.config` gives write access back to the
  runtime directory (where `openrelay/control.sock` lives) and to `~/.config`,
  where the daemon stores its identity. `ExecStartPre` creates
  `~/.config/openrelay` with mode 0700 before startup.
- `PrivateTmp=yes`: private `/tmp`, with no shared paths between services.
- `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`: AF_UNIX for the local
  socket; AF_INET/AF_INET6 for the QUIC endpoint if it is provisioned.
- `ProtectKernelTunables`, `ProtectControlGroups`, `RestrictNamespaces`,
  `RestrictSUIDSGID`, `LockPersonality`, `SystemCallArchitectures=native` and
  `UMask=0077` close surface without touching the startup path.

The daemon needs `$HOME` to store its identity (`~/.config/openrelay`): the unit
already declares it in `ReadWritePaths` and creates the directory in
`ExecStartPre`. If the state is moved with `--state-dir` or `XDG_CONFIG_HOME`,
that path has to be added to `ReadWritePaths`. If the socket were moved to a path
outside `%t`, it would have to be added too.

## Uninstall

```sh
systemctl --user disable --now openrelay.service
rm -f ~/.config/systemd/user/openrelay.service
systemctl --user daemon-reload
```

The binary and the `$XDG_RUNTIME_DIR/openrelay` directory are removed by hand if
they are no longer used; the manager does not touch them.

## Verify without installing

```sh
systemd-analyze --user verify packaging/systemd/openrelay.service
bash scripts/package.sh --check
```
