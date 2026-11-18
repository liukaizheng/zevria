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

**CI-only execution:** do not build or validate on the local machine, even when
reference documentation includes local command examples. Do not run local Cargo
commands, tests, format/lint checks, CLI or installer smoke tests, or archive
inventory/checksum scripts, and do not reproduce CI failures locally. Local work
is inspection, editing, Git operations, and reading/recording GitHub status,
logs, and receipts. Push the reviewed source and watch GitHub Actions; all
executable release gates run there.

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

**Every changelog item must be concise.** This applies to both GitHub release
notes and any committed changelog. Use one short sentence per bullet whenever
possible: state what changed and include only essential context or required
user action. Cut implementation details and repetition before finalizing.

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

## 3. Align the package version and delegate validation to GitHub CI

- The `zevria` package currently inherits `[workspace.package].version` from the
  root `Cargo.toml`. Inspect before editing. Make targeted version-only changes
  to the manifests and corresponding local package entries in `Cargo.lock`;
  do not upgrade dependencies or hand-edit their checksums. Let CI's locked
  Cargo commands report any inconsistency; do not run Cargo locally to check it.
- Read the final package identity from CI's locked metadata evidence. The tag
  must be exactly `v` plus the `zevria` package version, including any
  prerelease/build suffix. Never reuse a published version or move/delete an
  existing release tag.
- Prefix shell commands with `rtk`. Inspect the current workflow and docs for
  required CI gates, targets, desktop exclusions, toolchain, profiles, and
  locking policy. Required coverage includes formatting, locked workspace and
  all-target checks/tests, and the default-feature production release build.
  Do not provide or execute a local validation/build recipe.
- Preserve the provider's cacheable prompt prefix. CI must run the current
  explicit provider-prefix gates as well as the ordinary suite, checking raw
  libtest listings so every filtered gate selects at least one test. A green
  zero-test invocation is not evidence.
- CI must run the separate provider/host `cache-diagnostics` regressions required
  by the workflow. Never enable `cache-diagnostics` or `--all-features` on the
  production build; diagnostic tests and production packaging remain separate.
- CI must perform the help, exact-version, offline skill-listing, extracted
  package, installer, inventory, checksum, and uploaded-asset checks. Hosted
  smoke tests use disposable homes/workspaces, not real credentials/config.
  Inspect CI receipts rather than running these checks locally.
- Inspect hosted coverage before tagging. If a required check is missing, add or
  repair it in CI within the authorized source-change scope, or report a blocker.
  Never substitute local checks, skip a gate, or weaken an assertion to release.
  CI does not establish real Windows/WSL or interactive acceptance; collect
  maintainer evidence and report remaining manual checks without running them
  on the local machine.

## 4. Use the existing draft-release workflow

1. Review the version, notes, diff, and CI plan with the user. If committing is
   authorized, stage only intended release changes and commit them. Do not tag a
   dirty worktree or silently exclude intended files. Resolve the committed
   source SHA after every amendment and keep pending notes/checkpoints aligned.
2. With push/dispatch authorization, push the source branch to the confirmed
   `origin` and watch its CI. Run the existing `release.yml` build-only dispatch
   at that source ref before tagging; all required source gates must pass at the
   final source SHA. Use the repair loop below for source failures. A dispatch
   is always build-only, even for a version tag; it cannot create or repair a
   GitHub Release. Verify the run's event, ref, and immutable source rather than
   assuming a selected branch stayed unchanged. Record its run ID and attempt.
   After an amendment, watch newly triggered CI and dispatch the new source;
   rerunning an old run cannot test the amended commit.
3. After explicit version-tag push authorization, create an annotated `v<VERSION>`
   tag at the reviewed commit and push only that tag to the confirmed remote.
   Check both local and remote tag/release state first. Stop on conflicts; never
   force-push a release tag. The tag run independently rebuilds its own source.
4. Wait for the entire tag workflow, all native targets, bundle assembly, and
   draft upload verification to succeed. An independent green `test.yml` run is
   not a substitute. Let the workflow create its draft; do not bypass it with a
   hand-created release or manually substituted artifacts.
