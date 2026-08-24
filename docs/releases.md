# Releases

Vership is the release control plane for `upd`. GitHub Actions remains the
execution plane for cross-platform builds, publication, provenance, and
release-derived integration pins.

## Create a release

Start from a clean, up-to-date `main` branch and run:

```bash
vership preflight
vership bump patch # or minor/major
tarry cmd --timeout 20m -- vership verify
```

Vership runs `make check`, synchronizes package versions, updates the changelog,
creates the Conventional Commit release commit and tag, and pushes both. Use
`vership release` only when the on-disk version was intentionally set in
advance. The tag starts the release workflow.

The workflow validates that the tag matches the package version, builds and
attests every platform archive, publishes crates.io, PyPI, and the GitHub
release, and verifies the release artifacts. It then generates
`release-pins.json` from the exact archive bytes and checksum sidecars. A
version-specific pull request updates the manifest, GitLab component, and
documentation together. GitHub workflows resolve their default binary from the
manifest, so the release bot never needs permission to rewrite executable
workflow files. The pull request is merged only after the generated tree passes
`make check` and workflow validation.

## Retry release-pin synchronization

Publishing and pin synchronization are separate recovery domains. If packages
or release assets were published successfully but the pin job failed, do not
create another release. Run **Synchronize release pins** manually with the
existing `vX.Y.Z` tag. The job is idempotent, refuses downgrades, verifies that
the tag is an ancestor of `main`, and safely updates its version-specific
automation branch with force-with-lease.

If the release failed before any release, artifact, package, checksum, or
attestation became public, follow the repository's failed-release policy and
retry the same version. Once anything was published, preserve the tag and use a
patch release for release-content corrections.

## Dry runs

The Release workflow accepts an optional `version` input. A dry run defaults to
the package version on the selected branch and builds correctly named artifacts
without publishing. A non-dry manual run requires an existing explicit tag so
an accidental branch publication cannot occur.
