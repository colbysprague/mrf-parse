.PHONY: test lint fmt bench build memtest

build:
	cargo build --workspace --release

test:
	cargo test --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

bench:
	cargo bench --workspace

# Synthetic multi-GB TiC parse; asserts peak RSS < 512MB.
# MRF_MEMTEST_GB (default 2) sets the size, MRF_MEMTEST_DIR the scratch dir.
memtest:
	cargo test --release -p mrf-tic --test memtest -- --ignored --nocapture --test-threads=1
