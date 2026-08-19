#!/usr/bin/env bash
set -euo pipefail

# The krun conformance run, for a machine this repository's dev box is not:
# Linux with /dev/kvm, or an Apple Silicon Mac with HVF. It prepares a root
# filesystem, builds the binary that runs `tenon sandbox vmm`, and runs the same
# `krun_backend_conformance` test that skips itself everywhere else.
#
#   scripts/krun-smoke.sh [image reference]        default: alpine:3.20

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
reference="${1:-alpine:3.20}"
home="${TENON_HOME:-$HOME/.tenon}"
name="${TENON_KRUN_IMAGE:-smoke}"

case "$(uname -s)" in
  Linux)
    [ -e /dev/kvm ] || { echo "krun-smoke: /dev/kvm absent; this host cannot run a microVM" >&2; exit 1; }
    [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "krun-smoke: /dev/kvm not readable and writable by $(id -un)" >&2; exit 1; }
    ;;
  Darwin)
    [ "$(sysctl -n kern.hv_support)" = "1" ] || { echo "krun-smoke: HVF unavailable" >&2; exit 1; }
    ;;
  *) echo "krun-smoke: krun runs on Linux and macOS only" >&2; exit 1 ;;
esac

echo "==> building tenon"
(cd "$root/rs" && cargo build --release)
binary="$root/rs/target/release/tenon"

rootfs="${TENON_KRUN_ROOTFS:-}"
if [ -z "$rootfs" ]; then
  echo "==> unpacking $reference into $home/images/$name"
  TENON_HOME="$home" "$binary" sandbox image pull "$reference" --name "$name"
  rootfs="$home/images/$name/rootfs"
fi
[ -d "$rootfs" ] || { echo "krun-smoke: $rootfs is not a directory" >&2; exit 1; }

echo "==> conformance"
cd "$root/rs"
TENON_BIN="$binary" TENON_KRUN_ROOTFS="$rootfs" \
  cargo test -p tenon-sandbox --test conformance -- --nocapture krun_backend_conformance
