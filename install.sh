#!/usr/bin/env sh
set -eu

REPO="sasanktumpati/teams-cli"
INSTALL_DIR="${TEAMS_CLI_INSTALL_DIR:-$HOME/.local/bin}"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  BOLD="$(printf '\033[1m')"
  DIM="$(printf '\033[2m')"
  GREEN="$(printf '\033[32m')"
  YELLOW="$(printf '\033[33m')"
  RESET="$(printf '\033[0m')"
else
  BOLD=""
  DIM=""
  GREEN=""
  YELLOW=""
  RESET=""
fi

step() {
  printf "  ${DIM}%-12s${RESET} %s\n" "$1" "$2"
}

ok_step() {
  printf "  ${GREEN}%-12s${RESET} %s\n" "$1" "$2"
}

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

PREV_VERSION=""
if [ -x "$INSTALL_DIR/teams" ]; then
  PREV_VERSION="$("$INSTALL_DIR/teams" --version 2>/dev/null | awk '{ print $NF }' || true)"
fi

ARCHIVE="teams-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/teams-cli-install.XXXXXX")"
DEST_TMP=""
cleanup() {
  rm -rf "$TMP"
  if [ -n "$DEST_TMP" ] && [ -e "$DEST_TMP" ]; then
    rm -f "$DEST_TMP"
  fi
}
trap cleanup EXIT

printf "${BOLD}teams-cli %s${RESET} ${DIM}(%s)${RESET}\n" "$TAG" "$TARGET"
step "downloading" "$ARCHIVE"
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

SHORT_HASH="$(printf '%s' "$ACTUAL" | cut -c1-12)"
ok_step "verified" "sha256 ${SHORT_HASH}..."

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
DEST_TMP=""

NEW_VERSION="$("$INSTALL_DIR/teams" --version | awk '{ print $NF }')"
if [ -n "$PREV_VERSION" ] && [ "$PREV_VERSION" != "$NEW_VERSION" ]; then
  ok_step "installed" "$INSTALL_DIR/teams (v${PREV_VERSION} -> v${NEW_VERSION})"
elif [ -n "$PREV_VERSION" ]; then
  ok_step "installed" "$INSTALL_DIR/teams (reinstalled v${NEW_VERSION})"
else
  ok_step "installed" "$INSTALL_DIR/teams"
fi

PLIST="$HOME/Library/LaunchAgents/dev.teams-cli.daemon.plist"
if [ -f "$PLIST" ] && grep -Fq "<string>$INSTALL_DIR/teams</string>" "$PLIST" 2>/dev/null; then
  if launchctl print "gui/$(id -u)/dev.teams-cli.daemon" >/dev/null 2>&1; then
    if "$INSTALL_DIR/teams" daemon restart >/dev/null 2>&1; then
      ok_step "daemon" "restarted"
    fi
  fi
fi

NEED_PATH=0
case ":${PATH}:" in
  *":$INSTALL_DIR:"*) ;;
  *) NEED_PATH=1 ;;
esac

ACTIVE_BIN="$(command -v teams 2>/dev/null || true)"
if [ "$NEED_PATH" = "1" ]; then
  printf "\n${YELLOW}PATH${RESET}\n"
  printf "  export PATH=\"%s:\$PATH\"\n" "$INSTALL_DIR"
fi

if [ -n "$ACTIVE_BIN" ] && [ "$ACTIVE_BIN" != "$INSTALL_DIR/teams" ]; then
  printf "\n${YELLOW}Note${RESET}\n"
  printf "  Another 'teams' binary is earlier in PATH: %s\n" "$ACTIVE_BIN"
  printf "  Remove it or rerun with TEAMS_CLI_INSTALL_DIR=\"%s\"\n" "$(dirname "$ACTIVE_BIN")"
fi

if [ -z "$PREV_VERSION" ]; then
  printf "\n${BOLD}Next${RESET}\n"
  printf "  teams sync\n"
  printf "  teams daemon install\n"
  printf "  teams doctor\n"
fi
