#!/bin/bash
set -e

echo "🔍 Checking release readiness..."

# Check for uncommitted changes
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "❌ Error: You have uncommitted changes"
    git status --short
    exit 1
fi

# Check Cargo.lock is in sync
echo "📦 Checking Cargo.lock is in sync..."
cargo check --locked 2>/dev/null || {
    echo "❌ Error: Cargo.lock is out of date. Run 'cargo check' and commit Cargo.lock"
    exit 1
}

# Get version from Cargo.toml
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
echo "📋 Version: v$VERSION"

# Check if CHANGELOG has an entry for this version
if [ -f CHANGELOG.md ]; then
    if grep -q "## \[$VERSION\]" CHANGELOG.md; then
        echo "✅ CHANGELOG.md has entry for v$VERSION"
    else
        echo "⚠️  Warning: No CHANGELOG.md entry found for v$VERSION"
    fi
fi

echo ""
echo "✅ Release verification passed!"
echo "   Run 'make version-patch' (or minor/major) to create the release"
