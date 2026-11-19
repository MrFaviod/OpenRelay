#!/usr/bin/env bash
# Verifica un release local de openrelay: manifiesto, artefactos, SBOM y una
# instalación real del tarball del daemon en un prefijo temporal.
#
# Uso:
#   bash scripts/release-check.sh --manifest dist/release-manifest.json
#   bash scripts/release-check.sh --manifest ... --dir /ruta/a/artefactos
#   bash scripts/release-check.sh --emit-manifest dist/release-manifest.json \
#       [--dir dist] [--origin HOST]
#
# `--dir` (o OPENRELAY_RELEASE_DIR) apunta al directorio de artefactos; por defecto
# `dist/`, que está gitignorado. Sale 1 ante cualquier fallo; no hay PASS
# parcial. En este host el binario enlaza el PipeWire staged: exporta
# LD_LIBRARY_PATH antes de ejecutar el check (ver docs/systemd.md).
set -uo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
manifest="dist/release-manifest.json"
dir="${OPENRELAY_RELEASE_DIR:-$root/dist}"
emit=""
origin="local"

usage() {
  cat <<'EOF'
Uso: release-check.sh --manifest FILE [--dir DIR]
     release-check.sh --emit-manifest FILE [--dir DIR] [--origin HOST]

  --manifest FILE     manifiesto versionado que se verifica
  --dir DIR           directorio de artefactos (default: dist/ o OPENRELAY_RELEASE_DIR)
  --emit-manifest F   escribe un manifiesto desde los artefactos presentes
  --origin HOST       origen para artefactos nuevos al emitir (default: local)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --manifest) [ $# -ge 2 ] || { usage >&2; exit 2; }; manifest=$2; shift 2 ;;
    --dir) [ $# -ge 2 ] || { usage >&2; exit 2; }; dir=$2; shift 2 ;;
    --emit-manifest) [ $# -ge 2 ] || { usage >&2; exit 2; }; emit=$2; shift 2 ;;
    --origin) [ $# -ge 2 ] || { usage >&2; exit 2; }; origin=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'release-check.sh: argumento desconocido: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -d "$dir" ] || { printf 'release-check.sh: no existe el directorio de artefactos: %s\n' "$dir" >&2; exit 1; }
dir=$(cd "$dir" && pwd)
case "$manifest" in /*) ;; *) manifest="$root/$manifest" ;; esac
case "$emit" in /*) ;; "") ;; *) emit="$root/$emit" ;; esac

if [ -n "$emit" ]; then
  python3 - "$root" "$dir" "$emit" "$origin" <<'PY'
import hashlib, json, subprocess, sys, tomllib
from datetime import datetime, timezone
from pathlib import Path

root, directorio, salida, origen = (Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
with (root / "Cargo.toml").open("rb") as fh:
    version = tomllib.load(fh)["workspace"]["package"]["version"]
try:
    commit = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"],
                            check=True, capture_output=True, text=True).stdout.strip()
except (OSError, subprocess.CalledProcessError):
    commit = ""

def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for bloque in iter(lambda: fh.read(1 << 20), b""):
            h.update(bloque)
    return h.hexdigest()

anteriores = {}
if salida.is_file():
    try:
        previo = json.loads(salida.read_text(encoding="utf-8"))
        for item in previo.get("artifacts", []):
            anteriores[item["name"]] = item.get("origin", origen)
        if previo.get("sbom", {}).get("origin"):
            anteriores[previo["sbom"]["name"]] = previo["sbom"]["origin"]
    except (json.JSONDecodeError, KeyError, OSError):
        pass

artefactos, sbom = [], None
for path in sorted(directorio.iterdir()):
    if not path.is_file() or path.name == "SHA256SUMS":
        continue
    if path.name.endswith(".cdx.json"):
        sbom = {
            "name": path.name,
            "format": "CycloneDX",
            "sha256": sha256(path),
            "size": path.stat().st_size,
            "origin": anteriores.get(path.name, origen),
        }
    elif path.name.endswith(".tar.gz"):
        arch = "aarch64" if "aarch64" in path.name else "x86_64" if "x86_64" in path.name else "any"
        artefactos.append({
            "name": path.name, "kind": "daemon", "arch": arch,
            "sha256": sha256(path), "size": path.stat().st_size,
            "origin": anteriores.get(path.name, origen),
        })
    elif path.name.endswith(".apk"):
        arch = "aarch64" if "arm64" in path.name or "aarch64" in path.name else \
               "x86_64" if "x86_64" in path.name else "universal"
        artefactos.append({
            "name": path.name, "kind": "apk", "arch": arch,
            "sha256": sha256(path), "size": path.stat().st_size,
            "origin": anteriores.get(path.name, origen),
        })

if not artefactos:
    sys.exit("release-check.sh: no hay tarballs ni APK en %s" % directorio)
if sbom is None:
    sys.exit("release-check.sh: no hay SBOM *.cdx.json en %s" % directorio)

manifiesto = {
    "schema": "openrelay-release-manifest/v1",
    "version": version,
    "commit": commit,
    "branch": subprocess.run(["git", "-C", str(root), "branch", "--show-current"],
                             capture_output=True, text=True).stdout.strip(),
    "generated_at": datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "channel": "local",
    "artifacts": artefactos,
    "sbom": sbom,
    "checksums": {"name": "SHA256SUMS"},
}
salida.parent.mkdir(parents=True, exist_ok=True)
salida.write_text(json.dumps(manifiesto, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

lineas = [f"{a['sha256']}  {a['name']}" for a in artefactos]
lineas.append(f"{sbom['sha256']}  {sbom['name']}")
(directorio / "SHA256SUMS").write_text("\n".join(sorted(lineas)) + "\n", encoding="utf-8")
print(f"release-check.sh: manifiesto escrito en {salida} ({len(artefactos)} artefactos)")
PY
  exit $?
fi

[ -f "$manifest" ] || { printf 'release-check.sh: no existe el manifiesto: %s\n' "$manifest" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/openrelay-release-check.XXXXXX")
trap 'rm -rf "$work"' EXIT
plan="$work/plan.tsv"
: >"$plan"
fail=0
err() { printf 'release-check.sh: %s\n' "$*" >&2; fail=1; }

# 1-3. Manifiesto, hashes, SBOM contra Cargo.lock y SHA256SUMS.
if ! python3 - "$root" "$dir" "$manifest" >"$plan" <<'PY'
import hashlib, json, re, sys, tomllib
from pathlib import Path

root, directorio, mpath = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
errores = []

def error(msg):
    errores.append(msg)

def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for bloque in iter(lambda: fh.read(1 << 20), b""):
            h.update(bloque)
    return h.hexdigest()

try:
    man = json.loads(mpath.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError) as exc:
    sys.exit(f"release-check.sh: manifiesto ilegible: {exc}")

with (root / "Cargo.toml").open("rb") as fh:
    version = tomllib.load(fh)["workspace"]["package"]["version"]
if man.get("version") != version:
    error(f"versión del manifiesto {man.get('version')!r} != Cargo.toml {version!r}")
commit = str(man.get("commit", ""))
if not re.fullmatch(r"[0-9a-f]{7,40}", commit):
    error("commit base ausente o no hexadecimal")

artefactos = man.get("artifacts")
if not isinstance(artefactos, list) or not artefactos:
    sys.exit("release-check.sh: el manifiesto no lista artefactos")

vistos = set()
for a in artefactos:
    if not isinstance(a, dict):
        error("artefacto no es un objeto")
        continue
    faltan = [k for k in ("name", "kind", "arch", "sha256", "size", "origin") if k not in a]
    if faltan:
        error(f"artefacto sin campos {faltan}: {a!r}")
        continue
    nombre = str(a["name"])
    if not nombre or "/" in nombre or nombre in (".", ".."):
        error(f"nombre de artefacto inválido: {nombre!r}")
        continue
    if nombre in vistos:
        error(f"artefacto duplicado: {nombre}")
    vistos.add(nombre)
    if a["kind"] not in ("daemon", "apk"):
        error(f"kind desconocido: {a['kind']!r}")
    if not re.fullmatch(r"[0-9a-f]{64}", str(a["sha256"])):
        error(f"sha256 inválido en {nombre}")
    if not isinstance(a["size"], int) or a["size"] <= 0:
        error(f"tamaño inválido en {nombre}")
        continue
    ruta = directorio / nombre
    if not ruta.is_file():
        error(f"falta el artefacto: {nombre}")
        continue
    if ruta.stat().st_size != a["size"]:
        error(f"tamaño no coincide en {nombre}: {ruta.stat().st_size} != {a['size']}")
    if sha256(ruta) != a["sha256"]:
        error(f"sha256 no coincide en {nombre}")

sbom = man.get("sbom")
if not isinstance(sbom, dict) or not sbom.get("name"):
    error("el manifiesto no declara SBOM")
else:
    ruta = directorio / str(sbom["name"])
    if not ruta.is_file():
        error(f"falta el SBOM: {sbom['name']}")
    else:
        if ruta.stat().st_size != sbom.get("size"):
            error(f"tamaño no coincide en el SBOM {sbom['name']}")
        if sha256(ruta) != sbom.get("sha256"):
            error(f"sha256 no coincide en el SBOM {sbom['name']}")
        try:
            bom = json.loads(ruta.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            error(f"SBOM ilegible: {exc}")
        else:
            if bom.get("bomFormat") not in ("CycloneDX", "SPDX"):
                error("el SBOM no es CycloneDX ni SPDX")
            componentes = {(c.get("name"), c.get("version"))
                           for c in bom.get("components", []) if isinstance(c, dict)}
            with (root / "Cargo.lock").open("rb") as fh:
                lock = tomllib.load(fh)
            faltan = [(p["name"], p["version"]) for p in lock.get("package", [])
                      if (p["name"], p["version"]) not in componentes]
            if faltan:
                error(f"el SBOM no lista {len(faltan)} deps de Cargo.lock: {faltan[:5]}")
            sin_licencia = [c.get("name") for c in bom.get("components", [])
                            if isinstance(c, dict) and not c.get("licenses")]
            if sin_licencia:
                error(f"el SBOM no declara licencia para {len(sin_licencia)} componente(s): {sin_licencia[:5]}")

checks = man.get("checksums")
if not isinstance(checks, dict) or not checks.get("name"):
    error("el manifiesto no declara SHA256SUMS")
else:
    sums_path = directorio / str(checks["name"])
    if not sums_path.is_file():
        error(f"falta {checks['name']}")
    else:
        declarados = {}
        try:
            for linea in sums_path.read_text(encoding="utf-8").splitlines():
                if not linea.strip():
                    continue
                partes = linea.split(maxsplit=1)
                if len(partes) != 2 or not re.fullmatch(r"[0-9a-f]{64}", partes[0]):
                    error(f"línea inválida en {checks['name']}: {linea!r}")
                    continue
                declarados[partes[1].lstrip("*")] = partes[0]
        except OSError as exc:
            error(f"{checks['name']} ilegible: {exc}")
        esperados = {a["name"]: a["sha256"] for a in artefactos if isinstance(a, dict) and "name" in a}
        if isinstance(sbom, dict) and sbom.get("name"):
            esperados[str(sbom["name"])] = sbom.get("sha256")
        if declarados != esperados:
            error(f"{checks['name']} no coincide con los artefactos del manifiesto")

for msg in errores:
    print(f"release-check.sh: {msg}", file=sys.stderr)
if errores:
    sys.exit(1)

for a in artefactos:
    print(f"ART\t{a['kind']}\t{a['arch']}\t{a['name']}")
PY
then
  fail=1
fi

# 4. Instalación real del tarball del daemon para la arquitectura del host.
host_arch=$(uname -m)
daemon_name=$(awk -F'\t' -v arch="$host_arch" '$1=="ART" && $2=="daemon" && $3==arch {print $4; exit}' "$plan")
if [ -z "$daemon_name" ]; then
  if [ ! -s "$plan" ]; then
    err "se omite la instalación: falló la verificación del manifiesto o de los artefactos"
  else
    err "no hay tarball de daemon para la arquitectura del host ($host_arch)"
  fi
else
  tarball="$dir/$daemon_name"
  if [ ! -f "$tarball" ]; then
    err "tarball ausente: $daemon_name"
  else
    prefix="$work/prefix"
    mkdir -p "$prefix/bin" "$prefix/lib/systemd/user"
    if tar -xzf "$tarball" -C "$work"; then
      inner=$(tar -tzf "$tarball" | sed -n '1s|/.*||p')
      if [ -z "$inner" ] || [ ! -f "$work/$inner/openrelay" ]; then
        err "el tarball no contiene $inner/openrelay"
      else
        install -m 0755 "$work/$inner/openrelay" "$prefix/bin/openrelay"
        # Requisito de runtime: el daemon enlaza libpipewire-0.3.so.0. `ldd`
        # distingue una librería ausente de un fallo del propio openrelay; sin este
        # chequeo el síntoma era un 127 opaco. No se oculta el fallo: se nombra
        # la biblioteca que falta y se sale con código distinto de 0.
        if command -v ldd >/dev/null 2>&1; then
          missing_libs=$(ldd "$prefix/bin/openrelay" 2>/dev/null | awk '/not found/ {print $1}' | sort -u | tr '\n' ' ')
          if [ -n "$missing_libs" ]; then
            printf 'release-check.sh: el binario %s no puede arrancar: falta la biblioteca de runtime %s\n' \
              "$prefix/bin/openrelay" "${missing_libs% }" >&2
            printf 'release-check.sh: requisito de runtime: libpipewire-0.3 (paquete libpipewire-0.3-0 del sistema) u otro directorio con la biblioteca vía LD_LIBRARY_PATH; ver docs/release.md, «Dependencia de runtime libpipewire-0.3»\n' >&2
            exit 1
          fi
        fi
        if "$prefix/bin/openrelay" --help >"$work/help.out" 2>"$work/help.err"; then
          grep -q '^openrelay' "$work/help.out" || err "openrelay --help no imprime el uso esperado"
          printf 'release-check.sh: openrelay --help OK desde %s\n' "$prefix/bin/openrelay"
        else
          rc=$?
          err "openrelay --help falló con $rc: $(head -n 3 "$work/help.err" | tr '\n' ' ')"
        fi

        # La unidad empaquetada debe ser la misma que imprime el binario.
        unit_src="$work/$inner/openrelay.service"
        if [ -f "$unit_src" ]; then
          if "$prefix/bin/openrelay" service print-unit >"$work/print-unit.out" 2>"$work/print-unit.err"; then
            cmp -s "$work/print-unit.out" "$unit_src" || err "service print-unit no coincide con la unidad del tarball"
          else
            rc=$?
            err "openrelay service print-unit falló con $rc"
          fi
          if cmp -s "$unit_src" "$root/packaging/systemd/openrelay.service"; then
            :
          else
            err "la unidad del tarball no coincide con packaging/systemd/openrelay.service"
          fi
          install -m 0644 "$unit_src" "$prefix/lib/systemd/user/openrelay.service"
          sed -i "s|^ExecStart=.*|ExecStart=$prefix/bin/openrelay daemon|" "$prefix/lib/systemd/user/openrelay.service"
          if systemd-analyze --user verify "$prefix/lib/systemd/user/openrelay.service" >"$work/verify.out" 2>&1; then
            printf 'release-check.sh: systemd-analyze --user verify OK\n'
          else
            rc=$?
            err "systemd-analyze --user verify falló con $rc: $(head -n 3 "$work/verify.out" | tr '\n' ' ')"
          fi
        fi
      fi
    else
      err "no se pudo extraer $daemon_name"
    fi
  fi
fi

# 5. APK: se comprueba sólo si el manifiesto incluye un APK presente.
while IFS=$'\t' read -r etiqueta kind arch nombre; do
  [ "$etiqueta" = "ART" ] || continue
  [ "$kind" = "apk" ] || continue
  apk="$dir/$nombre"
  [ -f "$apk" ] || continue
  if command -v aapt2 >/dev/null 2>&1; then
    if aapt2 dump badging "$apk" >"$work/apk.out" 2>"$work/apk.err"; then
      printf 'release-check.sh: aapt2 badging OK %s\n' "$nombre"
    else
      err "aapt2 dump badging falló en $nombre: $(head -n 2 "$work/apk.err" | tr '\n' ' ')"
    fi
  elif command -v unzip >/dev/null 2>&1; then
    unzip -l "$apk" | grep -q 'AndroidManifest.xml' || err "$nombre no contiene AndroidManifest.xml"
  else
    err "no hay aapt2 ni unzip para comprobar $nombre"
  fi
  if command -v apksigner >/dev/null 2>&1; then
    if apksigner verify "$apk" >"$work/apksigner.out" 2>&1; then
      printf 'release-check.sh: apksigner verify OK %s\n' "$nombre"
    else
      err "apksigner verify falló en $nombre: $(head -n 2 "$work/apksigner.out" | tr '\n' ' ')"
    fi
  fi
done <"$plan"

if [ "$fail" -ne 0 ]; then
  printf 'release-check.sh: FAIL\n' >&2
  exit 1
fi

printf 'release-check.sh: OK — %s artefactos, SBOM completo, instalación %s verificada\n' \
  "$(wc -l <"$plan")" "$host_arch"
