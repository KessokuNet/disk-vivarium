default:
    @just --list

build:
    cargo build --features gui
    sudo setcap cap_sys_admin=ep target/debug/disk-vivarium

run path="" *args:
    cargo build --release --features gui
    sudo setcap cap_sys_admin=ep target/release/disk-vivarium
    ./target/release/disk-vivarium {{path}} {{args}}

test:
    cargo test

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets --features gui -- -D warnings
