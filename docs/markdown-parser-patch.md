# Desktop Markdown parser fork

## Pinned source and license

Zevria uses `markdown` **1.0.0** from the `liukaizheng/markdown-rs` GitHub fork,
commit **`e629e7e4370cac5cc6dac39971c77feba3e3aaf9`**. The commit was pushed to
that fork's `main` branch. The dependency is pinned to the complete commit, not
to a moving branch, through root `[patch.crates-io]`; `Cargo.lock` records the
same Git source/revision. There is no vendored parser or local-path override.

The fork starts at upstream markdown-rs commit
`1506572f9b406431402928f3a8b3df0b4ae2d8f5`, the 1.0.0 release. The equivalent
crates.io archive has SHA-256
`a5cab8f2cadc416a82d2e783a1946388b31654d391d1c7d92cc1f03e295b1deb`.
The parser is MIT, Copyright Titus Wormer; its original `license` file remains
in the upstream fork. No local license copies are maintained in Zevria.

The full parser source, generic regressions, patch ledger (`ZEVRIA.md`), and its
own workspace CI are maintained in the fork. The original library manifest,
MSRV (1.56), `to_mdast.rs`, and `to_html.rs` are unchanged. The fork's checked-in
workspace lockfile freezes the compatible historical SWC/Serde **test tooling**;
it does not constrain Zevria's dependency graph or downgrade its Serde versions.

## Why a parser patch

GPUI Kit 0.9.0's Markdown plugins receive completed mdast nodes. Upstream's
backslash delimiters are character escapes, not math nodes, so a renderer alone
cannot insert math objects into an existing text node or interrupt a paragraph
with display math. The isolated tokenizer patch creates ordinary `InlineMath`
and `Math` nodes while retaining the exact original source and spans.

No message preprocessing, provider replay rewriting, new parse-option fields,
Cargo features, AST/serde formats, or AST/HTML compiler changes are involved.
The TUI and provider's cacheable prompt prefix are untouched.

## Behavior / patch ledger

- `construct/tex_math_text.rs`: `\(…\)` before character escapes, using existing
  MathText events. Backslashes pair with the next byte; the first unpaired
  `\)` closes. Missing closers retain ordinary Markdown escape behavior.
- `construct/tex_math_flow.rs`: a bare `\[` line opens a raw display fence;
  an unpaired, line-ending `\]` closes it, with TeX allowed before the closer.
  Whole-line `\[…\]` and `$$…$$` forms are displays, but trailing prose declines
  them. Displays interrupt paragraphs and respect non-lazy container boundaries
  and indentation. `\\[2mm]` remains intact.
- `construct/raw_text.rs`: Pandoc-style single-dollar flanking, including
  Unicode whitespace and rejecting a closer followed by an ASCII digit.
  Invalid closers remain data and scanning continues. Currency such as
  `$5 and $10` and `costs $5-$10` stays prose.
- Dispatch/state wiring remains isolated in `construct/text.rs`,
  `construct/flow.rs`, `construct/mod.rs`, `state.rs`, and `tokenizer.rs`.
  Dedicated failed-scan memos keep unmatched inline openers linear-time.
- Existing MathText/MathFlow events feed the unmodified compilers. The original
  `math_text` / `math_flow` switches gate the new constructs; math-disabled
  output matches upstream. Raw flow is tried first, preserving legacy fences
  including `$$$`, code, and inline multi-dollar math mid-paragraph.
- A narrow crate-level Clippy style allowance preserves upstream compiler
  control flow on newer toolchains, and one existing tokenizer debug equality
  assertion uses `debug_assert_eq!`. Neither changes successful parser output.

Same-line displays can retract while their first line is still streaming:
`\[` at EOF can become `\[x` (prose), then `\[x\]` (display), then
`\[x\] tail` (prose). Last-block-only incremental parsers cannot always rejoin
an already committed preceding paragraph after such a retraction. The fork
records this boundary explicitly. Its generic append fixture separates these
ambiguous same-line forms with blank lines while still testing bracket fences
that interrupt prose without blank lines. Zevria's complete proof fixture tests
its no-blank-line fences at every character boundary.

## Regression coverage

Application-specific tests live in
[`crates/desktop/tests/markdown_math.rs`](../crates/desktop/tests/markdown_math.rs).
They use the shared assistant-output-only proof fixture and run on **all three
platforms**, with no GPUI or RaTeX dependency. Desktop's direct Markdown dependency
is test-only; native GPUI remains its only production consumer.

```sh
rtk cargo test --locked -p zevria-desktop --test markdown_math
rtk cargo test --locked -p zevria-desktop
rtk cargo fmt --all -- --check
rtk cargo check --locked --workspace --all-targets
rtk cargo test --locked --workspace
```

The proof must produce exactly 15 inline / 11 display nodes, exact delimited
source spans and raw aligned row spacing, and the specified top-level block
sequence. Tests also cover code/containers/links/tables, CRLF/tabs, currency,
disabled-mode golden mdast from the published crate, HTML output, linear-time
failed scans, and GPUI's last-block append model at chunks 1–16 and every
character boundary. GPUI tests additionally cover native copy, paint-only raster
allocation, streaming, fallbacks, and restored/Plan views.

The fork has its own generic fixture and full upstream workspace tests; no
application session records, provider data, local paths or proof fixture were
published there. At the pinned commit its workspace tests passed **207 tests**
with **11 ignored**; fmt and workspace Clippy passed (upstream style warnings
remain).

## Updating the pin

1. Work in the fork, not Cargo's registry cache. Keep the tokenizer delta
   isolated and avoid changes to public AST/options and the compilers.
2. Run the fork's fmt, locked all-features workspace tests and Clippy with debug
   assertions enabled. Keep generic public tests independent from Zevria data.
3. Commit and push the fork changes before updating Zevria. Verify the published
   commit, then set root `patch.crates-io.markdown.rev` to its full SHA and refresh
   the root lockfile (for example, `rtk cargo update -p markdown`).
4. Run the portable proof test, native desktop tests/gallery, workspace gates,
   and provider-prefix/cache-diagnostics gates. Confirm the lockfile source and
   build metadata select the intended Git revision, with no path fallback or
   unexpected dependency/version changes.
5. Verify CLI and non-macOS desktop production trees still contain no GPUI or
   RaTeX. Preserve existing session/replay bytes; never discard newer records
   from a live session just to recover an earlier whole-file hash.
6. Remove the Git override when an upstream release provides equivalent syntax,
   source spans, currency policy and streaming behavior. Do not remove it solely
   because a newer parser version exists.
