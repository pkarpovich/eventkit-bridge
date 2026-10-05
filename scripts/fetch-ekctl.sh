#!/usr/bin/env bash
set -euo pipefail

readonly EKCTL_VERSION="v1.8.0"
readonly EKCTL_URL="https://github.com/schappim/ekctl/releases/download/${EKCTL_VERSION}/ekctl-${EKCTL_VERSION}.tar.gz"
readonly EKCTL_SHA256="4e38314154e7df79c7e3eaa48f6330699ad2a6808ae033d9bd7510740787146a"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out-dir>" >&2
  exit 2
fi

out_dir="$1"
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

archive_name="ekctl-${EKCTL_VERSION}.tar.gz"
archive="${out_dir}/${archive_name}"
extract_dir="${out_dir}/ekctl-${EKCTL_VERSION}"

curl -fsSL -o "$archive" "$EKCTL_URL"

if ! (cd "$out_dir" && echo "${EKCTL_SHA256}  ${archive_name}" | shasum -a 256 -c - >&2); then
  echo "sha256 mismatch for ${EKCTL_URL}" >&2
  rm -f "$archive"
  exit 1
fi

rm -rf "$extract_dir"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"

binary=""
while IFS= read -r candidate; do
  binary="$candidate"
  break
done < <(find "$extract_dir" -type f -name ekctl)

if [[ -z "$binary" ]]; then
  echo "no ekctl binary in ${archive_name}" >&2
  exit 1
fi

chmod 755 "$binary"
echo "$binary"
