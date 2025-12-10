.PHONY: build release test lint fmt check clean run install version-get version-major version-minor version-patch version-push release-major release-minor release-patch build-wheel verify-release pre-commit-update

# Get version from Cargo.toml
VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

# Verify release is ready
verify-release:
	@echo "Verifying release readiness..."
	@./scripts/verify-release-ready.sh

# Build Python wheel locally
build-wheel:
	@echo "Building Python wheel..."
	maturin build --release

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

# Show current version
version-get:
	@echo "Current version: v$(VERSION)"

# Version tagging targets
version-major:
	@echo "Creating new major version tag..."
	$(eval CURRENT := $(shell git describe --tags --abbrev=0 2>/dev/null || echo v0.0.0))
	$(eval MAJOR := $(shell echo $(CURRENT) | sed -E 's/v([0-9]+)\.[0-9]+\.[0-9]+/\1/'))
	$(eval NEW_MAJOR := $(shell echo $$(( $(MAJOR) + 1 ))))
	$(eval NEW_TAG := v$(NEW_MAJOR).0.0)
	@echo "Current: $(CURRENT) -> New: $(NEW_TAG)"
	@sed -i '' 's/^version = ".*"/version = "$(NEW_MAJOR).0.0"/' Cargo.toml
	@cargo check --quiet
	@git add Cargo.toml Cargo.lock
	@git commit -m "chore: bump version to $(NEW_TAG)"
	@git tag -a $(NEW_TAG) -m "Release $(NEW_TAG)"
	@echo "Version $(NEW_TAG) created. Run 'make version-push' to trigger release."

version-minor:
	@echo "Creating new minor version tag..."
	$(eval CURRENT := $(shell git describe --tags --abbrev=0 2>/dev/null || echo v0.0.0))
	$(eval MAJOR := $(shell echo $(CURRENT) | sed -E 's/v([0-9]+)\.[0-9]+\.[0-9]+/\1/'))
	$(eval MINOR := $(shell echo $(CURRENT) | sed -E 's/v[0-9]+\.([0-9]+)\.[0-9]+/\1/'))
	$(eval NEW_MINOR := $(shell echo $$(( $(MINOR) + 1 ))))
	$(eval NEW_TAG := v$(MAJOR).$(NEW_MINOR).0)
	@echo "Current: $(CURRENT) -> New: $(NEW_TAG)"
	@sed -i '' 's/^version = ".*"/version = "$(MAJOR).$(NEW_MINOR).0"/' Cargo.toml
	@cargo check --quiet
	@git add Cargo.toml Cargo.lock
	@git commit -m "chore: bump version to $(NEW_TAG)"
	@git tag -a $(NEW_TAG) -m "Release $(NEW_TAG)"
	@echo "Version $(NEW_TAG) created. Run 'make version-push' to trigger release."

version-patch:
	@echo "Creating new patch version tag..."
	$(eval CURRENT := $(shell git describe --tags --abbrev=0 2>/dev/null || echo v0.0.0))
	$(eval MAJOR := $(shell echo $(CURRENT) | sed -E 's/v([0-9]+)\.[0-9]+\.[0-9]+/\1/'))
	$(eval MINOR := $(shell echo $(CURRENT) | sed -E 's/v[0-9]+\.([0-9]+)\.[0-9]+/\1/'))
	$(eval PATCH := $(shell echo $(CURRENT) | sed -E 's/v[0-9]+\.[0-9]+\.([0-9]+)/\1/'))
	$(eval NEW_PATCH := $(shell echo $$(( $(PATCH) + 1 ))))
	$(eval NEW_TAG := v$(MAJOR).$(MINOR).$(NEW_PATCH))
	@echo "Current: $(CURRENT) -> New: $(NEW_TAG)"
	@sed -i '' 's/^version = ".*"/version = "$(MAJOR).$(MINOR).$(NEW_PATCH)"/' Cargo.toml
	@cargo check --quiet
	@git add Cargo.toml Cargo.lock
	@git commit -m "chore: bump version to $(NEW_TAG)"
	@git tag -a $(NEW_TAG) -m "Release $(NEW_TAG)"
	@echo "Version $(NEW_TAG) created. Run 'make version-push' to trigger release."

version-push:
	$(eval LATEST_TAG := $(shell git describe --tags --abbrev=0))
	@echo "Pushing latest commit and tag $(LATEST_TAG) to origin..."
	@git push
	@git push origin $(LATEST_TAG)
	@echo "Release workflow triggered for $(LATEST_TAG)"

# Combined release targets
release-major: version-major version-push
release-minor: version-minor version-push
release-patch: version-patch version-push

# Update pre-commit hooks to latest versions
pre-commit-update:
	pre-commit autoupdate
