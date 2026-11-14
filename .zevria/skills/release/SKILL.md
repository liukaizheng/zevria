---
name: release
description: >
  Prepare a Zevria release, including version bumps, categorized release notes
  (features, bug fixes, breaking changes, and related updates), validation,
  version tags, and review of workflow-created GitHub drafts. Use when asked to
  release Zevria, cut a new version, or write its release notes.
---

# Release Zevria

Follow the repository's release process rather than inventing another publishing
path. Deliver accurate, user-facing release notes and a verified release handoff.
Applying this skill does not authorize commits, pushes, workflow dispatches,
remote draft edits, or publication. Perform only the stages the user authorized;
if asked only for release notes, do not bump versions or start a release.

## 1. Establish the release scope

- Read `AGENTS.md`, `docs/releases.md`, `.github/workflows/release.yml`, the root
  `Cargo.toml`, and `crates/zevria/Cargo.toml`. Consult
  `docs/transcript-performance.md` for provider-prefix gates and `docs/windows.md`
  for platform acceptance requirements. Re-read these sources on each release;
  versions, targets, commands, and gates can change.
- Inspect the worktree, staged changes, branch, remotes, tags, and recent history.
  Preserve unrelated user work. Do not reset, clean, stash, or stage everything.
- Confirm the intended version, source commit/ref, previous release, and whether
  this is stable or a prerelease. If the version is unspecified, propose a bump
  with a reason and obtain confirmation before changing it. Backward compatibility
  is not required by this project, but breaking changes must still be documented.
- Determine the previous published release on the relevant ancestry/channel,
  using authenticated GitHub release information when available. For a stable
  release, account for changes since the previous stable release, not just the
  latest release candidate. Do not assume the highest local tag is published or
  that an API/authentication failure means no release exists. Report offline
  uncertainty and ask for the baseline if it cannot be established safely.
- Review the complete commit range and resulting diff, including configuration,
  CLI, protocol, transcript, installer, dependency, and platform changes. Include
  intended local changes only when they will be committed into this release.
  For the first release, review the intended source without inventing a base tag.

## 2. Prepare categorized release notes

Write in clear English unless another language was requested. Describe the user
impact, not merely commit subjects or implementation details. Consolidate related
commits, omit reverted or unreleased work, and substantiate claims from the diff,
PRs, tests, and documentation. Never invent features, issue numbers, performance
measurements, security fixes, compatibility, or platform acceptance.

Use these headings in this order, omitting empty sections rather than adding
"None" entries:

- **Breaking Changes**: incompatible CLI/config, Rust API, event protocol,
  transcript/storage, installation, or behavior changes. Explain the old and new
  behavior and the migration action. State explicitly when no migration exists,
  a manual reset is required, or older sessions/data cannot be used. Do not claim
  compatibility just because compilation passes.
- **Features**: new capabilities, with the relevant command or setting when useful.
- **Bug Fixes**: the affected behavior and how it is corrected.
- **Improvements**: user-visible refinements that are neither new features nor fixes.
- **Performance**: latency, throughput, memory, or caching changes; include numbers
  only with verified measurements and their context.
- **Security**: confirmed security changes and required user action; never disclose
  secrets or nonpublic vulnerability details without authorization.
- **Deprecations**: discouraged interfaces, replacements, and any known timeline.
- **Documentation**: significant guidance and onboarding changes.
- **Build and Dependencies**: packaging, installers, dependency/runtime requirements,
  CI, tests, and developer tooling that matter to consumers or maintainers.
- **Known Issues**: verified limitations, platform caveats, and available workarounds.

Put breaking changes first even when the originating commit was labeled `feat`,
`fix`, or `refactor`. Do not duplicate an item across categories unless a distinct
migration instruction is needed. Include issue/PR references only when verified.

Use this structure, replacing all placeholders and keeping only populated
categories. Use the actual local date when dating a release, never a guessed date.
The source must be the full immutable commit SHA, not a branch name:

```markdown
# Zevria v<VERSION>

<One or two sentences summarizing the release's most important user impact.>

## Breaking Changes
- <Changed contract or behavior; required migration or reset.>

## Features
- <New capability and how to use it.>

## Bug Fixes
- <Previously incorrect behavior and the correction.>

## Improvements
- <Meaningful refinement.>

## Performance
- <Verified improvement and measurement context, if measured.>

## Security
- <Confirmed security change and required action.>

## Deprecations
- <Deprecated interface and replacement.>

## Documentation
- <Significant documentation update.>

## Build and Dependencies
- <Packaging, dependency, CI, or tooling change.>

## Known Issues
- <Verified limitation and workaround, if available.>

Source commit: `<FULL_SOURCE_SHA>`
<!-- zevria-release-source: <FULL_SOURCE_SHA> -->
```

For a notes-only request before the source is finalized, label the result a draft
and state that the source SHA/marker is pending; do not invent a SHA or insert a
placeholder marker into a GitHub draft. Prepare a Markdown notes file in a
user-requested location or a temporary file, and report its path. Update an
existing changelog if the repository uses one; do not introduce a new permanent
changelog convention without being asked. The source marker belongs in the
GitHub notes, not a committed changelog: finalize it after the release commit
exists so the commit never needs to contain its own SHA.

