#!/bin/sh
# Installs the latest ynab-mcp release binary for this machine into ~/.local/bin.
#   curl -fsSL https://raw.githubusercontent.com/mattCasanova/ynab-mcp-server/master/install.sh | sh
# Options via env: YNAB_MCP_VERSION=v0.1.1 (default: latest), YNAB_MCP_INSTALL_DIR=/some/dir
set -eu

repo="mattCasanova/ynab-mcp-server"
install_dir="${YNAB_MCP_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) echo "unsupported macOS arch: $arch" >&2; exit 1 ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64)        target="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
            *) echo "unsupported Linux arch: $arch" >&2; exit 1 ;;
          esac ;;
  *) echo "unsupported OS: $os (Windows: download the .zip from https://github.com/$repo/releases)" >&2; exit 1 ;;
esac

if [ -n "${YNAB_MCP_VERSION:-}" ]; then
  tag="$YNAB_MCP_VERSION"
else
  tag="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$tag" ] || { echo "could not determine the latest release" >&2; exit 1; }
fi
version="${tag#v}"
name="ynab-mcp-${version}-${target}"
base="https://github.com/$repo/releases/download/$tag"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "downloading $name.tar.gz"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/expected.sha256"

expected="$(awk '{print $1}' "$tmp/expected.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$name.tar.gz" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || { echo "checksum mismatch; refusing to install" >&2; exit 1; }

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$install_dir"
install -m 755 "$tmp/$name/ynab-mcp" "$install_dir/ynab-mcp"
echo "installed $install_dir/ynab-mcp ($tag)"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "note: $install_dir is not on your PATH; add it, or register the full path with Claude Code" ;;
esac
echo
echo "next: $install_dir/ynab-mcp setup"
