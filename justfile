# Local and CI share these gates. A successful core test is not display/VM evidence.
set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check: infra-check skills-check fmt-check typecheck clippy test

infra-check: metadata actionlint scripts-check

metadata:
    python3 scripts/check-infrastructure.py

skills-check:
    python3 scripts/check-skills.py

# Requires the pinned compiler; infra-check remains usable before it is installed.
cargo-metadata:
    cargo metadata --format-version 1 --no-deps > /dev/null

actionlint:
    actionlint -color

scripts-check:
    shellcheck scripts/*.sh

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lockfile-check:
    @test -f Cargo.lock || { echo 'Cargo.lock is missing: resolve the recorded dependency baseline, then run cargo generate-lockfile and review the result.' >&2; exit 1; }

typecheck: lockfile-check
    cargo check --workspace --all-targets --locked

clippy: lockfile-check
    cargo clippy --workspace --all-targets --locked -- -D warnings

test: lockfile-check
    cargo test --workspace --all-targets --locked

deny: lockfile-check
    cargo deny --locked check

check-core: lockfile-check
    cargo check -p koma-proto -p koma-model -p koma-seat -p koma-lua --all-targets --no-default-features --locked

test-core: lockfile-check
    cargo test -p koma-proto -p koma-model -p koma-seat -p koma-lua --all-targets --no-default-features --locked

# Generate to stdout; callers choose where to store the resulting definitions.
gen-types: lockfile-check
    cargo run --locked -p komactl -- gen-types

# Development-prerelease archive; publication happens only through the tag-triggered release workflow.
# Accepts `just package-release 0.0.1` or `just package-release version=0.0.1`.
[positional-arguments]
package-release version target="x86_64-unknown-linux-gnu" out="/tmp/koma/release":
    bash scripts/package-release.sh --version "${1#version=}" --target "${2#target=}" --out "${3#out=}"

hooks-install:
    lefthook install pre-commit
    carryctx hooks install --compose

