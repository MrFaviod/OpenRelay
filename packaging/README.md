# packaging

Artifacts for running `openrelay` as a user service. Nothing in this directory
installs itself: these are files to copy by hand and an optional unit, not a
system package.

| Path | What it is |
|---|---|
| `systemd/openrelay.service` | Optional `systemd --user` unit; runs `openrelay daemon`. |
| `../scripts/package.sh` | Builds and validates a reproducible tarball with `--check`. |

The unit uses the binary at `~/.local/bin/openrelay` and the default socket
`$XDG_RUNTIME_DIR/openrelay/control.sock`. It opens no administrative ports,
needs no root and does not modify the system: it runs with the privileges of the
user who enables it. Manual install and uninstall are in
[../docs/systemd.md](../docs/systemd.md).

## Tarball

`bash scripts/package.sh --check` builds the binary and assembles this in a
temporary directory:

```
openrelay-<version>/
  openrelay           0755
  openrelay.service   0644
  README.md     0644
```

and verifies permissions (no setuid/setgid, nothing outside the prefix), that
`openrelay service print-unit` matches `systemd/openrelay.service` byte for byte, and
that two runs produce the same `sha256`. It installs nothing and writes nothing
outside the temporary directory; it deletes that directory when done.

The script does not set `LD_LIBRARY_PATH` and knows nothing about host paths.
