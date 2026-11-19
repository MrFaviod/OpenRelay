#!/usr/bin/env bash
# Construye el tarball de instalación manual de openrelay y lo valida.
#
# Modo --check: compila `openrelay`, arma el tarball en un directorio temporal,
# valida permisos, contenido y reproducibilidad, y no instala nada.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:-}

if [ "$mode" != "--check" ]; then
  printf 'uso: %s --check\n' "$0" >&2
  exit 2
fi

fail() {
  printf 'package.sh: %s\n' "$*" >&2
  exit 1
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
[ -n "$version" ] || fail "no se pudo leer la versión del workspace"
prefix="openrelay-$version"

cargo build --locked -p openrelay-cli --bin openrelay
target_dir=${CARGO_TARGET_DIR:-$root/target}
bin="$target_dir/debug/openrelay"
[ -x "$bin" ] || fail "no existe el binario compilado: $bin"

work=$(mktemp -d "${TMPDIR:-/tmp}/openrelay-package.XXXXXX")
trap 'rm -rf "$work"' EXIT
stage="$work/$prefix"
mkdir -p "$stage"

install -m 0755 "$bin" "$stage/openrelay"
install -m 0644 "$root/packaging/systemd/openrelay.service" "$stage/openrelay.service"
install -m 0644 "$root/packaging/README.md" "$stage/README.md"

# Permisos exactos: binario 0755, el resto 0644, sin setuid/setgid.
[ "$(stat -c '%a' "$stage/openrelay")" = 755 ] || fail "el binario no quedó en 0755"
for file in "$stage/openrelay.service" "$stage/README.md"; do
  [ "$(stat -c '%a' "$file")" = 644 ] || fail "$(basename "$file") no quedó en 0644"
done
if find "$stage" -type f -perm /6000 | grep -q .; then
  fail "hay archivos con setuid o setgid"
fi

# La unidad impresa por el propio binario es la unidad empaquetada. Primero se
# comprueba que el binario arranca: un fallo de carga de bibliotecas no es una
# diferencia de contenido y debe verse como tal.
unit_out="$work/print-unit.out"
unit_err="$work/print-unit.err"
if "$stage/openrelay" service print-unit >"$unit_out" 2>"$unit_err"; then
  :
else
  rc=$?
  printf 'package.sh: el binario no arranca: `openrelay service print-unit` salió %s\n' "$rc" >&2
  printf 'package.sh: error de carga:\n' >&2
  if [ -s "$unit_err" ]; then
    sed 's/^/  /' "$unit_err" >&2
  else
    printf '  (sin mensaje de error)\n' >&2
  fi
  printf 'package.sh: revisa las bibliotecas del binario; en un host con PipeWire staged exporta LD_LIBRARY_PATH antes de ejecutar este script.\n' >&2
  exit 1
fi
cmp -s "$unit_out" "$root/packaging/systemd/openrelay.service" \
  || fail "openrelay service print-unit no coincide con packaging/systemd/openrelay.service"

# Tarball reproducible: orden estable, mtime fijo, sin dueño ni nombre en gzip.
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  --format=gnu -C "$work" -cf "$work/$prefix.tar" "$prefix"
gzip -n -9 "$work/$prefix.tar"

# Nada fuera del prefijo y sólo los tres archivos previstos.
entries=$(tar -tzf "$work/$prefix.tar.gz")
while IFS= read -r entry; do
  case "$entry" in
    "$prefix" | "$prefix/"* ) ;;
    *) fail "entrada fuera del prefijo: $entry" ;;
  esac
done <<< "$entries"
expected=$(printf '%s\n' "$prefix/" "$prefix/README.md" "$prefix/openrelay" "$prefix/openrelay.service")
[ "$entries" = "$expected" ] || fail "el tarball no contiene exactamente binario, unidad y README"

printf 'package.sh: OK %s.tar.gz (%s bytes, sha256 %s)\n' \
  "$prefix" \
  "$(stat -c '%s' "$work/$prefix.tar.gz")" \
  "$(sha256sum "$work/$prefix.tar.gz" | cut -d' ' -f1)"
