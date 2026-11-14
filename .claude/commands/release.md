Release proxmox-mcp by bumping the minor version, verifying the build, and tagging.

This is the same step list as gitlab-mcp and netbox-mcp; only repo-specific
details differ. Release from `main`: the release workflow refuses a tag that
isn't on it.

## Steps

### 1. Determine the new version

Read `Cargo.toml` and extract the current `version` field. Compute the next minor
version by incrementing the middle component and resetting the patch to 0
(e.g. `0.1.0` → `0.2.0`). If the user supplied a version as an argument to this
command, use that instead.

### 2. Update Cargo.toml

Change the `version` field in `Cargo.toml` to the new version string. (This repo
has no plugin.json to keep in step.)

### 3. Update Cargo.lock

Run `cargo build` (without `--locked`) so Cargo rewrites `Cargo.lock` to match
the new version. This is the only step that runs without `--locked`.

```
cargo build
```

### 4. Audit dependencies

```
make deny
cargo outdated
```

**Security (`make deny`):** If cargo-deny reports a vulnerability advisory, a
banned crate (aws-lc-sys/aws-lc-rs), or a license that isn't allowed, stop
immediately and report it to the user. Do not proceed until it is resolved. Never
add a license to `deny.toml` without checking with the user that it is
GPL-3.0-compatible.

**Outdated dependencies (`cargo outdated`):** Report the results to the user. Focus on
direct dependencies listed in `Cargo.toml` (the top-level rows). If any direct
dependencies are behind, ask the user whether to update them before continuing;
transitive-only updates are informational and do not require confirmation.

If `cargo deny` or `cargo outdated` are not installed, install them first:

```
cargo install --locked cargo-deny cargo-outdated
```

### 5. Check the pinned toolchain

The toolchain is pinned in `rust-toolchain.toml`. Compare it with the latest
stable (`rustup check`, or `rustc +stable --version` after
`rustup update stable`). If a newer stable exists, tell the user and offer to bump
the `channel` first. If they agree, update the pin, fix any new clippy findings,
and commit that separately before continuing.

### 6. Run the full quality gate

```
make check
```

This runs fmt-check, lint (secrets scan, clippy, fmt), test, deny, the release
build, and `make smoke` against the release binary. If anything fails, stop and
report the error; do not proceed to tagging. If only formatting fails, run
`make fmt` and re-run `make check`.

### 7. Live suite

proxmox-mcp has no live suite yet. When one exists, run it here with
`LIVE_REQUIRED=1` and confirm from the output that the tests ran (report the
count) rather than skipped.

### 8. Update CHANGELOG.md

Read `CHANGELOG.md`. This project keeps an `[Unreleased]` section near the top.

Derive the release content from two sources and merge them:
- The existing `[Unreleased]` section, if it has any entries.
- The git log since the previous release tag:

  ```
  git log v<previous version>..HEAD --oneline
  ```

Rename the `[Unreleased]` heading to `## [<new version>] - <today's date>`
(an ASCII hyphen, today's actual date, `YYYY-MM-DD`). Fold in anything from the
git log that isn't already listed, then add a fresh, empty `## [Unreleased]`
section above it.

Group entries into the standard changelog categories (Added, Changed, Fixed,
Removed, Security, Documentation, Internal), and omit any category with no
entries. Exclude housekeeping commits such as version bumps and `cargo fmt` runs.
Write the entries in the same style as the existing changelog entries. The
release workflow publishes this section as the release notes, so confirm it
extracts:

```
scripts/changelog-section.sh <new version>
```

### 9. Commit the version bump and changelog

```
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "Bump version to <new version>"
```

### 10. Tag the release

```
git tag -a v<new version> -m v<new version>
```

### 11. Confirm before pushing

Show the user the new version, the commit SHA, and the tag, then ask whether to
push. Pushing the tag triggers `.github/workflows/release.yml`. It verifies the
tag, runs the full CI, builds and smoke-tests linux amd64/arm64 and darwin
arm64/amd64 binaries, and publishes the release with checksums and a provenance
attestation. If they confirm, run:

```
git push origin main v<new version>
```

### 12. Verify the release

1. Watch the Release run until it finishes, and report each job's result:

   ```
   gh run watch <run-id> --exit-status
   ```

2. Confirm the release has the 4 binaries plus `checksums.txt`:

   ```
   gh release view v<new version> --json assets -q '.assets[].name'
   ```

3. Download this host's asset (e.g. `proxmox-mcp-darwin-arm64`) and
   `checksums.txt` into the scratchpad. Verify the sha256 (`shasum -a 256 -c`
   with `--ignore-missing`) and the provenance:

   ```
   gh attestation verify <asset> --repo 3kirt/proxmox-mcp
   ```

4. `chmod +x` it and run `make smoke BIN=<downloaded asset>`.

5. Make one real read-only tool call against the user's configured cluster.
   Either call `proxmox_version_get` over stdio with the downloaded binary,
   using the user's default config directory, or ask the user to make it through
   their MCP client. If the cluster token is pass-backed and locked, ask the user
   to run `proxmox-mcp unlock` first.
