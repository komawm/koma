#!/usr/bin/env bash
# Build and assemble a Koma development-prerelease archive with a manifest and checksums.
set -euo pipefail

readonly BINARIES=(koma komactl koma-mcp koma-overview)

usage() {
    cat <<'EOF'
Usage: scripts/package-release.sh --version X.Y.Z --target TRIPLE --out DIR

Build Koma release binaries and assemble a development-prerelease archive.

Options:
  --version X.Y.Z   Version to package; must equal workspace.package.version.
  --target TRIPLE   Rust target triple (currently x86_64-unknown-linux-gnu).
  --out DIR         Output directory for the archive and SHA256SUMS.
  -h, --help        Show this help.

Environment:
  CARGO_TARGET_DIR    Cargo build directory (default: <repo>/target).
  SOURCE_DATE_EPOCH   Archive mtime (default: HEAD commit time).

Outputs:
  DIR/koma-<version>-<target>.tar.gz
  DIR/SHA256SUMS
EOF
}

die() {
    printf 'package-release: %s\n' "$*" >&2
    exit 1
}

version=""
target=""
out=""

while (($# > 0)); do
    case "$1" in
        --version)
            (($# >= 2)) || die "--version requires a value"
            version="$2"
            shift 2
            ;;
        --target)
            (($# >= 2)) || die "--target requires a value"
            target="$2"
            shift 2
            ;;
        --out)
            (($# >= 2)) || die "--out requires a value"
            out="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown argument: $1"
            ;;
    esac
done

[[ -n "$version" ]] || die "--version is required"
[[ -n "$target" ]] || die "--target is required"
[[ -n "$out" ]] || die "--out is required"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z, got: $version"
[[ "$target" =~ ^[A-Za-z0-9_.-]+$ ]] || die "invalid target triple: $target"

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repo_root"

cargo_version="$(python3 - <<'PY'
import tomllib
with open("Cargo.toml", "rb") as source:
    print(tomllib.load(source)["workspace"]["package"]["version"])
PY
)"
[[ "$version" == "$cargo_version" ]] \
    || die "version $version does not match workspace.package.version $cargo_version"

name="koma-${version}-${target}"
mkdir -p -- "$out"
out="$(cd -- "$out" && pwd)"
archive="${out}/${name}.tar.gz"
[[ ! -e "$archive" ]] || die "refusing to overwrite existing archive: $archive"

git_revision="$(git rev-parse HEAD)"
if [[ -n "$(git status --porcelain)" ]]; then
    git_dirty=true
else
    git_dirty=false
fi
: "${SOURCE_DATE_EPOCH:=$(git log -1 --format=%ct HEAD)}"
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]] || die "SOURCE_DATE_EPOCH must be an integer"
export SOURCE_DATE_EPOCH

host_target="$(rustc -vV | sed -n 's/^host: //p')"
build_args=(build --release --locked)
for bin in "${BINARIES[@]}"; do
    build_args+=(-p "$bin")
done
if [[ "$target" != "$host_target" ]]; then
    build_args+=(--target "$target")
fi
cargo "${build_args[@]}"

target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
if [[ "$target" != "$host_target" ]]; then
    bin_dir="${target_dir}/${target}/release"
else
    bin_dir="${target_dir}/release"
fi

staging_root="$(mktemp -d "${TMPDIR:-/tmp}/koma-package.XXXXXX")"
trap 'rm -rf -- "$staging_root"' EXIT
stage="${staging_root}/${name}"
mkdir -p -- "${stage}/bin"

for bin in "${BINARIES[@]}"; do
    [[ -x "${bin_dir}/${bin}" ]] || die "missing built binary: ${bin_dir}/${bin}"
    install -m 0755 -- "${bin_dir}/${bin}" "${stage}/bin/${bin}"
done
cp -R -- resources "${stage}/resources"
cp -R -- examples "${stage}/examples"
for file in LICENSE README.md CHANGELOG.md; do
    [[ -f "$file" ]] || die "missing required file: $file"
    install -m 0644 -- "$file" "${stage}/${file}"
done

build_date="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
toml_string() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '"%s"' "$value"
}

{
    printf '# Koma development prerelease manifest. Not an accepted v0.1.0 release.\n'
    printf 'name = "koma"\n'
    printf 'version = %s\n' "$(toml_string "$version")"
    printf 'channel = "development"\n'
    printf 'prerelease = true\n'
    printf 'target = %s\n' "$(toml_string "$target")"
    printf 'git_revision = %s\n' "$(toml_string "$git_revision")"
    printf 'git_dirty = %s\n' "$git_dirty"
    printf 'source_date_epoch = %s\n' "$SOURCE_DATE_EPOCH"
    printf 'build_date = %s\n' "$(toml_string "$build_date")"
    printf 'rustc = %s\n' "$(toml_string "$(rustc -V)")"
    printf 'cargo = %s\n' "$(toml_string "$(cargo -V)")"
    for bin in "${BINARIES[@]}"; do
        sum="$(sha256sum -- "${stage}/bin/${bin}" | cut -d ' ' -f 1)"
        printf '\n[[binaries]]\n'
        printf 'name = %s\n' "$(toml_string "$bin")"
        printf 'path = %s\n' "$(toml_string "bin/${bin}")"
        printf 'sha256 = %s\n' "$(toml_string "$sum")"
    done
} > "${stage}/RELEASE-MANIFEST.toml"

tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    -C "$staging_root" -cf - "$name" | gzip -n -9 > "${archive}.partial"
mv -- "${archive}.partial" "$archive"

(cd -- "$out" && sha256sum -- "${name}.tar.gz" > SHA256SUMS)

printf 'package-release: wrote %s\n' "$archive"
printf 'package-release: wrote %s\n' "${out}/SHA256SUMS"
