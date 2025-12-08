#!/bin/bash
# Verify that the release is ready to be published

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

ERRORS=0

echo "Verifying release readiness..."
echo ""

# Check for uncommitted changes
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo -e "${RED}ERROR: Uncommitted changes detected${NC}"
    git status --short
    ERRORS=$((ERRORS + 1))
else
    echo -e "${GREEN}OK: No uncommitted changes${NC}"
fi

# Check Cargo.lock is up to date
if ! cargo check --locked 2>/dev/null; then
    echo -e "${RED}ERROR: Cargo.lock is out of date. Run 'cargo check' and commit Cargo.lock${NC}"
    ERRORS=$((ERRORS + 1))
else
    echo -e "${GREEN}OK: Cargo.lock is up to date${NC}"
fi

# Get version from Cargo.toml
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
echo -e "${GREEN}Version: ${VERSION}${NC}"

# Check CHANGELOG has entry for this version
if grep -q "## \[${VERSION}\]" CHANGELOG.md; then
    echo -e "${GREEN}OK: CHANGELOG.md has entry for v${VERSION}${NC}"
else
    echo -e "${YELLOW}WARNING: CHANGELOG.md may not have entry for v${VERSION}${NC}"
fi

# Check README exists
if [ -f README.md ]; then
    echo -e "${GREEN}OK: README.md exists${NC}"
else
    echo -e "${RED}ERROR: README.md not found${NC}"
    ERRORS=$((ERRORS + 1))
fi

# Run tests
echo ""
echo "Running tests..."
if cargo test --quiet 2>/dev/null; then
    echo -e "${GREEN}OK: All tests pass${NC}"
else
    echo -e "${RED}ERROR: Tests failed${NC}"
    ERRORS=$((ERRORS + 1))
fi

# Run clippy
echo ""
echo "Running clippy..."
if cargo clippy --quiet -- -D warnings 2>/dev/null; then
    echo -e "${GREEN}OK: No clippy warnings${NC}"
else
    echo -e "${RED}ERROR: Clippy warnings detected${NC}"
    ERRORS=$((ERRORS + 1))
fi

# Summary
echo ""
if [ $ERRORS -eq 0 ]; then
    echo -e "${GREEN}Release is ready!${NC}"
    echo ""
    echo "To release:"
    echo "  1. git tag -a v${VERSION} -m \"Release v${VERSION}\""
    echo "  2. git push origin main v${VERSION}"
    exit 0
else
    echo -e "${RED}${ERRORS} error(s) found. Please fix before releasing.${NC}"
    exit 1
fi
