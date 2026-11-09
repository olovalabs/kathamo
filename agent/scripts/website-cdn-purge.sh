#!/usr/bin/env bash
# Provider-specific script is kept outside the repository and receives both domains.
set -euo pipefail

if [[ -z "${RPI_CDN_PURGE_SCRIPT:-}" ]]; then
  echo 'CDN purge skipped: RPI_CDN_PURGE_SCRIPT is unset (edge content TTL: 60s, assets: 300s, images: 1d).'
  exit 0
fi

if [[ ! -f "$RPI_CDN_PURGE_SCRIPT" ]]; then
  echo "CDN purge script not found: $RPI_CDN_PURGE_SCRIPT" >&2
  exit 1
fi

# The script must wait for purge completion and exit nonzero on any API failure.
bash "$RPI_CDN_PURGE_SCRIPT" "$@"
