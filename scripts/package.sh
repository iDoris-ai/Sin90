#!/usr/bin/env bash
# ME4-6.0.2 (Sin90 part) / DEP-A4 — build a release tarball for a given
# target triple.
#
# Produces, under dist/:
#   sin90-<ver>-<os>-<arch>.tar.gz  — unpacks to a top-level
#     sin90-<ver>-<os>-<arch>/ directory containing exactly domain-os.yml
#     (the repo-root manifest, NOT domain-os.remote-allowed.yml, rewritten by
#     `agent24 package-integrity generate` to carry its `integrity:` section),
#     files.json (the per-file sha256 manifest it references) and bin/sin90
#     (the release binary; on macOS signed with Developer ID + notarized). That layout is what
#     `agent24 os install <dir>` expects: it copies the given directory
#     into the packages root verbatim and reads domain-os.yml's
#     `spawn.command` (here `bin/sin90`) relative to it — see
#     agent24-os-packages::install::install and this repo's domain-os.yml
#     (confirmed read-only against Agent24 rust/apps/agent24-cli/src/main.rs
#     ~L552-575, os_local/OsAction::Install).
#   SHA256SUMS — `shasum -a 256` over the tarball, run from inside dist/ so
#     the recorded filename is relative.
#
# `--target <triple>` selects which of the 4 supported targets to build for
# (mirrors Agent24's release.yml matrix — DEP-A3):
#   aarch64-apple-darwin        -> sin90-<ver>-macos-arm64.tar.gz
#   x86_64-apple-darwin         -> sin90-<ver>-macos-x64.tar.gz
#   x86_64-unknown-linux-gnu    -> sin90-<ver>-linux-x64.tar.gz
#   aarch64-unknown-linux-gnu   -> sin90-<ver>-linux-arm64.tar.gz
# With no --target, the triple is auto-detected from `uname -sm` (the
# existing "sin90-<ver>-macos-arm64" output on an Apple-silicon Mac is
# unchanged). Each target is built natively via `cargo build --release
# --target <triple>` — this script does not attempt cross-compilation; on
# CI each target runs on a native runner for that triple (see
# .github/workflows/release.yml), and locally it only succeeds for a
# target the host toolchain can actually link.
#
# The version is read from Cargo.toml's [package] version — the single
# source of truth — so this script never needs its own copy of "0.5.0".
#
# Ends with a self-check (sha256 verification, exact tarball file-list
# match, and binary-architecture check) so a broken package fails the build
# instead of shipping quietly.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

usage() {
    cat >&2 <<'EOF'
usage: scripts/package.sh [--target <triple>]

Supported targets:
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu

With no --target, the triple is auto-detected from the host (uname -sm).
EOF
}

target=""
while [ $# -gt 0 ]; do
    case "$1" in
        --target)
            [ $# -ge 2 ] || { echo "error: --target requires a value" >&2; usage; exit 1; }
            target="$2"
            shift 2
            ;;
        --target=*)
            target="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage
            exit 1
            ;;
    esac
done

# target triple -> (os, arch) package-name components. Bash 3.2 (macOS's
# default /bin/bash) has no associative arrays, so this is a case statement
# rather than a `declare -A` map.
target_os_arch() {
    case "$1" in
        aarch64-apple-darwin)      echo "macos arm64" ;;
        x86_64-apple-darwin)       echo "macos x64" ;;
        x86_64-unknown-linux-gnu)  echo "linux x64" ;;
        aarch64-unknown-linux-gnu) echo "linux arm64" ;;
        *) return 1 ;;
    esac
}

if [ -z "$target" ]; then
    host_os_arch="$(uname -sm)"
    case "$host_os_arch" in
        "Darwin arm64")  target="aarch64-apple-darwin" ;;
        "Darwin x86_64") target="x86_64-apple-darwin" ;;
        "Linux aarch64"|"Linux arm64") target="aarch64-unknown-linux-gnu" ;;
        "Linux x86_64")  target="x86_64-unknown-linux-gnu" ;;
        *)
            echo "error: cannot auto-detect a supported --target for host '$host_os_arch'; pass --target explicitly" >&2
            exit 1
            ;;
    esac
    echo "== no --target given, auto-detected '$target' from host '$host_os_arch' ==" >&2
