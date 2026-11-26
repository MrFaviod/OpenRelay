#!/usr/bin/env python3
"""Genera un SBOM CycloneDX 1.5 desde Cargo.lock, sin dependencias externas.

Uso:
    python3 scripts/sbom.py --lock Cargo.lock --output dist/openrelay-0.3.1.cdx.json

El SBOM lista cada paquete resuelto en Cargo.lock (workspace y registro) con su
versión y su licencia, tomada de `cargo metadata --locked --all-features`. Si
`cargo metadata` falla, el script sale con código distinto de 0: no emite un
SBOM degradado sin licencias. Un paquete que sólo declara `license-file`
registra ese archivo como licencia; no se inventa un identificador SPDX.
"""

import argparse
import hashlib
import json
import subprocess
import sys
import tomllib
import uuid
from datetime import datetime, timezone
from pathlib import Path


def workspace_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as fh:
        data = tomllib.load(fh)
    version = data.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit("sbom.py: Cargo.toml no declara workspace.package.version")
    return version


def cargo_licenses(root: Path) -> dict[tuple[str, str], str]:
    """name,version -> licencia declarada.

    Falla si `cargo metadata` no responde: un SBOM sin licencias no sirve.
    Un paquete que sólo declara `license-file` se registra como
    ``license-file:<archivo>``; no se inventa un identificador SPDX.
    """
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        print(
            f"sbom.py: cargo metadata falló y no hay licencias que registrar: {exc}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    meta = json.loads(proc.stdout)
    out: dict[tuple[str, str], str] = {}
    for pkg in meta.get("packages", []):
        license_id = pkg.get("license")
        if not license_id and pkg.get("license_file"):
            license_id = f"license-file:{pkg['license_file']}"
        if license_id:
            out[(pkg["name"], pkg["version"])] = license_id
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="SBOM CycloneDX desde Cargo.lock")
    ap.add_argument("--lock", default="Cargo.lock", help="ruta a Cargo.lock")
    ap.add_argument("--output", required=True, help="ruta del JSON de salida")
    ap.add_argument("--root", default=".", help="raíz del workspace (para Cargo.toml)")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    lock_path = Path(args.lock)
    if not lock_path.is_absolute():
        lock_path = root / lock_path
    raw = lock_path.read_bytes()
    lock = tomllib.loads(raw.decode("utf-8"))
    packages = lock.get("package", [])
    if not packages:
        print("sbom.py: Cargo.lock no tiene paquetes", file=sys.stderr)
        return 1

    version = workspace_version(root)
    licenses = cargo_licenses(root)
    digest = hashlib.sha256(raw).hexdigest()

    components = []
    for pkg in sorted(packages, key=lambda p: (p["name"], p["version"])):
        name, pkg_version = pkg["name"], pkg["version"]
        component = {
            "type": "library",
            "bom-ref": f"pkg:cargo/{name}@{pkg_version}",
            "name": name,
            "version": pkg_version,
            "purl": f"pkg:cargo/{name}@{pkg_version}",
            "scope": "required",
        }
        license_id = licenses.get((name, pkg_version))
        if license_id and license_id.startswith("license-file:"):
            component["licenses"] = [
                {"license": {"name": license_id.split(":", 1)[1]}}
            ]
        elif license_id:
            component["licenses"] = [{"license": {"id": license_id}}]
        components.append(component)

    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, 'openrelay-sbom:' + digest)}",
        "version": 1,
        "metadata": {
            "timestamp": datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
            "tools": [{"vendor": "openrelay", "name": "sbom.py", "version": "1"}],
            "component": {
                "type": "application",
                "bom-ref": f"pkg:cargo/openrelay@{version}",
                "name": "openrelay",
                "version": version,
            },
        },
        "components": components,
    }

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(bom, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"sbom.py: {output} ({len(components)} componentes, {version})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
