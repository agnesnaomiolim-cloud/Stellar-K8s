.PHONY: build test fmt clippy clean

# The factory tests deploy the compiled basket WASM, so build first.
build:
	cargo build --workspace --target wasm32v1-none --release

test: build
	cargo test --workspace

fmt:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean
