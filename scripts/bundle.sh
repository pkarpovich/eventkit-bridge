#!/usr/bin/env bash
set -euo pipefail

readonly BUNDLE_ID="dev.pkarpovich.eventkit-bridge"
readonly APP_NAME="EventKitBridge.app"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "$0 builds a macOS app bundle and runs only on macOS" >&2
  exit 1
fi

if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "usage: $0 <binary> <ekctl> <out-dir> [identity]" >&2
  exit 2
fi

binary="$1"
ekctl="$2"
out_dir="$3"
identity="${4:-}"

root="$(cd "$(dirname "$0")/.." && pwd)"
entitlements="${root}/entitlements.plist"

version="$(awk -F '"' '/^\[/ { section = $0 } section == "[package]" && /^version *=/ { print $2; exit }' "${root}/Cargo.toml")"
if [[ -z "$version" ]]; then
  echo "no package version in ${root}/Cargo.toml" >&2
  exit 1
fi

mkdir -p "$out_dir"
app="$(cd "$out_dir" && pwd)/${APP_NAME}"

rm -rf "$app"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
install -m 755 "$binary" "${app}/Contents/MacOS/eventkit-bridge"
install -m 755 "$ekctl" "${app}/Contents/MacOS/ekctl"
install -m 644 "${root}/ekctl-LICENSE.txt" "${app}/Contents/Resources/ekctl-LICENSE.txt"
sed "s/__VERSION__/${version}/g" "${root}/Info.plist.template" > "${app}/Contents/Info.plist"
plutil -lint "${app}/Contents/Info.plist" >&2

if [[ -n "$identity" ]]; then
  codesign --force --sign "$identity" --options runtime --timestamp \
    --entitlements "$entitlements" --identifier "${BUNDLE_ID}.ekctl" \
    "${app}/Contents/MacOS/ekctl"
  codesign --force --sign "$identity" --options runtime --timestamp \
    --entitlements "$entitlements" --identifier "$BUNDLE_ID" \
    "$app"
  codesign --verify --strict --deep --verbose=2 "$app"
fi

echo "$app"
