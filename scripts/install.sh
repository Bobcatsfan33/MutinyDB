#!/bin/sh
set -eu

repo="${MUTINYDB_REPOSITORY:-Bobcatsfan33/MutinyDB}"
version="${MUTINYDB_VERSION:-0.1.0}"
install_dir="${MUTINYDB_INSTALL_DIR:-/usr/local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform="linux-x86_64" ;;
  Darwin-arm64) platform="darwin-arm64" ;;
  *) echo "MutinyDB does not publish a binary for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

archive="mutinydb-${platform}.tar.gz"
base="https://github.com/${repo}/releases/download/mutinydb-v${version}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -fsSL "$base/$archive" -o "$tmp/$archive"
curl --proto '=https' --tlsv1.2 -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256"
(
  cd "$tmp"
  if command -v sha256sum >/dev/null 2>&1; then sha256sum -c "$archive.sha256"; else shasum -a 256 -c "$archive.sha256"; fi
  tar -xzf "$archive"
)

if [ -w "$install_dir" ]; then
  install -m 0755 "$tmp/mutinyd" "$install_dir/mutinyd"
else
  sudo install -m 0755 "$tmp/mutinyd" "$install_dir/mutinyd"
fi
echo "installed mutinyd ${version} to ${install_dir}/mutinyd"
