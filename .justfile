set shell := ["bash", "-euc"]
set windows-shell := ["bash.exe", "-uc"]
set quiet := true

default:
    just --list

patch:
    cargo release patch --no-publish --execute

ci:
    cargo fmt --all -- --check
    cargo check --all-targets --locked
    cargo clippy --all-targets --locked -- -D warnings
    cargo nextest run --all-targets --locked --no-fail-fast
