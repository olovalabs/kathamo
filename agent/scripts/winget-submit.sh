#!/usr/bin/env bash
# Open the winget-pkgs PR for a released version.
#
#   bash scripts/winget-submit.sh 0.3.4
#
# Server-side only (no 866 MB monorepo clone): fork, branch off upstream
# master, PUT the three manifests, open the PR. The winget-team moderator merge
# is external — this command cannot complete it.
#
# After it opens the PR, sign the CLA by commenting at column 0 with nothing
# else on the line:
#   @microsoft-github-policy-service agree
# (a leading space silently fails the match).

set -euo pipefail

VERSION="${1:?usage: winget-submit.sh X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'invalid version' >&2; exit 1; }

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

me=$(gh api user --jq .login) || die "gh is not authenticated"
branch="rpi-$VERSION"
dir="manifests/b/bigfish1913/rpi/$VERSION"
# Reuse submissions when a job is retried.
if gh api "repos/microsoft/winget-pkgs/contents/$dir" >/dev/null 2>&1; then
  note "bigfish1913.rpi $VERSION is already in Microsoft winget"
  exit 0
fi
# `gh pr list --head` accepts a bare branch, not owner:branch. Filter the
# repository owner explicitly so another fork's same-named branch cannot match.
submissions=$(gh pr list --repo microsoft/winget-pkgs --head "$branch" \
  --state all --json url,state,headRepositoryOwner)
existing=$(jq -c --arg owner "$me" \
  'map(select(.headRepositoryOwner.login == $owner)) | .[0] // empty' <<<"$submissions")
if [[ -n "$existing" ]]; then
  state=$(jq -r .state <<<"$existing")
  if [[ "$state" == MERGED ]]; then
    note "Already merged: $(jq -r .url <<<"$existing")"
    exit 0
  fi
  [[ "$state" != CLOSED ]] || die "Previous winget PR was closed; inspect it before resubmitting"
fi

if ! gh api "repos/$me/winget-pkgs/git/ref/heads/$branch" >/dev/null 2>&1; then
  if ! gh api "repos/$me/winget-pkgs" >/dev/null 2>&1; then
    note "fork $me/winget-pkgs (server-side)"
    gh repo fork microsoft/winget-pkgs --clone=false >/dev/null
  fi
  # Import upstream commits before creating a branch; never force a diverged fork.
  gh repo sync "$me/winget-pkgs" --source microsoft/winget-pkgs --branch master
  up=$(gh api "repos/$me/winget-pkgs/commits/master" --jq .sha)
  note "branch $branch off $up"
  gh api --method POST "repos/$me/winget-pkgs/git/refs" \
    -f ref="refs/heads/$branch" -f sha="$up" >/dev/null
fi

for f in \
  packaging/winget/bigfish1913.rpi.yaml \
  packaging/winget/bigfish1913.rpi.installer.yaml \
  packaging/winget/bigfish1913.rpi.locale.en-US.yaml; do
  name=$(basename "$f")
  [[ -f "$f" ]] || die "missing $f"
  grep -q "^PackageVersion: $VERSION$" "$f" || die "wrong manifest version: $f"
  content=$(base64 <"$f" | tr -d '\r\n')
  current=$(gh api "repos/$me/winget-pkgs/contents/$dir/$name?ref=$branch" 2>/dev/null || true)
  if [[ -n "$current" ]]; then
    remote=$(jq -r '.content' <<<"$current" | tr -d '\r\n')
    [[ "$remote" != "$content" ]] || continue
    sha_args=(-f "sha=$(jq -r .sha <<<"$current")")
  else
    sha_args=()
  fi
  note "put $dir/$name"
  gh api --method PUT "repos/$me/winget-pkgs/contents/$dir/$name" \
    -f message="New version: bigfish1913.rpi version $VERSION" \
    -f content="$content" -f branch="$branch" "${sha_args[@]}" >/dev/null
done

if [[ -n "$existing" ]]; then
  note "Updated existing PR: $(jq -r .url <<<"$existing")"
else
  note "open PR"
  gh pr create --repo microsoft/winget-pkgs --base master --head "$me:$branch" \
    --title "New version: bigfish1913.rpi version $VERSION" \
    --body "Adds bigfish1913.rpi $VERSION from bigfish1913/pi-rust (tag v$VERSION). The installer is the portable Windows zip."
fi
note "Submitted for Microsoft review; this does not mean the package is live."
note "If the policy bot requests a CLA, the maintainer must review and sign it."
