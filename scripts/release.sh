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

# v7.37.8 followup (mailrs lock-hang 4th-recurrence ack P2 — prod-shape
# catalog in CI gate). Run the dogfood-replay battery before any
# crates.io / docker / acceptance work. Fast tier (~5 min) covers
# every synthetic recovery + replay scenario and the in-tree fixtures;
# the prod snapshot (mailrs.spg, 800 MB+) is skipped here but available
# via `gate.sh dogfood --full` for nightly runs on a machine that
# carries it. Setting SKIP_DOGFOOD=1 selects the legacy
# "no preflight gate" behaviour for the rare case where the operator
# already ran it externally.
if [[ "${SKIP_DOGFOOD:-0}" == 0 ]]; then
    banner "preflight gate: dogfood-replay (fast tier)"
    if ! scripts/gate.sh dogfood; then
        echo "preflight: dogfood-replay gate FAILED — release blocked. \
Re-run with \`SKIP_DOGFOOD=1 scripts/release.sh $VERSION\` to override, \
but only after triaging the failure (mailrs P0 4th-recurrence root \
cause was exactly this kind of skipped gate)." >&2
        exit 1
    fi
    echo "preflight gate: dogfood-replay PASS"
else
    echo "preflight gate: dogfood-replay SKIPPED (SKIP_DOGFOOD=1 set)"
fi

# v7.37.27 (27.7 + 27.12) — full G1-G5 release battery before
# any artefact lands on crates.io or docker.io. The dogfood
# preflight above catches prod-shape regressions; gate.sh all
# closes the workspace / unit / e2e / gates / biz lattice that
# G1-G5 cover (TESTING.md five categories). Setting SKIP_FULL=1
# selects the v7.37.7-and-earlier behaviour for the rare case
# where the operator already ran gate.sh all externally.
if [[ "${SKIP_FULL:-0}" == 0 ]]; then
    banner "preflight gate: gate.sh all (G1-G5)"
    if ! scripts/gate.sh all; then
        echo "preflight: gate.sh all FAILED — release blocked. \
Re-run with \`SKIP_FULL=1 scripts/release.sh $VERSION\` only after \
triaging the failure. Each category (lint / unit / e2e / gates / \
biz / dogfood) maps to one of the G1-G5 release gates in \
WIRE_FORMAT_PROMISE.md; a green release.sh means the wire-format \
promise is intact." >&2
        exit 1
    fi
    echo "preflight gate: gate.sh all PASS"
else
    echo "preflight gate: gate.sh all SKIPPED (SKIP_FULL=1 set)"
fi

if [[ "$SKIP_CRATES" == 0 ]]; then
    banner "crates.io publish × ${#CRATES[@]}"
    # v7.37.7 — crates.io's data-access policy now rejects bare-curl GETs
    # with HTTP 403 ("usually means you are in violation of our API data
    # access policy"). Send a real UA so the skip-check works again; the
    # endpoint returns HTTP 200 when the version exists.
    UA="release.sh/$VERSION (goliajp/spg; help@golia.jp)"
    for crate in "${CRATES[@]}"; do
        if curl -fsS -A "$UA" "https://crates.io/api/v1/crates/${crate}/${VERSION}" \
            >/dev/null 2>&1; then
            echo "  ${crate} ${VERSION} already on crates.io — skip"
        else
            echo "  publishing ${crate} ${VERSION}"
            # If a prior retry left this version in the index but the UA
            # check above somehow still missed it, the cargo publish call
            # below will fail with "already exists on crates.io index";
            # treat that case as success so the loop continues.
            if ! cargo publish -p "$crate" --locked 2> /tmp/release-publish-err.txt; then
                if grep -q "already exists on crates.io index" /tmp/release-publish-err.txt; then
                    echo "  ${crate} ${VERSION} already on crates.io — skip (post-publish)"
                else
                    cat /tmp/release-publish-err.txt >&2
                    exit 1
                fi
            fi
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
