#!/bin/sh
# Installs a prebuilt `sand` executable from GitHub Releases (Linux, macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/insertt/sand/main/install.sh | sh
#
# Environment:
#   SAND_VERSION      release to install, e.g. v0.1.0 (default: latest)
#   SAND_INSTALL_DIR  where to put the executable (default: ~/.local/bin)
set -eu

REPO="insertt/sand"
VERSION="${SAND_VERSION:-latest}"
INSTALL_DIR="${SAND_INSTALL_DIR:-$HOME/.local/bin}"

die() {
    echo "install.sh: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Linux) os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) die "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) die "unsupported architecture $(uname -m)" ;;
esac

asset="sand-$arch-$os.tar.gz"
if [ "$VERSION" = latest ]; then
    url="https://github.com/$REPO/releases/latest/download/$asset"
else
    url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "curl or wget is required"
    fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $url"
fetch "$url" "$tmp/$asset" || die "download failed: $url"
fetch "$url.sha256" "$tmp/$asset.sha256" || die "download failed: $url.sha256"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$asset.sha256" >/dev/null) || die "checksum mismatch for $asset"
elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$asset.sha256" >/dev/null) || die "checksum mismatch for $asset"
else
    die "sha256sum or shasum is required to verify the download"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
# Move into place via a rename so a running `sand` is never half-overwritten.
cp "$tmp/sand" "$INSTALL_DIR/.sand.new"
chmod 755 "$INSTALL_DIR/.sand.new"
mv -f "$INSTALL_DIR/.sand.new" "$INSTALL_DIR/sand"

echo "Installed $("$INSTALL_DIR/sand" --version) to $INSTALL_DIR/sand"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Note: $INSTALL_DIR is not on your PATH; add it to run \`sand\`." ;;
esac
