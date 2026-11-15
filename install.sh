#!/bin/sh
# agentsflow installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/Venius24/agentsflow/master/install.sh | sh
#
# Downloads a release archive from GitHub, checks its SHA-256 against the
# SHA256SUMS file of the same release, and installs the `agentsflow` binary
# into a user directory. It never uses sudo and never edits your shell
# profile or PATH: it only tells you what to do.
#
# Environment variables:
#   AGENTSFLOW_VERSION       release tag to install, for example v0.1.0
#                            (default: the latest release)
#   AGENTSFLOW_INSTALL_DIR   target directory (default: $HOME/.local/bin)
#   AGENTSFLOW_DOWNLOAD_BASE URL (or file:// URL) of a folder that holds the
#                            archive and SHA256SUMS, for a mirror or a test

set -eu

REPO="Venius24/agentsflow"
BIN="agentsflow"

say() { printf '%s\n' "$*"; }
err() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

# Pick a downloader.
if command -v curl > /dev/null 2>&1; then
  fetch() { curl -fsSL --retry 2 -o "$2" "$1" && [ -s "$2" ]; }
elif command -v wget > /dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1" && [ -s "$2" ]; }
else
  err "need curl or wget to download the release"
fi

# Pick a SHA-256 tool.
if command -v sha256sum > /dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum > /dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  err "need sha256sum or shasum to verify the download"
fi

command -v tar > /dev/null 2>&1 || err "need tar to unpack the release"

# Which build do we need?
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "no prebuilt Linux binary for '$arch' (only x86_64). Build from source: https://github.com/$REPO#install" ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)
        target="x86_64-apple-darwin"
        # A shell running under Rosetta reports x86_64 on an Apple silicon Mac.
        if [ "$(sysctl -n hw.optional.arm64 2> /dev/null || true)" = "1" ]; then
          target="aarch64-apple-darwin"
        fi
        ;;
      *) err "no prebuilt macOS binary for '$arch'. Build from source: https://github.com/$REPO#install" ;;
    esac
    ;;
  *)
    err "unsupported system '$os'. On Windows use install.ps1; otherwise build from source: https://github.com/$REPO#install"
    ;;
esac

archive="$BIN-$target.tar.gz"
version="${AGENTSFLOW_VERSION:-latest}"
if [ -n "${AGENTSFLOW_DOWNLOAD_BASE:-}" ]; then
  base="$AGENTSFLOW_DOWNLOAD_BASE"
elif [ "$version" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$version"
fi

install_dir="${AGENTSFLOW_INSTALL_DIR:-}"
if [ -z "$install_dir" ]; then
  [ -n "${HOME:-}" ] || err "HOME is not set; set AGENTSFLOW_INSTALL_DIR"
  install_dir="$HOME/.local/bin"
fi

tmp=$(mktemp -d 2> /dev/null || mktemp -d -t agentsflow) || err "could not create a temporary directory"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

say "Platform:  $target"
say "Release:   $version"
say "Install to: $install_dir"
say ""

say "Downloading $archive ..."
fetch "$base/$archive" "$tmp/$archive" || err "could not download $base/$archive
  Either no release is published yet, this release has no build for your platform, or the network is down.
  Releases: https://github.com/$REPO/releases
  To build from source instead: https://github.com/$REPO#install"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "could not download $base/SHA256SUMS (refusing to install without a checksum)"

# SHA256SUMS lines look like "<hash>  <file>" (a leading "*" marks binary mode).
expected=$(awk -v f="$archive" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || err "$archive is not listed in SHA256SUMS"
actual=$(sha256_of "$tmp/$archive")
if [ "$expected" != "$actual" ]; then
  err "checksum mismatch for $archive
  expected: $expected
  actual:   $actual
Nothing was installed."
fi
say "Checksum OK ($actual)"

# Extract only the one file we need; the archive layout is <name>/<binary>.
member="$BIN-$target/$BIN"
mkdir -p "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x" "$member" || err "could not unpack $member from $archive"
[ -f "$tmp/x/$member" ] || err "$member is missing from $archive"

mkdir -p "$install_dir" || err "could not create $install_dir"
[ -w "$install_dir" ] || err "$install_dir is not writable. Choose another directory with AGENTSFLOW_INSTALL_DIR (this script never uses sudo)."

# Copy next to the destination first, then rename, so a running copy is not torn.
cp "$tmp/x/$member" "$install_dir/$BIN.new"
chmod 755 "$install_dir/$BIN.new"
mv -f "$install_dir/$BIN.new" "$install_dir/$BIN"

say "Installed $install_dir/$BIN"
if "$install_dir/$BIN" --version 2> /dev/null; then
  :
else
  say "Warning: the installed binary did not start with --version. Run it by hand to see why."
fi

# Is the directory on PATH?
case ":${PATH:-}:" in
  *":$install_dir:"*)
    say ""
    say "Done. Next: run 'agentsflow doctor' to check that git, claude and codex are found."
    ;;
  *)
    say ""
    say "$install_dir is not on your PATH. To use 'agentsflow' by name, add it:"
    case "$(basename "${SHELL:-sh}")" in
      zsh)  say "  echo 'export PATH=\"$install_dir:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
      bash) say "  echo 'export PATH=\"$install_dir:\$PATH\"' >> ~/.bashrc && exec bash" ;;
      fish) say "  fish_add_path $install_dir" ;;
      *)    say "  export PATH=\"$install_dir:\$PATH\"   # add this line to your shell profile" ;;
    esac
    say "This script did not change your PATH or any profile file."
    say ""
    say "Then run 'agentsflow doctor' to check that git, claude and codex are found."
    ;;
esac