## 3. Align the package version and validate

- The `zevria` package currently inherits `[workspace.package].version` from the
  root `Cargo.toml`. Inspect before editing. Keep workspace-inherited package
  versions in `Cargo.lock` consistent, without an unrelated dependency upgrade.
  Never hand-edit dependency checksums or use `cargo update` indiscriminately.
- Verify the final identity with locked Cargo metadata. The tag must be exactly
  `v` plus the `zevria` package version, including any prerelease/build suffix.
  Never reuse a published version or move/delete an existing release tag.
- Prefix shell commands with `rtk`. Run Cargo commands with `--release` whenever
  supported; profile-free commands such as `cargo metadata` and `cargo fmt` are
  exempt. Use `--locked` for validation after any intentional lockfile refresh.
- Derive checks from the current workflow and documentation. Local starting
  points, using the workflow's current desktop exclusion, are:

  ```sh
  rtk proxy cargo metadata --locked --no-deps --format-version 1
  rtk cargo fmt --all -- --check
  rtk cargo check --release --locked --workspace --exclude zevria-desktop --all-targets
  rtk cargo test --release --locked --workspace --exclude zevria-desktop
  rtk cargo build --release --locked -p zevria --bin zevria
  ```

- Preserve the provider's cacheable prompt prefix. Run the current explicit
  provider-prefix gates as well as the ordinary suite. Confirm each filtered gate
  selects at least one test before running it; a successful zero-test invocation
  is not evidence. When raw libtest listings are needed, use `rtk proxy` to avoid
  output filtering. Add `--release --locked` to local Cargo test invocations.
- Run the separate provider/host `cache-diagnostics` regressions required by the
  workflow. Never enable `cache-diagnostics` or `--all-features` on the production
  build; diagnostic tests and production packaging are separate invocations.
- Follow the documented help, exact-version, offline skill-listing, extracted
  package, installer, inventory, and checksum checks. Use disposable homes and
  workspaces for smoke tests so real credentials/configuration remain untouched.
- Do not claim local tests establish all native targets or real Windows/WSL
  acceptance. Report failures, unavailable tools, skipped checks, and missing
  platform evidence. Do not weaken gates or rewrite unrelated code to release.

## 4. Use the existing draft-release workflow

1. Review the version, notes, diff, and validation with the user. If committing is
   authorized, stage only intended release changes and commit them. Do not tag a
   dirty worktree or silently exclude intended files. Resolve the final committed
   source SHA and ensure the notes/marker identify that exact source.
2. Prefer an authorized build-only dispatch of `release.yml` at the exact source
   ref before tagging. A dispatch is always build-only, even for a version tag;
   it cannot create or repair a GitHub Release. Record the tested source and run ID.
3. After explicit version-tag push authorization, create an annotated `v<VERSION>`
   tag at the reviewed commit and push only that tag to the confirmed remote.
   Check both local and remote tag/release state first. Stop on conflicts; never
   force-push a release tag. The tag run independently rebuilds its own source.
4. Wait for the entire tag workflow, all native targets, bundle assembly, and
   draft upload verification to succeed. An independent green `test.yml` run is
   not a substitute. Let the workflow create its draft; do not bypass it with a
   hand-created release or manually substituted artifacts.
5. Inspect the draft identity, prerelease flag, source SHA, exact Rust version,
   assets, and checksums. The current bundle contains the three platform archives
   and `SHA256SUMS`; derive exact names/targets from the current workflow. These
   packages are unsigned and unnotarized, not package-manager publications.
6. Once no run/retry is active, replace generated commit-list notes with the
   reviewed categorized notes only if remote draft editing is authorized. First
   read the existing body and preserve exactly one matching
   `<!-- zevria-release-source: FULL_40_CHARACTER_SOURCE_SHA -->` marker. Preserve
   useful attribution/compare links without leaving a second uncategorized change
   list. Keep the release a draft and preserve its tag, title, and prerelease flag.
   Inspect the installed `gh release edit --help` before using `--notes-file`, and
   verify the resulting draft body/status after editing.
7. Hand the reviewed draft to the maintainer for separate manual publication.
   Never publish automatically as a side effect of preparing notes or tagging.
   Require complete assets, a successful final workflow, appropriate platform
   evidence, and no active retry before recommending publication.

## 5. Recover safely and report evidence

On failure, follow `docs/releases.md`. A failed create/upload can leave an
incomplete draft: never recommend publishing it. Retry the original run for
transient failures; reruns retain the original source and do not pick up fixes
committed later. Source/workflow fixes need a new reviewed, aligned version/tag,
not a moved tag. Preserve the source marker and maintainer edits so draft retries
remain valid. Stop on published, unmarked, or conflicting releases rather than
claiming or overwriting them.

Finish with the version/tag, baseline and source SHA, notes file or draft location,
files changed, validation results, workflow run IDs, remaining blockers, and the
exact next authorized/manual action. Clearly distinguish prepared, tagged,
built, draft-ready, and published; never claim a release was published merely
because a tag was pushed or a draft exists.
