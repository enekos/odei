#!/bin/sh
# Install or update odei.
#
#   curl -fsSL https://raw.githubusercontent.com/enekos/odei/master/install.sh | sh
#
# What it does: works out which release asset fits this machine, downloads it
# over HTTPS, checks it against the SHA-256 published in the same release, and
# moves the binary into place. No sudo, nothing outside the install directory,
# and no shell code is ever downloaded and run — only a tarball.
#
# Run it again to update; it stops early when you are already current.
#
# Options, as flags or environment variables:
#   --version <tag>   ODEI_VERSION      a release tag, e.g. v0.1.0 (default: latest)
#   --dir <path>      ODEI_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   --force           ODEI_FORCE=1      reinstall even if the version matches
#
# Piped to sh, flags need a separator:
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0

set -eu

REPO="enekos/odei"
VERSION="${ODEI_VERSION:-}"
INSTALL_DIR="${ODEI_INSTALL_DIR:-}"
FORCE="${ODEI_FORCE:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
    --dir) INSTALL_DIR="${2:?--dir needs a path}"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,25p' "$0" 2>/dev/null || echo "see https://github.com/$REPO"; exit 0 ;;
    *) echo "install.sh: unknown option: $1" >&2; exit 2 ;;
  esac
done

: "${INSTALL_DIR:=$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# --- fetching -----------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --proto '=https' --tlsv1.2 "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL --proto '=https' --tlsv1.2 "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  die "needs curl or wget"
fi

# --- which build ---------------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) target="" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
      *) target="" ;;
    esac ;;
  *) target="" ;;
esac

# --- from source, when there is no build for this machine ----------------

from_source() {
  say "No prebuilt odei for ${os}/${arch}."
  command -v cargo >/dev/null 2>&1 || die "install Rust (https://rustup.rs) and re-run, or build from a clone"
  say "Building from source with cargo — this takes a minute."
  if [ -n "$VERSION" ]; then
    cargo install --git "https://github.com/$REPO" --tag "$VERSION" --locked odei
  else
    cargo install --git "https://github.com/$REPO" --locked odei
  fi
  say "Installed to ${CARGO_HOME:-$HOME/.cargo}/bin/odei"
  exit 0
}

[ -n "$target" ] || from_source

# --- which version -------------------------------------------------------

if [ -z "$VERSION" ]; then
  # The redirect on /releases/latest names the tag, so this needs no JSON
  # parsing and no API token.
  VERSION="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$VERSION" ] || die "could not work out the latest version; pass --version <tag>"
fi

installed=""
if [ -x "$INSTALL_DIR/odei" ]; then
  installed="$("$INSTALL_DIR/odei" version 2>/dev/null | awk '{print $2}')"
elif command -v odei >/dev/null 2>&1; then
  installed="$(odei version 2>/dev/null | awk '{print $2}')"
fi
if [ -n "$installed" ] && [ "v$installed" = "$VERSION" ] && [ -z "$FORCE" ]; then
  say "odei $installed is already the latest ($VERSION). Use --force to reinstall."
  exit 0
fi

# --- download, verify, install -------------------------------------------

name="odei-${VERSION}-${target}"
base="https://github.com/$REPO/releases/download/$VERSION"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/odei-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $name"
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" \
  || die "no release asset $name.tar.gz — check https://github.com/$REPO/releases"
fetch "$base/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" \
  || die "release $VERSION has no checksum for $target; refusing to install unverified"

if command -v sha256sum >/dev/null 2>&1; then
  checksum() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  checksum() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "needs sha256sum or shasum to verify the download"
fi

expected="$(awk '{print $1}' "$tmp/$name.tar.gz.sha256")"
actual="$(checksum "$tmp/$name.tar.gz")"
if [ "$expected" != "$actual" ]; then
  die "checksum mismatch for $name.tar.gz
  expected $expected
  got      $actual
Nothing was installed."
fi
say "Checksum verified."

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
[ -f "$tmp/$name/odei" ] || die "the archive did not contain an odei binary"

mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || die "$INSTALL_DIR is not writable; pass --dir <path>"
chmod +x "$tmp/$name/odei"
# Into place in one step, so a half-written binary is never on PATH — and via
# a rename in the same directory, which also works while the old one is
# running.
mv "$tmp/$name/odei" "$INSTALL_DIR/odei.new"
mv "$INSTALL_DIR/odei.new" "$INSTALL_DIR/odei"

say ""
say "odei ${VERSION#v} installed to $INSTALL_DIR/odei"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say ""
    say "$INSTALL_DIR is not on your PATH. Add it:"
    say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc   # or ~/.bashrc"
    ;;
esac

say ""
say "Next: odei setup     (stores a Kimi Coding-plan key in ~/.odei/config.json)"
say "      odei doctor    (checks the key, the profile and live connectivity)"
say "      odei           (start a session in the current directory)"
