#!/usr/bin/env bash
set -euo pipefail

mode="${1:-staged}"
shift || true

pattern='sk-[A-Za-z0-9_-]{16,}|sk-ant-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|gho_[A-Za-z0-9]{30,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}|hf_[A-Za-z0-9]{30,}|glpat-[A-Za-z0-9_-]{20,}|BEGIN (RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY|(API_KEY|APIKEY|AUTH_TOKEN|ACCESS_TOKEN|SECRET_KEY|CLIENT_SECRET|PASSWORD|PASSWD)[A-Z_]*[[:space:]]*[=:][[:space:]]*["'"'"']?[A-Za-z0-9_./+=-]{16,}|Bearer [A-Za-z0-9_./+=-]{20,}|eyJ[A-Za-z0-9_-]{20,}\.eyJ[A-Za-z0-9_-]{20,}'
allow='example|placeholder|redacted|<token>|<key>|\$\{?[A-Z_]+\}?|your[_-]?key|xxx+|dummy|scan-secrets'
forbidden='^(\.env|\.env\..*|.*\.env|.*\.env\.sh|env\.sh|.*\.key|.*\.pem|.*\.p12|.*\.pfx|.*\.token|.*\.secret|secrets/.*|credentials.*|id_rsa.*|id_ed25519.*)$'

case "$mode" in
  staged) diff=(git diff --cached --no-color -U0 --diff-filter=ACMR) ;;
  range)  diff=(git diff --no-color -U0 --diff-filter=ACMR "$1" "$2") ;;
  *) echo "usage: scan-secrets.sh staged | range <from> <to>" >&2; exit 2 ;;
esac

hits=0
for file in $("${diff[@]}" --name-only); do
  if printf '%s\n' "$file" | grep -Eq -e "$forbidden"; then
    printf 'forbidden file: %s\n' "$file" >&2
    hits=$((hits + 1))
    continue
  fi
  while IFS= read -r line; do
    printf 'secret? %s: %s\n' "$file" "$(printf '%s' "$line" | cut -c1-80)" >&2
    hits=$((hits + 1))
  done < <("${diff[@]}" -- "$file" | grep -E '^\+[^+]' | cut -c2- | grep -E -e "$pattern" | grep -Eiv -e "$allow" || true)
done

if [ "$hits" -gt 0 ]; then
  echo "scan-secrets: $hits finding(s); refusing. Use --no-verify only if every hit is a false positive." >&2
  exit 1
fi
