Release proxmox-mcp by bumping the minor version, verifying the build, and tagging.

## Steps

### 1. Determine the new version

Read `Cargo.toml` and extract the current `version` field. Compute the next minor
version by incrementing the middle component and resetting the patch to 0
(e.g. `0.1.0` → `0.2.0`). If the user supplied a version as an argument to this
command, use that instead.

### 2. Update Cargo.toml

Change the `version` field in `Cargo.toml` to the new version string.

### 3. Update Cargo.lock

Run `cargo build` (without `--locked`) so Cargo rewrites `Cargo.lock` to match
the new version. This is the only step that runs without `--locked`.

```
cargo build
```

### 4. Audit dependencies

Run these two checks:

```
cargo outdated
cargo audit
```

**Security (`cargo audit`):** If any vulnerabilities are reported, stop immediately and
report them to the user. Do not proceed to the quality gate or tagging until the
vulnerabilities are resolved.

**Outdated dependencies (`cargo outdated`):** Report the results to the user. Focus on
direct dependencies listed in `Cargo.toml` (the top-level rows). If any direct
dependencies are behind, ask the user whether to update them before continuing;
transitive-only updates are informational and do not require confirmation. Either
way, proceed to the quality gate once the user has been informed.

If `cargo outdated` or `cargo audit` are not installed, install them first:

```
cargo install cargo-outdated
cargo install cargo-audit
```

### 5. Run the full quality gate — all four checks must pass

Run these commands. If any fails, stop and report the error; do not proceed to
tagging.

```
cargo test --all --locked
cargo clippy --locked -- -D warnings
cargo fmt --check
cargo build --release
```

If `cargo fmt --check` fails, run `cargo fmt` to fix formatting, then re-run
`cargo fmt --check` to confirm it passes before continuing.

### 6. Update CHANGELOG.md

Read `CHANGELOG.md`. This project keeps an `[Unreleased]` section near the top.

Derive the release content from two sources and merge them:
- The existing `[Unreleased]` section, if it has any entries.
- The git log since the previous release tag:

  ```
  git log v<previous version>..HEAD --oneline
  ```

  If there is no previous tag yet (this is the first tagged release), list the
  full history instead:

  ```
  git log --oneline
  ```

Convert the `[Unreleased]` heading into a new `## [<new version>] — <today's date>`
section (use today's actual date), folding in anything from the git log that is
not already listed. Then leave a fresh, empty `## [Unreleased]` section above it
for future work.

Group entries into the standard changelog categories (Added, Changed, Fixed,
Removed, Documentation) — omit any category that has no entries. Exclude
housekeeping commits such as version bumps, `cargo fmt`, and `rustfmt` runs.
Write the entries in the same style as the existing changelog entries.

### 7. Commit the version bump and changelog

Stage `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`, then commit:

```
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "Bump version to <new version>"
```

### 8. Tag the release

```
git tag v<new version>
```

### 9. Confirm before pushing

Show the user the new version, the commit SHA, and the tag, then ask whether to
push both to origin. Pushing the `v<new version>` tag triggers the
`.github/workflows/release.yml` workflow, which builds the linux amd64/arm64 and
darwin arm64 binaries and publishes a GitHub release. If they confirm, run:

```
git push origin main
git push origin v<new version>
```
