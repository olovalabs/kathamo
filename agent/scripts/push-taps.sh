#!/usr/bin/env bash
# Push the refreshed Homebrew formula + Scoop manifest into their tap repos.
#
#   bash scripts/push-taps.sh 0.3.4
#
# The files in packaging/ are the source of truth; the tap repos are copies.
# Uses the GitHub contents API (needs write access to both repos).

set -euo pipefail

VERSION="${1:?usage: push-taps.sh X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'invalid version' >&2; exit 1; }
CHANNEL="${2:-all}"
MSG="rpi $VERSION"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

# put REPO PATH LOCAL_FILE — create or update a file via the contents API.
put() {
  local repo="$1" path="$2" file="$3"
  [[ -f "$file" ]] || die "missing $file"
  local local_version
  local_version=$(sed -nE 's/^[[:space:]]*("version":|version) "([0-9.]+)".*/\2/p' "$file" | head -n1)
  [[ "$local_version" == "$VERSION" ]] || die "manifest version does not match $VERSION: $file"
  local content
  content=$(base64 <"$file" | tr -d '\r\n')
  local sha current remote_version
  current=$(gh api "repos/$repo/contents/$path" 2>/dev/null || true)
  sha=""
  if [[ -n "$current" ]]; then
    sha=$(jq -r .sha <<<"$current")
    if [[ $(jq -r .content <<<"$current" | tr -d '\r\n') == "$content" ]]; then
      note "$repo/$path already matches $VERSION"
      return
    fi
    remote_version=$(jq -r .content <<<"$current" | tr -d '\r\n' | base64 --decode \
      | sed -nE 's/^[[:space:]]*("version":|version) "([0-9.]+)".*/\2/p' | head -n1)
    if [[ -n "$remote_version" && "$remote_version" != "$VERSION" ]]; then
      [[ $(printf '%s\n%s\n' "$remote_version" "$VERSION" | sort -V | tail -n1) == "$VERSION" ]] \
        || die "refusing to downgrade $repo from $remote_version to $VERSION"
    fi
  fi
  if [[ -n "$sha" ]]; then
    note "update $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" -f sha="$sha" >/dev/null
  else
    note "create $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" >/dev/null
  fi
}

case "$CHANNEL" in
  all|homebrew|scoop) ;;
  *) die "unknown channel: $CHANNEL" ;;
esac
if [[ "$CHANNEL" == all || "$CHANNEL" == homebrew ]]; then
  put "bigfish1913/homebrew-tap" "Formula/rpi.rb" "packaging/homebrew/rpi.rb"
fi
if [[ "$CHANNEL" == all || "$CHANNEL" == scoop ]]; then
  put "bigfish1913/scoop-bucket" "bucket/rpi.json" "packaging/scoop/rpi.json"
fi

note "taps pushed"
