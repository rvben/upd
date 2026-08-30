.PHONY: build release test test-verbose lint fmt fmt-check release-pins-check check ci clean run run-release install release-patch release-minor release-major

# Build debug binary
build:
	cargo build

# Build release binary
release:
	cargo build --release

# Run all tests
# Several registry tests intentionally exercise process-global credential and
# index environment variables, so they must not race each other.
test:
	cargo test -- --test-threads=1

# Run tests with output
test-verbose:
	cargo test -- --nocapture --test-threads=1

# Run clippy lints
lint:
	cargo clippy -- -D warnings

# Format code
fmt:
	cargo fmt

# Check formatting without changing files
fmt-check:
	cargo fmt -- --check

# Verify that every distributed integration uses the canonical release pins.
release-pins-check:
	python3 scripts/sync-release-pins.py --check
	python3 -m unittest discover -s scripts/tests -p 'test_*.py'

# Everything CI's test job runs, in the order it runs them. CI invokes this
# target rather than repeating the list, so the two cannot drift apart.
ci: release-pins-check fmt-check lint test

# Alias for `ci`
check: ci

# Clean build artifacts
clean:
	cargo clean

# Run debug build
run:
	cargo run

# Run with arguments
run-release:
	./target/release/upd

# Install to ~/.cargo/bin
install:
	cargo install --path .

release-patch:
	vership bump patch

release-minor:
	vership bump minor

release-major:
	vership bump major