5. Inspect the draft's numeric release ID, tag, prerelease flag, source SHA, exact
   Rust version, assets, and checksums. Derive exact asset names/targets from the
   current workflow; its current bundle has three platform archives and
   `SHA256SUMS`. These packages are unsigned and unnotarized, not package-manager
   publications.
   - Read the exact Rust version from that run's recorded prepare outputs or
     summary. If reading rustup logs, select the installed/updated/unchanged
     toolchain result, not a parenthetical previous version. A broad search for
     every `rustc` version can include both old and selected compilers; stop if
     the selected version cannot be established unambiguously.
   - Read hosted bundle/upload verification receipts for the explicit successful
     run ID and attempt. CI must verify the expected inventory, archive members,
     and `SHA256SUMS` against archive bytes, and compare uploaded GitHub assets
     with the successful tag run's bundle. Names, sizes, and API digest metadata
     alone do not replace a required hosted gate. Missing CI evidence is a
     blocker, not a reason to run local archive/checksum helpers. Stop on
     unexpected inventory; do not delete unrelated assets to make it pass.
   - Inspect the GitHub asset metadata and CI-confirmed hashes against the tag
     run, not the build-only run. Independent builds can produce different
     archive hashes at the same source; do not require dry-run/tag-run hash
     equality or substitute dry-run artifacts.
6. Once no run/retry is active, replace generated commit-list notes with the
   reviewed categorized notes only if remote draft editing is authorized. First
   read the existing body and preserve exactly one matching
   `<!-- zevria-release-source: FULL_40_CHARACTER_SOURCE_SHA -->` marker. Preserve
   useful attribution/compare links without leaving a second uncategorized change
   list. Keep the release a draft and preserve its tag, title, and prerelease flag.
   - Immediately before editing, re-read the release by numeric ID and check
     that its body, identity, assets, and latest workflow attempt still match the
     reviewed evidence. Reconcile unreviewed maintainer changes rather than
     overwriting them, and reverify evidence invalidated by a retry.
   - Inspect the installed `gh release edit --help` before using `--notes-file`.
     After editing, re-read by release ID and verify the reviewed body, exactly
     one matching source marker, draft/tag/title/prerelease identity, and
     unchanged verified asset IDs, upload states, sizes, and digests.
   - Capture the fresh `html_url` after the edit. A draft URL can change while its
     release ID and tag remain the same; do not use an old URL as identity or
     report it without refreshing.
7. Hand the reviewed draft to the maintainer for separate manual publication.
   Never publish automatically as a side effect of preparing notes or tagging.
   Require complete assets, a successful final workflow, appropriate platform
   evidence, and no active retry before recommending publication.

## 5. Recover safely and report evidence

### Monitor hosted workflows and handle CLI differences

- Inspect installed `gh` subcommand help before assuming flags can be combined.
  If `--slurp` is rejected with `--jq` or `--template`, obtain complete paginated
  JSON with `--paginate --slurp` and parse it locally. Preserve every page and
  propagate API errors; never turn a failed lookup into an empty release list.
- Monitor the recorded run ID with structured status/job data. Check the source,
  trigger/ref, latest attempt, `status`, and `conclusion`; only a completed run
  with a successful conclusion establishes workflow success. A nonzero
  `gh run watch` exit can instead be an EOF, timeout, or API transport failure.
  Retry transient read-only requests with bounded backoff, without redispatching
  the workflow, rerunning jobs, or changing tags merely to repair monitoring.
- Logs may be unavailable through `gh run view --log` while a run is in progress.
  Poll structured status/jobs until logs are available, then inspect them;
  unavailable logs alone are not a failed job. Persistent API/authentication
  failures leave state unverified and block release mutations; they do not
  justify bypassing a gate.

### Repair source failures before tagging

Within an authorized release/CI-repair scope, use this loop for actual source or
workflow failures. Do not run local builds, tests, linters, smoke tests, or other
validation to reproduce or check a fix.

