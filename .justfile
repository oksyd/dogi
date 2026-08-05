set shell := ["bash", "-euc"]
set windows-shell := ["bash.exe", "-uc"]
set quiet := true

default:
    just --list

patch:
    cargo release patch --no-publish --execute

ci:
    cargo fmt --all -- --check
    cargo check --workspace --all-targets --all-features
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo nextest run --workspace --all-targets --all-features --no-fail-fast