fi

if ! os_arch_pair="$(target_os_arch "$target")"; then
    echo "error: unsupported --target '$target'" >&2
    usage
    exit 1
fi
pkg_os="${os_arch_pair%% *}"
pkg_arch="${os_arch_pair##* }"

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/^version = "([^"]*)"$/\1/')"
if [ -z "$version" ]; then
    echo "error: could not read package version from Cargo.toml" >&2
    exit 1
fi

echo "== building sin90 $version ($target, release) ==" >&2
rustup target add "$target" >/dev/null 2>&1 || true
cargo build --release --locked --target "$target"

bin_path="target/${target}/release/sin90"
if [ ! -x "$bin_path" ]; then
    echo "error: expected an executable release binary at $bin_path" >&2
    exit 1
fi

pkg_name="sin90-${version}-${pkg_os}-${pkg_arch}"
dist_dir="dist"
stage_dir="${dist_dir}/${pkg_name}"
tarball="${pkg_name}.tar.gz"

# Reset dist/ so a stale tarball or SHA256SUMS from a previous version never
# lingers alongside this run's output.
rm -rf "$dist_dir"
mkdir -p "${stage_dir}/bin"

# On any failure, leave no half-built package behind: the notarization zip, the
# staged directory (owner read-only once generate ran), a partial tarball and
# SHA256SUMS.
# notarize-submission.json is kept for diagnosis. Preserves the exit status.
cleanup_on_failure() {
    local status=$?
    set +e
    if [ "$status" -ne 0 ]; then
        rm -f "${dist_dir}/.sin90-notarize.zip" "${dist_dir}/${tarball}" "${dist_dir}/SHA256SUMS" \
            || echo "warning: could not remove partial outputs in ${dist_dir}" >&2
        if [ -d "$stage_dir" ]; then
            { chmod -R u+w "$stage_dir" && rm -rf "$stage_dir"; } \
                || echo "warning: could not remove ${stage_dir}" >&2
        fi
    fi
    exit "$status"
}
trap cleanup_on_failure EXIT

cp domain-os.yml "${stage_dir}/domain-os.yml"
cp "$bin_path" "${stage_dir}/bin/sin90"
chmod +x "${stage_dir}/bin/sin90"

# S1-9 (Agent24 ADR-038 D10, design S1-9-OS-PACKAGE-INTEGRITY-DESIGN.md §5–§6):
# the order is fixed — every byte-changing step (signing) before the manifest
# is generated, the manifest before tar. Signing after `generate` would change
# bin/sin90's sha256 and the package would be refused on scan.
if [ "$pkg_os" = "macos" ] && [ "${SKIP_SIGN:-0}" != "1" ]; then
    sign_identity="${SIN90_SIGN_IDENTITY:-Developer ID Application: Huifeng Jiao (HYAP444FN9)}"
    notary_profile="${SIN90_NOTARY_PROFILE:-apple-notary-HYAP444FN9}"
    echo "== signing bin/sin90 (Developer ID, Hardened Runtime) ==" >&2
    codesign --force --options runtime --timestamp --sign "$sign_identity" "${stage_dir}/bin/sin90"
    codesign --verify --strict --verbose=1 "${stage_dir}/bin/sin90"
    if [ "${SKIP_NOTARIZE:-0}" != "1" ]; then
        echo "== notarizing bin/sin90 ==" >&2
        notary_zip="${dist_dir}/.sin90-notarize.zip"
        ditto -c -k --keepParent "${stage_dir}/bin/sin90" "$notary_zip"
        # A bare Mach-O cannot be stapled; Gatekeeper looks the ticket up online.
        xcrun notarytool submit "$notary_zip" --keychain-profile "$notary_profile" --wait \
            --output-format json > "${dist_dir}/notarize-submission.json"
        rm -f "$notary_zip"
        grep -q '"status" *: *"Accepted"' "${dist_dir}/notarize-submission.json" || {
            echo "error: notarization was not accepted, see ${dist_dir}/notarize-submission.json" >&2
            exit 1
        }
    fi
