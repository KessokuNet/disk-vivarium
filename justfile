default:
    @just --list

build:
    cargo build
    sudo setcap cap_sys_admin=ep target/debug/disk-vivarium

run path="" *args:
    cargo build --release
    sudo setcap cap_sys_admin=ep target/release/disk-vivarium
    ./target/release/disk-vivarium {{path}} {{args}}

test:
    cargo test

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets -- -D warnings