1. Read the failing GitHub job/step logs and make the necessary scoped local
   source/workflow edits. Preserve provider-prefix behavior and existing gates;
   do not rewrite unrelated code or weaken tests to obtain green CI.
2. Inspect the branch tip, worktree, staged diff, and remote refs. Amend only the
   agent-owned last commit for this ongoing release, before any release tag
   references it. Preserve unrelated user work and record the confirmed remote
   source-branch SHA before rewriting. If the tip is unrelated or already tagged,
   stop rather than amending it.
3. Stage only the intended fix files and amend that commit, normally with
   `rtk git commit --amend --no-edit`. If unrelated changes are already staged,
   stop and resolve scope instead of including or unstaging them. Update a stale
   commit message if needed; do not attach old validation claims to changed source.
4. Push the amended source branch to `origin` with an explicit lease, never an
   unconditional `--force`. Substitute the previously confirmed, nonempty
   source-branch name and remote SHA in this form:

   ```sh
   rtk git push --force-with-lease="refs/heads/$branch:$expected_remote_sha" \
     origin "HEAD:refs/heads/$branch"
   ```

   Rewrite only that untagged source-branch tip, not all refs or any release tag.
   If the lease fails or branch protection rejects the push, stop and report the
   conflict; do not refresh the expected SHA blindly, discard someone else's
   commits, bypass protection, or escalate to an unconditional force push.
5. Record the new full source SHA and invalidate prior-source CI receipts and
   pending source markers. Watch the newly triggered branch CI and start a new
   build-only release run at the amended source. Check its actual source/run ID;
   do not rerun an older workflow expecting it to pick up the fix. Repeat the
   local fix -> amend -> force-with-lease -> GitHub CI loop until required gates
   pass, then finalize the reviewed notes/source marker and tag that exact SHA.

### Recover from transient or tagged-workflow failures

For a confirmed transient infrastructure failure with no source change, follow
`docs/releases.md` and retry the original run within the authorized scope rather
than manufacturing an amendment. A rerun retains its original source. After a
release tag exists, a source/workflow fix needs a new reviewed, aligned
version/tag; never amend a tagged release commit or move an existing release tag
to repair it. All builds and verification of any fix still run in GitHub CI.

A failed create/upload can leave an incomplete draft: never recommend publishing
it. Preserve the source marker and maintainer edits so draft retries remain
valid. Stop on published, unmarked, or conflicting releases rather than claiming
or overwriting them.

### Checkpoint and resume without duplicating actions

- Keep concise receipts in an ignored per-release directory, such as
  `target/release-prep/v<VERSION>/`. Record the version/tag, baseline and full
  source SHA, confirmed remote, notes path, workflow run IDs/attempts and exact
  toolchains, numeric release ID, verified asset inventory/hashes, completed
  stages, and next action. Do not store credentials or copy release-specific
  values and incident logs into this permanent skill.
- On continuation, inspect the current worktree, remote refs, release identity
  and body, latest run attempts, and artifacts against those receipts. Reuse
  completed-stage evidence only while its relevant source and verification
  context still match. Re-establish changed validation evidence through GitHub
  CI, not local commands. An intentional repair amendment changes the source
  and needs fresh CI receipts; mere continuation must not duplicate commits,
  pushes, dispatches, tag creation, or note edits.
- Checkpoints are evidence, not authorization or a replacement for required
  gates. Stay within the user's authorized stages, preserve maintainer edits,
  and retain the manual-publication boundary. Conflicting source/tag/release
  state requires review and the recovery procedure, never a moved release tag.

Finish with the version/tag, baseline and source SHA, notes file or draft location,
files changed, GitHub CI results, workflow run IDs/attempts, release ID and fresh
URL, remaining blockers, and the exact next authorized/manual action. State that
no local builds/validation were run. Clearly distinguish prepared, tagged, built,
draft-ready, and published; never claim a release was published merely because a
tag was pushed or a draft exists.
