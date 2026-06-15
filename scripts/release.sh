#!/usr/bin/env bash
# release.sh — post-finish publish train.
#
# Run AFTER `git flow release|hotfix finish` has merged to master and
# the vX.Y.Z tag exists, and after `git push origin master develop
# --follow-tags`. Every step is idempotent, so a half-failed train can
# simply be re-run:
#
#   1. preflight   on master, clean tree, synced with origin,
#                  tag vX.Y.Z == HEAD, workspace version == X.Y.Z
#   2. crates      cargo publish × 11 in topological order;
#                  versions already on crates.io are skipped
#   3. docker      buildx multi-arch (amd64+arm64), tags X.Y.Z / X.Y /
#                  latest, --push; manifest digest captured to stdout
#                  and target/release-digest-vX.Y.Z.txt
#   4. acceptance  drop-in panel against the freshly pushed image →
#                  scripts/dropin-acceptance-report-vX.Y.Z.md
#   5. checklist   prints what remains human: check in the report on
#                  develop, mailrs ack (include the digest)
#
# Usage: scripts/release.sh <X.Y.Z> [--skip-crates] [--skip-docker]
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-}"
[[ -n "$VERSION" ]] || { echo "usage: $0 <X.Y.Z> [--skip-crates] [--skip-docker]" >&2; exit 2; }
shift

SKIP_CRATES=0
SKIP_DOCKER=0
for arg in "$@"; do
    case "$arg" in
        --skip-crates) SKIP_CRATES=1 ;;
        --skip-docker) SKIP_DOCKER=1 ;;
        *) echo "release.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# crates.io topological order — leaves first; crates.io rejects
# dependency-before-dependent publishes.
CRATES=(
    spg-wire spg-crypto spg-sql spg-storage spg-audit spg-manifest
    spg-engine spg-embedded spg-embedded-tokio spg-sqlx spg-server spgctl
)

banner() { printf '\n══ release.sh %s ══\n' "$*"; }

banner "preflight v${VERSION}"
branch=$(git rev-parse --abbrev-ref HEAD)
[[ "$branch" == "master" ]] || { echo "preflight: on '$branch', need master" >&2; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "preflight: working tree not clean" >&2; exit 1; }
git fetch origin
[[ "$(git rev-parse master)" == "$(git rev-parse origin/master)" ]] \
    || { echo "preflight: master not in sync with origin/master — push first" >&2; exit 1; }
tag_commit=$(git rev-parse "v${VERSION}^{commit}" 2>/dev/null) \
    || { echo "preflight: tag v${VERSION} does not exist" >&2; exit 1; }
[[ "$tag_commit" == "$(git rev-parse HEAD)" ]] \
    || { echo "preflight: tag v${VERSION} is not HEAD" >&2; exit 1; }
manifest_version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
[[ "$manifest_version" == "$VERSION" ]] \
    || { echo "preflight: Cargo.toml workspace version is ${manifest_version}, expected ${VERSION}" >&2; exit 1; }
echo "preflight OK: master @ $(git rev-parse --short HEAD), tag v${VERSION}"

if [[ "$SKIP_CRATES" == 0 ]]; then
    banner "crates.io publish × ${#CRATES[@]}"
    for crate in "${CRATES[@]}"; do
        if curl -fsS "https://crates.io/api/v1/crates/${crate}/${VERSION}" >/dev/null 2>&1; then
            echo "  ${crate} ${VERSION} already on crates.io — skip"
        else
            echo "  publishing ${crate} ${VERSION}"
            cargo publish -p "$crate" --locked
        fi
    done
else
    banner "crates.io publish SKIPPED (--skip-crates)"
fi

IMAGE_REPO="goliakk/spg"
MINOR="${VERSION%.*}"
if [[ "$SKIP_DOCKER" == 0 ]]; then
    banner "docker buildx ${IMAGE_REPO}:{${VERSION},${MINOR},latest}"
    metadata="target/release-buildx-metadata-v${VERSION}.json"
    docker buildx build --platform linux/amd64,linux/arm64 \
        -t "${IMAGE_REPO}:${VERSION}" \
        -t "${IMAGE_REPO}:${MINOR}" \
        -t "${IMAGE_REPO}:latest" \
        --metadata-file "$metadata" \
        --push .
    digest=$(grep -o 'sha256:[a-f0-9]*' "$metadata" | head -1)
    echo "$digest" > "target/release-digest-v${VERSION}.txt"
    echo "manifest digest: ${digest}"
else
    banner "docker push SKIPPED (--skip-docker)"
fi

banner "drop-in acceptance vs ${IMAGE_REPO}:${VERSION}"
scripts/dropin-acceptance.sh \
    --image "${IMAGE_REPO}:${VERSION}" \
    --port 25433 \
    --fixture scripts/fixtures/mailrs-pg-extensions.sql \
    --fixture scripts/fixtures/mailrs-init-schema-v1.7.142.sql \
    --report "scripts/dropin-acceptance-report-v${VERSION}.md"

banner "v${VERSION} published — remaining human steps"
cat <<EOF
  [ ] commit scripts/dropin-acceptance-report-v${VERSION}.md on develop
      (chore(release): v${VERSION} post-release — check in dropin report)
  [ ] mailrs ack note — include the manifest digest:
      $(cat "target/release-digest-v${VERSION}.txt" 2>/dev/null || echo '(docker step skipped)')
  [ ] release battery was green before finish: gate.sh all + mailrs
      zero-change validation (see docs/TESTING.md "Release battery")
EOF
