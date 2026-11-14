default:
    @just --list

build:
    cargo build --features gui

run path="" *args:
    cargo run --release --features gui -- {{path}} {{args}}

test:
    cargo test

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets --features gui -- -D warnings
