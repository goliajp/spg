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
#                  tmp/reports/dropin-acceptance-report-vX.Y.Z.md
#                  (untracked: internal working material)
#   5. checklist   prints what remains human: check in the report on
#                  develop, mailrs ack (include the digest)
#
# Usage: scripts/release.sh <X.Y.Z> [--skip-crates] [--skip-docker]
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-}"
[[ -n "$VERSION" ]] || { echo "usage: $0 <X.Y.Z> [--fast] [--skip-crates] [--skip-docker]" >&2; exit 2; }
shift

SKIP_CRATES=0
SKIP_DOCKER=0
# --fast: the end-of-the-line path. Ships the same artefacts through the
# same preflight, and replaces the ~40-minute battery (dogfood + gate.sh
# all + the 59-cell drop-in panel) with the precommit tier, whose own
# hard cap is 150 s. That tier is not nothing — fmt, clippy, the unit
# tests of every affected crate, THIS version's pins, an slt smoke subset
# and the ironrule wire/WAL smoke — but it is a smoke test, and the
# things it does not run are the ones that have historically caught
# release-blocking regressions: the prod-shape dogfood replay, the full
# corpus, the perf gate, and the drop-in acceptance panel.
#
# It exists for two jobs the owner named: shipping a fix for a live
# production defect, and getting something brand new in front of real
# use quickly. Both are cases where the cost of waiting exceeds the cost
# of a narrower gate — which is a judgement about THIS release, not a
# new default.
#
# The obligation it creates: run the full battery afterwards. If it goes
# red, the answer is the next version, never a retag — a published tag
# and a published crate are both unrecallable.
FAST=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        --skip-crates) SKIP_CRATES=1 ;;
        --skip-docker) SKIP_DOCKER=1 ;;
        *) echo "release.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# crates.io topological order — leaves first; crates.io rejects
# dependency-before-dependent publishes.
# v7.37.14 — spg-tzif was missing, and the train only found out at
# spg-engine: `no matching package named spg-tzif found`, with six crates
# already up and unrecallable. It has no dependencies of its own, so it
# goes first.
CRATES=(
    spg-tzif spg-wire spg-crypto spg-sql spg-storage spg-audit spg-manifest
    spg-engine spg-embedded spg-embedded-tokio spg-sqlx spg-server spgctl
)

# v7.39.12 — every banner carries the seconds since the last one, so
# the next release says where its time went instead of being guessed at.
_last_banner_at=$(date +%s)
banner() {
    local now
    now=$(date +%s)
    printf '\n══ release.sh %s ══  [+%ss]\n' "$*" "$(( now - _last_banner_at ))"
    _last_banner_at=$now
}

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
# v7.40.0 — and no BARE tag beside it.
#
# `git flow release finish` names the tag after the branch, so
# `release/X.Y.Z` produces `X.Y.Z` and this repository's convention is
# `vX.Y.Z`. Twenty-one bare tags have accumulated since 7.22.0, several
# of them pushed, and every release has depended on someone noticing
# between `finish` and here. The check above only proves the `v` tag
# exists — it says nothing about a bare twin, which is a valid tag that
# nothing will complain about again.
#
# `scripts/release-finish.sh` is the way to not create one.
if git rev-parse -q --verify "refs/tags/${VERSION}" >/dev/null; then
    echo "preflight: a bare tag '${VERSION}' exists beside v${VERSION} — \
git-flow named it after the branch. Delete it (git tag -d ${VERSION}), and \
use scripts/release-finish.sh next time." >&2
    exit 1
fi
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
if [[ "$FAST" == 1 ]]; then
    # v7.40.2 — through `precommit-tier.sh`, so the gate runs where the
    # machine is idle and proves it graded THIS tree.
    #
    # The v7.40.1 train was blocked by its own gate on a box carrying
    # somebody else's job: eight CPU-saturating workers, and `slt-smoke`
    # took 129.3 s against a 15 s budget where its own history reads
    # 1.6-1.9 s. The tier was right to refuse — a contaminated machine
    # cannot tell a slow step from a busy box — but the release does not
    # become unshippable because this laptop is busy.
    #
    # The host factor that is supposed to absorb this is measured once,
    # on `fmt`, which runs FIRST. That run read 1.05x at load 17 and
    # then met load 27 four steps later. A ruler read at t=0 does not
    # describe a machine whose load doubled during the run.
    #
    # `precommit-tier.sh` falls back to running here when there is no
    # testbed or the trees differ, so this is the same gate either way.
    banner "preflight gate: precommit tier (--fast)"
    if ! scripts/precommit-tier.sh; then
        echo "preflight: precommit tier FAILED — release blocked. This is \
the narrow gate already; there is nothing left to fall back to, so the \
failure is the answer." >&2
        exit 1
    fi
    cat >&2 <<FASTNOTE
