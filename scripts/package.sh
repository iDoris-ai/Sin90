#!/usr/bin/env bash
# ME4-6.0.2 (Sin90 part) — build a release tarball for macOS/arm64.
#
# Produces, under dist/:
#   sin90-<ver>-macos-arm64.tar.gz  — unpacks to a top-level
#     sin90-<ver>-macos-arm64/ directory containing exactly domain-os.yml
#     (the repo-root manifest, NOT domain-os.remote-allowed.yml) and
#     bin/sin90 (the release binary, executable). That layout is what
#     `agent24 os install <dir>` expects: it copies the given directory
#     into the packages root verbatim and reads domain-os.yml's
#     `spawn.command` (here `bin/sin90`) relative to it — see
#     agent24-os-packages::install::install and this repo's domain-os.yml
#     (confirmed read-only against Agent24 rust/apps/agent24-cli/src/main.rs
#     ~L552-575, os_local/OsAction::Install).
#   SHA256SUMS — `shasum -a 256` over the tarball, run from inside dist/ so
#     the recorded filename is relative.
#
# The version is read from Cargo.toml's [package] version — the single
# source of truth — so this script never needs its own copy of "0.5.0".
#
# Ends with a self-check (sha256 verification + exact tarball file-list
# match) so a broken package fails the build instead of shipping quietly.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Cross-compiling a macOS/arm64 tarball from another host would silently
# produce a binary that cannot run where the package claims it can. Refuse
# instead of guessing.
os_arch="$(uname -sm)"
if [ "$os_arch" != "Darwin arm64" ]; then
    echo "error: scripts/package.sh only runs on Darwin arm64 (got: $os_arch)" >&2
    exit 1
fi

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/^version = "([^"]*)"$/\1/')"
if [ -z "$version" ]; then
    echo "error: could not read package version from Cargo.toml" >&2
    exit 1
fi

echo "== building sin90 $version (release) ==" >&2
cargo build --release

bin_path="target/release/sin90"
if [ ! -x "$bin_path" ]; then
    echo "error: expected an executable release binary at $bin_path" >&2
    exit 1
fi

pkg_name="sin90-${version}-macos-arm64"
dist_dir="dist"
stage_dir="${dist_dir}/${pkg_name}"
tarball="${pkg_name}.tar.gz"

# Reset dist/ so a stale tarball or SHA256SUMS from a previous version never
# lingers alongside this run's output.
rm -rf "$dist_dir"
mkdir -p "${stage_dir}/bin"

cp domain-os.yml "${stage_dir}/domain-os.yml"
cp "$bin_path" "${stage_dir}/bin/sin90"
chmod +x "${stage_dir}/bin/sin90"

echo "== packing ${tarball} ==" >&2
( cd "$dist_dir" && tar -czf "$tarball" "$pkg_name" )

# The staged directory only existed to feed tar; the tarball is the artifact.
rm -rf "$stage_dir"

echo "== checksums ==" >&2
( cd "$dist_dir" && shasum -a 256 "$tarball" > SHA256SUMS )

echo "== self-check: SHA256SUMS verifies ==" >&2
( cd "$dist_dir" && shasum -a 256 -c SHA256SUMS )

echo "== self-check: tarball contains exactly the expected files ==" >&2
actual_entries="$(cd "$dist_dir" && tar -tzf "$tarball" | grep -v '/$' | sort)"
expected_entries="$(printf '%s\n' "${pkg_name}/domain-os.yml" "${pkg_name}/bin/sin90" | sort)"

if [ "$actual_entries" != "$expected_entries" ]; then
    echo "error: tarball file list does not match expected set" >&2
    echo "expected:" >&2
    echo "$expected_entries" >&2
    echo "actual:" >&2
    echo "$actual_entries" >&2
    exit 1
fi

echo "OK: ${dist_dir}/${tarball} (+ ${dist_dir}/SHA256SUMS)" >&2
