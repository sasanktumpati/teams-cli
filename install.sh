#!/usr/bin/env sh
set -eu

REPO="sasanktumpati/teams-cli"
INSTALL_DIR="${TEAMS_CLI_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
if [ "$OS" != "Darwin" ]; then
  echo "teams-cli only supports macOS (detected: $OS)." >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
  x86_64) TARGET="x86_64-apple-darwin" ;;
  *)
    echo "Unsupported macOS architecture: $ARCH" >&2
    exit 1
    ;;
esac

if [ -n "${TEAMS_CLI_VERSION:-}" ]; then
  case "$TEAMS_CLI_VERSION" in
    v*) TAG="$TEAMS_CLI_VERSION" ;;
    *) TAG="v$TEAMS_CLI_VERSION" ;;
  esac
else
  LATEST_URL="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
  TAG="${LATEST_URL##*/}"
  if [ -z "$TAG" ] || [ "$TAG" = "latest" ] || [ "$TAG" = "releases" ]; then
    echo "Could not resolve latest release tag for $REPO." >&2
    exit 1
  fi
fi

ARCHIVE="teams-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/teams-cli-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading teams-cli $TAG ($TARGET)..."
curl -fsSL --proto '=https' --tlsv1.2 "$BASE_URL/$ARCHIVE" -o "$TMP/$ARCHIVE"
curl -fsSL --proto '=https' --tlsv1.2 "$BASE_URL/SHA256SUMS.txt" -o "$TMP/SHA256SUMS.txt"

EXPECTED="$(awk -v name="$ARCHIVE" '$2 == name || $2 == "*" name { print $1; exit }' "$TMP/SHA256SUMS.txt")"
if [ -z "$EXPECTED" ]; then
  echo "Checksum entry for $ARCHIVE not found in SHA256SUMS.txt." >&2
  exit 1
fi

ACTUAL="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{ print $1 }')"
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "SHA-256 checksum mismatch for $ARCHIVE." >&2
  echo "  expected: $EXPECTED" >&2
  echo "  actual:   $ACTUAL" >&2
  exit 1
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP" teams
if [ ! -f "$TMP/teams" ]; then
  echo "Archive $ARCHIVE did not contain teams binary." >&2
  exit 1
fi

if command -v xattr >/dev/null 2>&1; then
  xattr -d com.apple.quarantine "$TMP/teams" 2>/dev/null || true
fi

mkdir -p "$INSTALL_DIR"
DEST_TMP="$INSTALL_DIR/.teams.tmp.$$"
cp "$TMP/teams" "$DEST_TMP"
chmod 755 "$DEST_TMP"
mv -f "$DEST_TMP" "$INSTALL_DIR/teams"

echo "Installed $("$INSTALL_DIR/teams" --version) to $INSTALL_DIR/teams"

case ":${PATH}:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "Add $INSTALL_DIR to your PATH:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
