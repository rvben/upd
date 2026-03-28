.PHONY: build release test lint fmt check clean run install

# Build debug binary
build:
	cargo build

# Build release binary
release:
	cargo build --release

# Run all tests
test:
	cargo test

# Run tests with output
test-verbose:
	cargo test -- --nocapture

# Run clippy lints
lint:
	cargo clippy -- -D warnings

# Format code
fmt:
	cargo fmt

# Check formatting without changing files
fmt-check:
	cargo fmt -- --check

# Run all checks (format, lint, test)
check: fmt-check lint test

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
