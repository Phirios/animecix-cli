#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
binary=$(realpath -- "${1:-$repo_dir/target/release/animecix}")
output_dir=$(realpath -m -- "${2:-$repo_dir/dist}")
build_dir=$(mktemp -d)
trap 'rm -rf -- "$build_dir"' EXIT

command -v rpmbuild >/dev/null || { echo 'Install rpm-build before packaging.' >&2; exit 1; }
test -x "$binary" || { echo 'Build the release binary first.' >&2; exit 1; }
mkdir -p "$build_dir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS} "$output_dir"
cp -- "$binary" "$build_dir/SOURCES/animecix"
cp -- "$repo_dir/README.md" "$repo_dir/LICENSE" "$build_dir/SOURCES/"
"$binary" --completions bash > "$build_dir/SOURCES/animecix.bash"
"$binary" --completions zsh > "$build_dir/SOURCES/_animecix"
"$binary" --completions fish > "$build_dir/SOURCES/animecix.fish"
rpmbuild -bb --define "_topdir $build_dir" "$repo_dir/packaging/animecix.spec"
find "$build_dir/RPMS" -type f -name '*.rpm' -exec cp -- {} "$output_dir/" \;