preflight gate: precommit tier PASS (--fast)

  NOT RUN for v${VERSION}: the dogfood prod-shape replay, gate.sh all
  (lint / unit / e2e / gates / biz), the perf gate, and the 59-cell
  drop-in acceptance panel. This build has had a smoke test, not a
  release battery.

  Run \`scripts/suite.sh prerelease\` against tag v${VERSION} as soon as
  the reason for the hurry is over. If it goes red, cut the next
  version — the tag and the crates are already unrecallable.
FASTNOTE
elif PRERELEASE_REPORT=$(scripts/prerelease-verdict.sh 2>/dev/null); then
    # v7.39.11 — a green `suite.sh prerelease` on THIS commit is the
    # battery, and running it again here learns nothing.
    #
    # The release used to run the categories three times on one tree:
    # `suite.sh prerelease` (the operator's step, ~74 minutes of
    # budget), then `gate.sh dogfood`, then `gate.sh all` — which is
    # lint / unit / e2e / gates / biz / dogfood / perf, the same list
    # again. The tree does not change between them.
    #
    # The report is only accepted when its runid names HEAD's short
    # SHA, so evidence from the commit before this one does not let a
    # release through, and a report with any step `fail` or `skipped`
    # is refused.
    echo "preflight gate: prerelease PASS on HEAD — ${PRERELEASE_REPORT}"
    echo "preflight gate: dogfood-replay + gate.sh all SKIPPED (that report covers them)"
    SKIP_FULL=1
elif [[ "${SKIP_DOGFOOD:-0}" == 0 ]]; then
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
# v7.37 (round 895) — performance blocks the release, by the owner's
# decision of 2026-08-09. `PERF_REQUIRED=1` turns gate.sh's perf category
# from a loud skip into a hard failure, so a release cannot be cut
# without SPGS having been compared against a live PG18. Unsetting it is
# not an option here; the escape hatch is SKIP_PERF=1, which announces
# itself in the log so an unchecked build is visible afterwards rather
# than indistinguishable from a checked one.
export PERF_REQUIRED=1
if [[ "$FAST" == 1 ]]; then
    echo "preflight gate: gate.sh all SKIPPED (--fast; see the note above)"
elif [[ "${SKIP_FULL:-0}" == 0 ]]; then
    banner "preflight gate: gate.sh all (G1-G5 + perf)"
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
    if [[ -n "${PRERELEASE_REPORT:-}" ]]; then
        echo "preflight gate: gate.sh all SKIPPED (covered by ${PRERELEASE_REPORT})"
    else
        echo "preflight gate: gate.sh all SKIPPED (SKIP_FULL=1 set)"
    fi
fi

# v7.40.11 — the acceptance panel runs BEFORE anything unrecallable.
#
# It used to run last, against the image this script had just pushed,
# after the crates were already on crates.io. So it could report a tail
# and never prevent one — and `--fast` skipped it outright.
#
# 7.40.10 is why. It shipped a regression: the uniqueness fix in that
# release taught one site a rule and left its ON CONFLICT arbiter
# behind, and `round12.upsert_via_unique_index` went 68 of 69 against
# the PUBLISHED image, minutes after the crates became permanent. The
# panel found it; it just found it too late to matter.
#
# The candidate is built from the binaries the preflight gate already
# produced, native architecture only — the panel exercises the code, not
# the architecture, and the multi-arch build below still publishes both.
# Same shape CI has used since v7.29 (`Dockerfile.ci`), which is where
# this should have been copied from the first time.
#
# `--fast` does not skip this. `--fast` trades the heavy re-run of the
# battery for the narrow gate; it was never meant to trade away the
# check that says the artifact works.
if [[ "$SKIP_DOCKER" == 0 ]]; then
    banner "candidate image + drop-in acceptance (BEFORE publishing)"
    # Built from SOURCE through buildx, for the host's own platform,
    # and loaded locally rather than pushed.
    #
    # NOT `Dockerfile.ci`: that one packages binaries the runner already
    # built, which is right on CI's Linux runner and wrong here — a
    # `cargo build --release` on this macOS host produces a Mach-O
    # binary, and a distroless Linux image carrying it answers nothing.
    # Measured: 0 of 59, with the wire never coming up.
    #
    # The layers this produces are the ones the multi-arch push below
    # reuses for this architecture, so the build is paid once.
    docker buildx build --load -t "spg-candidate:v${VERSION}" .
    scripts/dropin-acceptance.sh \
        --image "spg-candidate:v${VERSION}" \
        --no-pull \
        --port 25433 \
        --report "tmp/reports/dropin-acceptance-candidate-v${VERSION}.md"
    echo "candidate accepted — publishing is now allowed"
else
    banner "candidate acceptance SKIPPED (--skip-docker)"
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
            _pub_t0=$(date +%s)
            echo "  publishing ${crate} ${VERSION}"
            # If a prior retry left this version in the index but the UA
            # check above somehow still missed it, the cargo publish call
            # below will fail with "already exists on crates.io index";
            # treat that case as success so the loop continues.
            # v7.39.12 — one target directory for all thirteen
            # verifications.
            #
            # `cargo publish` packages the crate, extracts the tarball
            # under `target/package/<crate>-<ver>/`, and BUILDS it there
            # to prove the published artefact compiles. That check is
            # worth keeping — it is the one thing that catches a file
            # the manifest forgot to include — but each extracted crate
            # is its own workspace with its own target dir, so the
            # dependency graph was compiled from scratch thirteen times
            # in a row, having just been compiled by the gate.
            #
            # A shared `CARGO_TARGET_DIR` leaves the verification doing
            # exactly what it did and lets crate two onwards reuse what
            # crate one built. Kept out of `target/` proper so a release
            # never races the gate's own artefacts.
            if ! CARGO_TARGET_DIR="${PWD}/target/publish-verify" \
                cargo publish -p "$crate" --locked 2> /tmp/release-publish-err.txt; then
                if grep -q "already exists on crates.io index" /tmp/release-publish-err.txt; then
                    echo "  ${crate} ${VERSION} already on crates.io — skip (post-publish)"
                else
                    cat /tmp/release-publish-err.txt >&2
                    exit 1
                fi
            fi
            echo "  ${crate} took $(( $(date +%s) - _pub_t0 ))s"
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

# v7.40.11 — the SECOND run, against what was actually pushed. The
# candidate above is the gate; this one confirms the published artifact
# matches it, and runs on `--fast` too, because the thing it checks is
# the thing a customer pulls.
if [[ "$SKIP_DOCKER" == 1 ]]; then
    banner "published-image acceptance SKIPPED (--skip-docker)"
else
    banner "drop-in acceptance vs ${IMAGE_REPO}:${VERSION}"
    scripts/dropin-acceptance.sh \
        --image "${IMAGE_REPO}:${VERSION}" \
        --port 25433 \
        --fixture scripts/fixtures/mailrs-pg-extensions.sql \
        --fixture scripts/fixtures/mailrs-init-schema-v1.7.142.sql \
        --report "tmp/reports/dropin-acceptance-report-v${VERSION}.md"
    # v7.39.11 — the reports are internal working material and are not
    # tracked. The public repository carries what a USER of SPG needs;
    # a per-release acceptance run is evidence for us and for a customer
    # we hand it to, not documentation of the product. `tmp/` is
    # gitignored, so the record accumulates without appearing in the
    # tree.
    #
    # The root copy this used to write is gone with the same reasoning:
    # a tracked file whose freshness depends on someone remembering is a
    # file that goes stale, which is what it had done (it said
    # `goliakk/spg:7.37.15` and `panel cases: 57` while the panel had
    # been 66 for several releases).
fi

banner "v${VERSION} published — remaining human steps"
if [[ "$FAST" == 1 ]]; then
cat <<EOF
  This was a --fast release: the narrow preflight gate instead of the
  full battery. The drop-in acceptance panel ran TWICE and is not on
  this list — once against the candidate before anything was published,
  which is what allowed the publish, and once against the pushed image.

  v7.40.11 — this list used to carry "run the acceptance panel as soon
  as the reason for the hurry is over", and that sentence is how 7.40.10
  shipped a regression: the panel found it, minutes after the crates
  became permanent. A check that a release depends on does not belong on
  a list of things to do afterwards.

  [ ] run \`scripts/suite.sh prerelease\` against tag v${VERSION} if it
      was not already green on these exact bytes before the tag
  [ ] mailrs ack note — include the manifest digest:
      $(cat "target/release-digest-v${VERSION}.txt" 2>/dev/null || echo '(docker step skipped)')
  [ ] say in the release note that this build shipped on the fast path,
      so nobody reads its version number as carrying the usual evidence
EOF
else
cat <<EOF
  [ ] nothing to commit for the dropin report — it is written to
      tmp/reports/, which is not tracked
      The versioned reports stop at v7.38.8: this step was skipped for
      several releases running, which is why it now names both files.
  [ ] mailrs ack note — include the manifest digest:
      $(cat "target/release-digest-v${VERSION}.txt" 2>/dev/null || echo '(docker step skipped)')
  [ ] release battery was green before finish: gate.sh all + mailrs
      zero-change validation (see docs/TESTING.md "Release battery")
EOF
fi

# v7.40.3 — and put the operator back on develop.
#
# The train is the last thing done on master, and nothing brought you
# back afterwards. Three commits after v7.40.2 landed on master that
# way: ordinary development work, invisible on develop, while
# `git push origin develop` answered "Everything up-to-date" three
# times because the commits sat on the branch nobody was pushing. It
# was found by a mismatch — `origin/develop..develop` empty while the
# two refs named different commits — and not by anything that checks.
#
# The preflight above REFUSES to run anywhere but master, so this
# cannot strand a later run; it only ends the one that just finished.
git checkout -q develop 2>/dev/null \
    && echo "release.sh: back on develop" \
    || echo "release.sh: still on $(git rev-parse --abbrev-ref HEAD) — switch before committing" >&2
