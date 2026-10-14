#!/bin/sh
# Install script for the todo CLI (https://github.com/aaberg/todo)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/aaberg/todo/main/install.sh | sh
#
# Options (when run from a terminal):
#   --version <tag>   install a specific release tag (e.g. 0.1.0); default: latest
#   --to <dir>        installation directory; default: ~/.local/bin (or
#                     /usr/local/bin when running as root)
#   --no-checksum     skip SHA256 verification (not recommended)

set -eu

REPO="aaberg/todo"
VERSION="latest"
DEST=""
VERIFY_CHECKSUM=1

usage() {
    cat <<EOF
Usage: install.sh [--version <tag>] [--to <dir>] [--no-checksum]

Installs the todo CLI from GitHub releases.

  --version <tag>   install a specific release tag (e.g. 0.1.0); default: latest
  --to <dir>        install into <dir> (default: ~/.local/bin, or /usr/local/bin as root)
  --no-checksum     skip SHA256 verification (not recommended)
  -h, --help        show this help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || { echo "error: --version requires an argument" >&2; exit 1; }
            VERSION="$2"; shift 2 ;;
        --to)
            [ $# -ge 2 ] || { echo "error: --to requires an argument" >&2; exit 1; }
            DEST="$2"; shift 2 ;;
        --no-checksum)
            VERIFY_CHECKSUM=0; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            echo "error: unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

say() { printf '%s\n' "$*"; }
err() { printf 'install.sh: %s\n' "$*" >&2; }
need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required command not found: $1"
        exit 1
    fi
}

need uname
need tar
for cmd in curl wget; do
    if command -v "$cmd" >/dev/null 2>&1; then
        DOWNLOADER="$cmd"
        break
    fi
done
[ -n "${DOWNLOADER:-}" ] || { err "need curl or wget to download"; exit 1; }

if [ "$VERIFY_CHECKSUM" = "1" ]; then
    if command -v shasum >/dev/null 2>&1; then
        :
    elif command -v sha256sum >/dev/null 2>&1; then
        :
    else
        err "need shasum or sha256sum to verify the download"
        err "install with --no-checksum to skip verification (not recommended)"
        exit 1
    fi
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-linux-musl"; SUFFIX="tar.gz" ;;
            aarch64|arm64)
                err "Linux ARM64 binaries are not yet available."
                err "You can build from source: cargo install --git https://github.com/$REPO todo"
                exit 1 ;;
            *)
                err "unsupported architecture: $ARCH"; exit 1 ;;
        esac ;;
    Darwin)
        case "$ARCH" in
            x86_64|arm64|aarch64) TARGET="universal-apple-darwin"; SUFFIX="tar.gz" ;;
            *)
                err "unsupported architecture: $ARCH"; exit 1 ;;
        esac ;;
    *)
        err "unsupported operating system: $OS (this installer covers Linux and macOS)"; exit 1 ;;
esac

case "$TARGET" in
    *-linux-*)        PLATFORM="linux" ;;
    *-apple-darwin)  PLATFORM="macos" ;;
esac

ARCHIVE="todo-$TARGET.$SUFFIX"
SUMS_FILE="SHA256SUMS-$PLATFORM"

if [ "$VERSION" = "latest" ]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
    RELEASE_REF="latest"
else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
    RELEASE_REF="$VERSION"
fi

# Default install location: ~/.local/bin for users, /usr/local/bin for root.
if [ -z "$DEST" ]; then
    if [ "$(id -u)" = "0" ]; then
        DEST="/usr/local/bin"
    else
        DEST="${HOME}/.local/bin"
    fi
fi
mkdir -p "$DEST" || { err "cannot create install directory: $DEST"; exit 1; }

TMPDIR_INSTALL="$(mktemp -d "$DEST/tmp.XXXXXX" 2>/dev/null || mktemp -d "${TMPDIR:-/tmp}/todo-install.XXXXXX")"
trap 'rm -rf "$TMPDIR_INSTALL"' EXIT

fetch() {
    # fetch <url> <output-file>
    case "$DOWNLOADER" in
        curl) curl -fsSL "$1" -o "$2" ;;
        wget) wget -qO "$2" "$1" ;;
    esac
}

say "==> Installing todo CLI ($RELEASE_REF) for $OS $ARCH"

say "--> Downloading $ARCHIVE"
fetch "$BASE_URL/$ARCHIVE" "$TMPDIR_INSTALL/$ARCHIVE"

if [ "$VERIFY_CHECKSUM" = "1" ]; then
    say "--> Downloading $SUMS_FILE"
    if ! fetch "$BASE_URL/$SUMS_FILE" "$TMPDIR_INSTALL/$SUMS_FILE"; then
        err "could not download $SUMS_FILE; aborting"
        err "the download cannot be verified; install with --no-checksum to skip verification (not recommended)"
        exit 1
    fi
fi

if [ "$VERIFY_CHECKSUM" = "1" ]; then
    say "--> Verifying SHA256 checksum"
    WANT="$(grep " $ARCHIVE\$" "$TMPDIR_INSTALL/$SUMS_FILE" | awk '{print $1}')"
    if [ -z "$WANT" ]; then
        err "$ARCHIVE not listed in $SUMS_FILE; aborting"
        exit 1
    fi
    if command -v shasum >/dev/null 2>&1; then
        GOT="$(shasum -a 256 "$TMPDIR_INSTALL/$ARCHIVE" | awk '{print $1}')"
    else
        GOT="$(sha256sum "$TMPDIR_INSTALL/$ARCHIVE" | awk '{print $1}')"
    fi
    if [ "$WANT" != "$GOT" ]; then
        err "checksum mismatch for $ARCHIVE"
        err "  expected: $WANT"
        err "  got:      $GOT"
        err "The download may be corrupted or tampered with; aborting."
        exit 1
    fi
fi

say "--> Extracting"
tar -xzf "$TMPDIR_INSTALL/$ARCHIVE" -C "$TMPDIR_INSTALL"

BIN="$TMPDIR_INSTALL/todo"
[ -f "$BIN" ] || { err "binary 'todo' not found in archive"; exit 1; }

say "--> Installing to $DEST/todo"
chmod 755 "$BIN"
mv "$BIN" "$DEST/todo"

INSTALLED="$DEST/todo"

say ""
say "Installed: $INSTALLED"
if "$INSTALLED" --version >/dev/null 2>&1; then
    say "Version:  $("$INSTALLED" --version 2>/dev/null || echo unknown)"
else
    say "Version:  $RELEASE_REF"
fi

case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        say ""
        say "WARNING: $DEST is not in your PATH."
        say "Add it to your shell profile, e.g.:"
        say "  echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.profile  # or ~/.bashrc / ~/.zshrc"
        ;;
esac

say ""
say "Done. Try: todo --help"
