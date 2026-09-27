# Background-driven named themes

Zevria targets truecolor (24-bit RGB) terminals. Generate a complete coordinated
palette offline, without a provider, credentials, a TTY, a model invocation, an
installed skill, or a network connection:

```sh
zevria theme generate --name ocean --background '#1E1E2E'
zevria theme generate --name paper --background '#F5EEDF'
```

Both `--name` and `--background` are required. Quote the background so your shell
does not interpret `#` as a comment. RGB accepts either hex case and is saved as
uppercase `#RRGGBB`. Alpha, shorthand hex, terminal color names, repeated options,
unknown flags, and combinations with other startup modes are rejected.

Names are 1–64 lowercase ASCII characters. Start with a letter or digit, then
use letters, digits, underscores, or hyphens. Zevria appends `.toml`; do not pass
an extension, dot, path, separator, whitespace, or uppercase name.

## Files and selection

The generated document lives at:

```text
~/.zevria/themes/ocean.toml
```

Configuration contains **only the selection**, never RGB values or a theme path:

```toml
[theme]
name = "ocean"
```

Theme storage always uses `$HOME/.zevria/themes`, independent of the workspace
and `ZEVRIA_CONFIG`. The latter changes **which configuration selects the theme**
and, for session startup, also selects its sibling `models.jsonc`:

```sh
ZEVRIA_CONFIG=/path/to/work.toml zevria theme generate --name ocean --background '#1E1E2E'
ZEVRIA_CONFIG=/path/to/home.toml zevria theme generate --name ocean --background '#1E1E2E'
```

Both configurations can share the same named file. HOME is therefore required
for generation even with a custom configuration path.

A successful generation always saves **and selects** the theme; there is no
`--apply` flag. If configuration is absent, Zevria creates its normal commented
TOML first-run skeleton plus the selector; offline theme generation does not
create or require `models.jsonc`. Provider/model routing must be configured in that
sibling file before interactive use. Existing TOML comments and skill settings
are preserved, and model-file credentials, defaults, and comments are untouched.

A new TUI process validates and installs the selected palette before terminal
initialization, Connecting/Resuming frames, or syntax highlighting. Root,
Explore, and external-agent panes share one immutable process-lifetime palette,
including resumed sessions and fresh Plan handoffs. **Restart Zevria to activate
changes. Already-running TUI processes do not change.** Colors are not stored in
transcripts. Headless ACP processes do not open theme files or initialize the
renderer.

Without a selector, the exact built-in **Zevria Dark** palette remains the
default, including its `#282C34` canvas. There is no automatic background
detection, transparency, hot reload, or ANSI/256-color fallback.

## Collisions, retries, and failures

Names are not overwritten. If a different or invalid file already occupies a
name, choose another name. This protects all configurations sharing the file.
A valid, semantically identical definition can be reused: its bytes, comments,
permissions, and modification time are left untouched. Provenance differences
alone do not make otherwise identical palettes conflict.

Generation and post-rounding validation finish before persistence. A generation
failure changes neither theme files nor configuration. Once I/O starts, a
failed operation may leave directories or lock files, but does not damage an
existing theme or configuration.

Persistence is coordinated using a stable `.themes.lock` in the theme directory
and the existing `<config>.skills.lock`, in that order. The new theme and selector
are prepared and synchronized as private sibling stages. New files use
no-clobber publication; existing configuration uses guarded atomic replacement.
The **theme is committed before its selector**.

Two files cannot be replaced as one atomic transaction. If saving succeeds but
selection fails, the command fails with **“saved but not selected”** and a retry
instruction. The saved theme is kept, and this command does not update the
configuration. Fix the reported configuration permission/concurrency problem,
then retry the same command. Identical-file reuse makes this safe. If a
non-cooperating editor changed configuration, its changes are not overwritten.

Selected documents are bounded to **64 KiB**, must be UTF-8 regular files, and
cannot be symlinks. The theme directory must be a real directory. Missing,
malformed, unsupported-schema, or invalid selected themes cause an actionable
error **before terminal takeover**, not silent fallback. Recover by generating
under another name or resetting the selection. Recovery accepts any old selector
shape as long as the configuration itself is syntactically valid TOML.

## Reset

