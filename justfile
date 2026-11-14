# Local and CI share these gates. A successful core test is not display/VM evidence.
set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check: infra-check fmt-check typecheck clippy test

infra-check: metadata actionlint scripts-check

metadata:
    python3 scripts/check-infrastructure.py

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

check-core: lockfile-check
    cargo check -p koma-proto -p koma-model -p koma-seat -p koma-lua --all-targets --no-default-features --locked

test-core: lockfile-check
    cargo test -p koma-proto -p koma-model -p koma-seat -p koma-lua --all-targets --no-default-features --locked

# Generate to stdout; callers choose where to store the resulting definitions.
gen-types: lockfile-check
    cargo run --locked -p komactl -- gen-types

hooks-install:
    lefthook install pre-commit
    carryctx hooks install --compose