elif [ "$pkg_os" = "macos" ]; then
    echo "warning: SKIP_SIGN=1 — this macOS package is unsigned; Agent24 will refuse it once signing is enforced" >&2
fi

# The integrity manifest is generated by the Agent24 CLI of the release this
# package targets (AGENT24_CLI, default: `agent24` on PATH). It writes
# files.json, rewrites domain-os.yml with the `integrity:` section and sets the
# modes Agent24 checks (dirs 0700, bin/sin90 0500, the rest 0400).
agent24_cli="${AGENT24_CLI:-agent24}"
command -v "$agent24_cli" >/dev/null 2>&1 || {
    echo "error: AGENT24_CLI '$agent24_cli' not found; it must be an Agent24 CLI with 'package-integrity'" >&2
    exit 1
}
echo "== generating the integrity manifest with $agent24_cli ==" >&2
# Building for another CPU (an x86_64-apple-darwin package on an Apple-silicon
# Mac): tell the CLI which architecture the executable is for. A same-CPU
# build passes nothing, so an older pinned Agent24 CLI without --arch still works.
target_cpu="${target%%-*}"
case "$(uname -m)" in
    arm64|aarch64) host_cpu=aarch64 ;;
    x86_64|amd64)  host_cpu=x86_64 ;;
    *)             host_cpu="$(uname -m)" ;;
esac
arch_args=()
if [ "$target_cpu" != "$host_cpu" ]; then
    arch_args=(--arch "$target_cpu")
fi
"$agent24_cli" package-integrity generate --root "$stage_dir" --executable bin/sin90 ${arch_args[@]+"${arch_args[@]}"}
"$agent24_cli" package-integrity verify --root "$stage_dir" ${arch_args[@]+"${arch_args[@]}"}

echo "== packing ${tarball} ==" >&2
# COPYFILE_DISABLE: macOS tar would otherwise add AppleDouble `._*` entries for
# extended attributes — extra files the integrity check refuses.
( cd "$dist_dir" && COPYFILE_DISABLE=1 tar -czf "$tarball" "$pkg_name" )

# The staged directory only existed to feed tar; the tarball is the artifact.
# generate left it owner read-only (0400/0500/0700), so make it removable first.
chmod -R u+w "$stage_dir"
rm -rf "$stage_dir"

echo "== checksums ==" >&2
( cd "$dist_dir" && shasum -a 256 "$tarball" > SHA256SUMS )

echo "== self-check: SHA256SUMS verifies ==" >&2
( cd "$dist_dir" && shasum -a 256 -c SHA256SUMS )

echo "== self-check: tarball contains exactly the expected files ==" >&2
actual_entries="$(cd "$dist_dir" && tar -tzf "$tarball" | grep -v '/$' | sort)"
expected_entries="$(printf '%s\n' "${pkg_name}/domain-os.yml" "${pkg_name}/files.json" "${pkg_name}/bin/sin90" | sort)"

if [ "$actual_entries" != "$expected_entries" ]; then
    echo "error: tarball file list does not match expected set" >&2
    echo "expected:" >&2
    echo "$expected_entries" >&2
    echo "actual:" >&2
    echo "$actual_entries" >&2
    exit 1
fi

echo "== self-check: binary architecture ==" >&2
case "$pkg_os" in
    macos)
        archs="$(lipo -archs "$bin_path")"
        echo "archs: $archs" >&2
        case "$pkg_arch" in
            arm64) echo "$archs" | grep -qw arm64 ;;
            x64)   echo "$archs" | grep -qw x86_64 ;;
        esac
        ;;
    linux)
        file_out="$(file "$bin_path")"
        echo "$file_out" >&2
        case "$pkg_arch" in
            arm64) echo "$file_out" | grep -qi "aarch64" ;;
            x64)   echo "$file_out" | grep -Eqi "x86-64|x86_64" ;;
        esac
        ;;
esac

echo "OK: ${dist_dir}/${tarball} (+ ${dist_dir}/SHA256SUMS)" >&2
