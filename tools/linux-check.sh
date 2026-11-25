#!/usr/bin/env bash
# Linux build check from a Windows checkout, through WSL (D132): catches Windows-only calls
# before a deploy (the web image is built on Linux). Run from Git Bash or WSL:
#
#   rust/tools/linux-check.sh [wsl-distro]        # default distro: Ubuntu
#
# Needs rustup in the distro (per-user, no sudo):
#   curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
# The toolchain comes from rust-toolchain.toml. The target dir lives in the distro's home, so
# the Windows target/ is untouched. Checks the headless workspace (all targets, no `desktop`;
# the desktop feature is checked by CI's `rust-desktop` job, which has the GTK packages).
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)

if [ -f /proc/version ] && grep -qi microsoft /proc/version; then
  cd "$here"
  export CARGO_TARGET_DIR="$HOME/.cache/abhoth-linux-target"
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo check --workspace --all-targets --locked
  echo "LINUX CHECK OK"
  exit 0
fi

distro=${1:-Ubuntu}
win_path=$(cygpath -w "$here")
exec wsl.exe -d "$distro" -- bash -lc "cd \"\$(wslpath '$win_path')\" && bash tools/linux-check.sh"
