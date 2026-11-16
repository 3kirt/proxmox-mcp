#!/usr/bin/env bash
# Bootstrap launcher for the proxmox-mcp Claude Code plugin.
#
# On first run (and after every plugin update) this downloads the release
# binary matching the plugin's version from GitHub releases, verifies its
# SHA-256 against the release's checksums.txt, caches it under
# ~/.cache/proxmox-mcp/<version>/, and execs it. Subsequent runs exec the
# cached binary directly. Whenever the download path can't produce a binary
# (unsupported platform, network failure, missing asset), a proxmox-mcp
# already on PATH is used instead.
#
# All diagnostics go to stderr: stdout is the MCP stdio transport.
set -euo pipefail

REPO="3kirt/proxmox-mcp"
# The server's own arguments (e.g. --config-dir), forwarded on exec. Saved
# here because "$@" inside a function means that function's arguments. The
# ${args[@]+...} form keeps bash 3.2 (macOS) happy under `set -u` when empty.
args=("$@")
PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { echo "proxmox-mcp plugin: $*" >&2; }

# Exec a proxmox-mcp from PATH if one exists, else explain and die.
# $1 is the reason the download path was abandoned.
fallback_or_die() {
  if command -v proxmox-mcp >/dev/null 2>&1; then
    say "$1; falling back to $(command -v proxmox-mcp)"
    exec proxmox-mcp ${args[@]+"${args[@]}"}
  fi
  say "$1"
  say "install manually with: cargo install --git https://github.com/$REPO"
  exit 1
}

# The binary version is pinned to the plugin version, so a plugin update
# (which ships a new plugin.json) also rolls the binary forward. The release
# workflow's version-lockstep check guarantees the matching release exists.
version="$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$PLUGIN_ROOT/.claude-plugin/plugin.json" | head -n 1)"
if [ -z "$version" ]; then
  fallback_or_die "cannot read version from $PLUGIN_ROOT/.claude-plugin/plugin.json"
fi

cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/proxmox-mcp/v$version"
bin="$cache_dir/proxmox-mcp"

if [ ! -x "$bin" ]; then
  case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux) os="linux" ;;
    *) fallback_or_die "no prebuilt binary for OS $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch="arm64" ;;
    x86_64 | amd64) arch="amd64" ;;
    *) fallback_or_die "no prebuilt binary for architecture $(uname -m)" ;;
  esac
  asset="proxmox-mcp-$os-$arch"

  base="https://github.com/$REPO/releases/download/v$version"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  say "downloading $asset v$version from GitHub releases..."
  if ! curl -fsSL --retry 2 --connect-timeout 10 --max-time 120 \
    -o "$tmp/$asset" "$base/$asset" ||
    ! curl -fsSL --retry 2 --connect-timeout 10 --max-time 30 \
      -o "$tmp/checksums.txt" "$base/checksums.txt"; then
    fallback_or_die "download failed (no release asset for $asset v$version?)"
  fi

  expected="$(awk -v a="$asset" '$2 == a { print $1 }' "$tmp/checksums.txt")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
  else
    actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
  fi
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    say "checksum mismatch for $asset (expected ${expected:-<missing>}, got $actual); aborting"
    exit 1
  fi

  # Stage within the cache directory so the final mv is an atomic rename on
  # the same filesystem — an interrupted install can never leave a partial
  # (but executable) file at $bin for the [ ! -x ] gate to trust forever.
  mkdir -p "$cache_dir"
  staged="$cache_dir/.proxmox-mcp.partial.$$"
  cp "$tmp/$asset" "$staged"
  chmod +x "$staged"
  mv -f "$staged" "$bin"
  # exec replaces the process, which skips the EXIT trap — clean up here.
  rm -rf "$tmp"
  trap - EXIT
  say "installed $bin"
fi

exec "$bin" ${args[@]+"${args[@]}"}
