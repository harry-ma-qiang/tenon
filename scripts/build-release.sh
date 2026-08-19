#!/usr/bin/env bash
set -euo pipefail

# The shipping shape of RFC section 3: one file. Builds the BEAM release, tars
# it, builds `tenon` with that tarball embedded by cli/build.rs, and leaves the
# binary and its sha256 in dist/.
#
#   scripts/build-release.sh                 -> dist/tenon-<os>-<arch>
#   scripts/build-release.sh --verify        -> plus a boot from a fresh home
#
# --verify starts the produced binary in a throwaway TENON_HOME with no
# TENON_RELEASE_DIR and no --release-dir, so the embedded payload is the only
# way it can find a BEAM release at all, and stops it again. Set
# TENON_VERIFY_BASE_URL to an OpenAI-compatible endpoint to have it run one
# `tenon run` turn against it as well.
#
# Requires: OTP + Elixir (mix), a stable Rust toolchain. Nothing else.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
verify=0
for arg in "$@"; do
  case "$arg" in
    --verify) verify=1 ;;
    *) echo "usage: build-release.sh [--verify]" >&2; exit 2 ;;
  esac
done

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "build-release: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
arch="$(uname -m)"

release="$root/beam/_build/prod/rel/tenon_beam"
dist="$root/dist"
target="$dist/tenon-$os-$arch"

echo "==> beam release"
(cd "$root/beam" && MIX_ENV=prod mix release --overwrite)
version="$(basename "$(ls -d "$release"/releases/*/ | head -1)")"
[ -x "$release/bin/tenon_beam" ] || { echo "build-release: $release/bin/tenon_beam missing" >&2; exit 1; }

echo "==> payload ($version)"
mkdir -p "$dist"
tar="$dist/tenon_beam-$version.tar.gz"
rm -f "$tar"
tar -czf "$tar" -C "$root/beam/_build/prod/rel" tenon_beam

echo "==> tenon"
(cd "$root/rs" && TENON_RELEASE_TAR="$tar" TENON_RELEASE_VERSION="$version" cargo build --release)

install -m 0755 "$root/rs/target/release/tenon" "$target"
rm -f "$tar"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist" && sha256sum "$(basename "$target")" > "$(basename "$target").sha256")
else
  (cd "$dist" && shasum -a 256 "$(basename "$target")" > "$(basename "$target").sha256")
fi

echo "==> $target"
ls -l "$target"
cat "$target.sha256"

[ "$verify" = 1 ] || exit 0

echo "==> verify: a fresh home, no release directory, the payload only"
home="$(mktemp -d "${TMPDIR:-/tmp}/tenon-release-XXXXXX")"
cleanup() {
  TENON_HOME="$home" "$target" stop --all >/dev/null 2>&1 || true
  rm -rf "$home"
}
trap cleanup EXIT
unset TENON_RELEASE_DIR
export TENON_HOME="$home"
if [ -n "${TENON_VERIFY_BASE_URL:-}" ]; then
  mkdir -p "$home/profiles/root"
  cat > "$home/profiles/root/harness.yml" <<YML
llm:
  provider: openai
  base_url: $TENON_VERIFY_BASE_URL
  model: fake-model
  api_key_env: TENON_VERIFY_NO_KEY
  retry_base_ms: 20
max_steps: 4
approval: deny
YML
fi
"$target" start

# The harness comes up after the node and the worker; `run` before that is an
# error, not a wait, so the verification does the waiting.
ready=0
for _ in $(seq 1 120); do
  if "$target" status | grep -A4 '"harness"' | grep -q '"state": "ready"'; then ready=1; break; fi
  sleep 1
done
"$target" status
[ "$ready" = 1 ] || { echo "build-release: the harness never became ready" >&2; exit 1; }

if [ -n "${TENON_VERIFY_BASE_URL:-}" ]; then
  "$target" run "reply with the single word pong" --timeout 180
fi
"$target" stop
trap - EXIT
rm -rf "$home"
echo "==> verified"
