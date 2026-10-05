#!/usr/bin/env bash
set -euo pipefail

readonly REMINDCTL_VERSION="v0.3.8"
readonly REMINDCTL_URL="https://github.com/openclaw/remindctl/releases/download/${REMINDCTL_VERSION}/remindctl-macos.zip"
readonly REMINDCTL_SHA256="b1a7ff303bea4fba7dd3c63416e8203ecef51769e87dcb464e8bbf9822363d52"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out-dir>" >&2
  exit 2
fi

out_dir="$1"
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

archive_name="remindctl-${REMINDCTL_VERSION}-macos.zip"
archive="${out_dir}/${archive_name}"
extract_dir="${out_dir}/remindctl-${REMINDCTL_VERSION}"

curl -fsSL -o "$archive" "$REMINDCTL_URL"

if ! (cd "$out_dir" && echo "${REMINDCTL_SHA256}  ${archive_name}" | shasum -a 256 -c - >&2); then
  echo "sha256 mismatch for ${REMINDCTL_URL}" >&2
  rm -f "$archive"
  exit 1
fi

rm -rf "$extract_dir"
mkdir -p "$extract_dir"
unzip -q "$archive" -d "$extract_dir"

binary=""
while IFS= read -r candidate; do
  binary="$candidate"
  break
done < <(find "$extract_dir" -type f -name remindctl)

if [[ -z "$binary" ]]; then
  echo "no remindctl binary in ${archive_name}" >&2
  exit 1
fi

chmod 755 "$binary"
echo "$binary"
