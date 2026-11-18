#!/usr/bin/env bash
# Release a new version of mcp-lazy.
#
# Sets the version in Cargo.toml, commits, tags, and pushes. The tag starts
# .github/workflows/release.yml, which builds the binaries, creates the
# GitHub release with notes from the commits since the previous tag, and
# uploads the archives and SHA256SUMS.
#
# Usage:
#   scripts/release.sh patch        # 0.1.0 -> 0.1.1
#   scripts/release.sh minor        # 0.1.0 -> 0.2.0
#   scripts/release.sh major        # 0.1.0 -> 1.0.0
#   scripts/release.sh 1.2.3        # explicit version
#   scripts/release.sh current      # tag the version already in Cargo.toml

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

[[ $# -eq 1 ]] || { usage; exit 1; }
cd "$(git rev-parse --show-toplevel)"

CURRENT=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
[[ -n "$CURRENT" ]] || die "can't find the version in Cargo.toml"

bump() {
    local major minor patch
    IFS=. read -r major minor patch <<<"$CURRENT"
    case "$1" in
        major) echo "$((major+1)).0.0" ;;
        minor) echo "$major.$((minor+1)).0" ;;
        patch) echo "$major.$minor.$((patch+1))" ;;
    esac
}

case "$1" in
    -h|--help) usage; exit 0 ;;
    major|minor|patch) NEW=$(bump "$1") ;;
    current) NEW="$CURRENT" ;;
    *) [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid version: $1"; NEW="$1" ;;
esac
TAG="v$NEW"

[[ -z "$(git status --porcelain)" ]] || die "working tree not clean"
[[ "$(git rev-parse --abbrev-ref HEAD)" == "master" ]] || die "not on master"
! git rev-parse --verify "$TAG" >/dev/null 2>&1 || die "tag $TAG already exists"

if [[ "$NEW" != "$CURRENT" ]]; then
    echo "version $CURRENT -> $NEW"
    tmp=$(mktemp)
    awk -v new="$NEW" '!done && /^version = "/ { print "version = \"" new "\""; done=1; next } { print }' Cargo.toml > "$tmp"
    mv "$tmp" Cargo.toml
    cargo update --workspace --quiet
    cargo test --locked --quiet || die "tests failed after the version change"
    git add Cargo.toml Cargo.lock
    git commit -m "Release $TAG"
else
    cargo test --locked --quiet || die "tests failed"
fi

git tag -a "$TAG" -m "Release $TAG"
git push origin master
git push origin "$TAG"
echo "Pushed $TAG. The release workflow builds and publishes it:"
echo "  https://github.com/ericmason/mcp-lazy/actions/workflows/release.yml"
