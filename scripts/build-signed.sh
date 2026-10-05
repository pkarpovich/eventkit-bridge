#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "$0 builds a signed macOS app bundle and runs only on macOS" >&2
  exit 1
fi

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <team-id>" >&2
  exit 2
fi

team_id="$1"
root="$(cd "$(dirname "$0")/.." && pwd)"
keychain="${HOME}/Library/Keychains/login.keychain-db"

identity="$(security find-identity -v -p codesigning "$keychain" |
  awk -v suffix="(${team_id})\"" 'index($0, "\"Developer ID Application: ") && substr($0, length($0) - length(suffix) + 1) == suffix { print $2; exit }')"
if [[ -z "$identity" ]]; then
  echo "no Developer ID Application (${team_id}) identity in ${keychain}" >&2
  exit 1
fi

cd "$root"
cargo build --release
ekctl="$("${root}/scripts/fetch-ekctl.sh" "${root}/target/ekctl")"
"${root}/scripts/bundle.sh" "${root}/target/release/eventkit-bridge" "$ekctl" "${root}/dist" "$identity"
