#!/usr/bin/env sh
set -eu

# POSIX-sh installer for notlin: fetch the release binary and drop it on the
# user PATH (~/.local/bin). No shell adapters, no rc-file edits.
# Documented invocation: curl -fsSL https://github.com/fralalonde/notlin/releases/latest/download/install.sh | sh

REPOSITORY="${NOTLIN_REPOSITORY:-fralalonde/notlin}"
DEST_DIR="${NOTLIN_DIR:-$HOME/.local/bin}"
VERSION="${NOTLIN_VERSION:-}"

if [ -z "$VERSION" ]; then
  VERSION="$(curl --fail --silent --show-error --location \
    "https://api.github.com/repos/$REPOSITORY/releases/latest" \
    | tr -d '\r' | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | head -n1)" || true
fi
[ -n "$VERSION" ] || { echo 'Unable to determine notlin version; set NOTLIN_VERSION=x.y.z.' >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=linux ;;
  *) echo "Unsupported platform: $(uname -s). Windows: install.ps1." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)
    # static musl build: runs on any Linux regardless of libc
    target=x86_64-unknown-linux-musl ;;
  arm64|aarch64) target=aarch64-unknown-linux-gnu ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

base="${NOTLIN_DOWNLOAD_BASE_URL:-https://github.com/$REPOSITORY/releases/download/v$VERSION}"
asset="notlin-$VERSION-$target"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl --fail --location --retry 3 --output "$tmp/$asset" "$base/$asset"

# Verify checksum when a checksums.txt is published with the release
# (best-effort: missing file is tolerated, mismatch is fatal).
if curl --fail --silent --show-error --location --output "$tmp/checksums.txt" "$base/checksums.txt"; then
  expected="$(grep -F " $asset" "$tmp/checksums.txt" | cut -d' ' -f1)"
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
    else
      actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
    fi
    [ "$actual" = "$expected" ] || { echo "checksum mismatch for $asset" >&2; exit 1; }
  fi
fi

mkdir -p "$DEST_DIR"
install -m 0755 "$tmp/$asset" "$DEST_DIR/notlin"

printf '\nInstalled notlin %s to %s/notlin\n' "$VERSION" "$DEST_DIR"
case ":$PATH:" in
  *":$DEST_DIR:"*) ;;
  *) printf 'Note: %s is not on your PATH.\n' "$DEST_DIR" >&2 ;;
esac
printf 'Run: notlin --help\n'