```sh
zevria theme reset
# Or reset only a particular configuration:
ZEVRIA_CONFIG=/path/to/work.toml zevria theme reset
```

Reset removes only the current configuration's `theme` section. It **never
deletes saved themes** or changes another configuration. Missing configuration
and an already-default configuration are successful no-ops; reset does not
create a first-run skeleton. `zevria clean` also preserves saved themes.

## How generation works

The Rust generator uses `palette` 0.7.7 to derive colors in perceptual **OKLCH**.
The input canvas RGB is preserved **exactly**, not shifted to make a palette
pass. Both light-foreground and dark-foreground arrangements are evaluated using
WCAG contrast and available tonal range; an HSL lightness cutoff does not decide
the theme polarity.

Restrained panel/overlay surfaces keep the background's hue and low chroma.
Workflow, speaker/tool, and syntax accents use seed-related hue candidates and
varied perceptual lightness. Feedback stays in recognizable green, amber, red,
and blue regions. Near-achromatic inputs (OKLCH chroma below 0.01) use a stable
255-degree blue seed rather than an unstable measured hue.

`oklch-search-1` has a versioned deterministic order and finite budget:

- Light foreground first, then dark foreground.
- Surface spacings 0.035, 0.018, and 0.008 in OKLCH lightness.
- Accent hue offsets 0, −12, +12 degrees; lightness 0.10–0.98 in 0.02 steps,
  filtered to the foreground polarity; chroma 0.07, 0.11, 0.15.
- A stable 24-wide beam chooses workflow and feedback groups, rejecting pairs
  below CVD separation floors. Passing candidates are scored for separation,
  coordination, text contrast, and surface hierarchy. Ties keep earlier entries.
- Text/border/selection tiers search 999 fixed lightness samples.
- Derived out-of-gamut colors reduce **chroma only**, holding lightness and hue,
  with 28 bisection iterations before rounding to sRGB bytes. Naive RGB clipping
  is not the gamut-mapping strategy.

The final **rounded 8-bit colors** must pass all checks:

- **4.5:1** normal and semantic text against canvas, panel, and overlay;
  selected text against selection; the existing text/syntax/diff foreground set
  against both tinted diff backgrounds.
- **3:1** strong borders and selection boundaries against adjacent surfaces,
  and workflow focus indicators against panels. Decorative borders are not
  treated as strong boundaries.
- Minimum pairwise **OKLab distances of 0.08** within workflows and **0.075**
  within feedback under full-severity protanopia, deuteranopia, and tritanopia
  simulation.

Contrast is a hard gate, never traded for an aesthetic score. Some saturated or
mid-tone exact backgrounds leave very little usable tonal range. Failure means
**no passing palette was found within the search budget**, not proof that the
background is mathematically impossible. Try a lighter or darker background;
Zevria will not silently change your canvas or weaken the checks.

Hue rotation alone cannot guarantee lightness separation, legibility, CVD
separation, or gamut safety. Copying the old foreground palette onto a new
background cannot preserve its contrast relationships either. Coordinated
perceptual generation with final contrast validation addresses those constraints
together rather than treating each component as an independent swatch.

## Stable document and semantic roles

A saved document has `schema_version`, `generator_version`, `source_background`,
and a complete `[palette]` with 27 concrete RGB references. The filename alone
is its identity. Unknown/missing fields, invalid RGB, and a source-background /
canvas mismatch are rejected. `generator_version` is provenance, **not a command
to regenerate**: a supported saved schema loads its fixed colors unchanged after
an upgrade.

One centralized reference-to-semantic graph resolves 38 rendering fields.
Heading aliases Explore; strong/type alias Plan; code aliases syntax string;
link/hunk alias info; addition aliases success; deletion/invalid alias error;
comment aliases muted. Selection foreground aliases the canvas. These aliases
are not independently editable fields in the file. Selection is a final override
of nested Markdown, syntax, diff, and role colors, preserving glyphs and modifiers.

CVD simulation is a useful quality check, **not a guarantee of identical
perception for every user**. Labels, status descriptions, diff `+`/`−` glyphs,
icons, and modifiers remain important non-color cues. Numeric checks do not
replace visual review in your truecolor terminal: inspect light/dark themes,
grayscale/CVD diagnostics, long diffs, code fences, selected rows, and modal
open/close behavior.
