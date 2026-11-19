#!/usr/bin/env bash
# Construye el tarball de instalación manual de arl y lo valida.
#
# Modo --check: compila `arl`, arma el tarball en un directorio temporal,
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
prefix="arl-$version"

cargo build --locked -p arl-cli --bin arl
target_dir=${CARGO_TARGET_DIR:-$root/target}
bin="$target_dir/debug/arl"
[ -x "$bin" ] || fail "no existe el binario compilado: $bin"

work=$(mktemp -d "${TMPDIR:-/tmp}/arl-package.XXXXXX")
trap 'rm -rf "$work"' EXIT
stage="$work/$prefix"
mkdir -p "$stage"

install -m 0755 "$bin" "$stage/arl"
install -m 0644 "$root/packaging/systemd/arl.service" "$stage/arl.service"
install -m 0644 "$root/packaging/README.md" "$stage/README.md"

# Permisos exactos: binario 0755, el resto 0644, sin setuid/setgid.
[ "$(stat -c '%a' "$stage/arl")" = 755 ] || fail "el binario no quedó en 0755"
for file in "$stage/arl.service" "$stage/README.md"; do
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
if "$stage/arl" service print-unit >"$unit_out" 2>"$unit_err"; then
  :
else
  rc=$?
  printf 'package.sh: el binario no arranca: `arl service print-unit` salió %s\n' "$rc" >&2
  printf 'package.sh: error de carga:\n' >&2
  if [ -s "$unit_err" ]; then
    sed 's/^/  /' "$unit_err" >&2
  else
    printf '  (sin mensaje de error)\n' >&2
  fi
  printf 'package.sh: revisa las bibliotecas del binario; en un host con PipeWire staged exporta LD_LIBRARY_PATH antes de ejecutar este script.\n' >&2
  exit 1
fi
cmp -s "$unit_out" "$root/packaging/systemd/arl.service" \
  || fail "arl service print-unit no coincide con packaging/systemd/arl.service"

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
expected=$(printf '%s\n' "$prefix/" "$prefix/README.md" "$prefix/arl" "$prefix/arl.service")
[ "$entries" = "$expected" ] || fail "el tarball no contiene exactamente binario, unidad y README"

printf 'package.sh: OK %s.tar.gz (%s bytes, sha256 %s)\n' \
  "$prefix" \
  "$(stat -c '%s' "$work/$prefix.tar.gz")" \
  "$(sha256sum "$work/$prefix.tar.gz" | cut -d' ' -f1)"
