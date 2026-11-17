# Image prompts and clipboard paste

## Using the composer

1. Copy a screenshot or bitmap with your desktop application's Copy action.
2. Enter **Insert mode** (`i`). Type any surrounding text and move the caret.
3. Press **Ctrl-V**. Zevria reads the native clipboard off the terminal event loop and inserts a numbered inline **`[image N]`** token at the caret.
4. Press **Ctrl-Enter** to submit. The backed token becomes a real image content block at that position—not text sent to the model.

Image-only prompts and multiple images work. Pasting the same image twice creates two distinct attachment occurrences. Typed or terminal-pasted lookalikes such as `[image 1]` are only text and use ordinary text editing.

**Backspace** inside or immediately after a registered image token removes its whole marker and attachment. At the start of a token, Backspace targets the preceding content, not the image ahead of the caret. Other edits through a registered token, such as inserting text or deleting to the end of a line, still invalidate its attachment without automatically removing the remaining marker text. **Ctrl-C** clears the whole draft, including image-only drafts, and cancels pending paste results.

Ctrl-V acts only on an editable Insert-mode composer. It does not switch modes. Busy, selected-history, inspect-only, and modal views do not read the clipboard. During a paste the originating draft is briefly locked, but pane navigation and Ctrl-C remain available. Switching panes cannot redirect a pending result. Clipboard failures and timeouts leave the previous draft unchanged. One native operation may run at a time, including after a timeout or session switch.

Image retrieval has priority over accompanying clipboard text. If no bitmap is returned (including a native retrieval error), Zevria tries text. If a bitmap is returned but fails validation or bounded image preparation, the paste is rejected without text substitution. Oversized clipboard images are prepared on the same worker thread; a resized notice is local display only, never model instructions or history. Native text fallback uses multiline caret insertion. Terminal bracketed-paste events remain **text-only**. Copied paths, URLs, and data URLs are never opened or fetched as images. OSC 52 copying remains independent of native reading.

## Workflows, recall, and controls

Images are user arguments in Build/Plan turns, `/orchestrate <prompt>` requests, enabled `$skill` invocations, transcript replacements, `/ensemble-plan` and `/ensemble-review` initial requests, and Plan-worker feedback. Images do not enable skills, tools, mutations, implementation, or additional worker permissions.

Host controls such as `/build`, `/plan`, `/implement`, `/implement-fresh`, `/confirm`, `/baseline`, `/retry`, and `/new` do not accept attachments. Rejection retains the draft without executing the control. Leading-space literal escapes and worker `//` escapes remain available. A leading image cannot hide a host command that becomes exposed by removing its token.

Selecting a submitted text or image block and recalling it restores the **whole** ordered user prompt. Skill, ensemble, and typed orchestration prefixes are preserved. `/orchestrate` strips only its own leading prefix and keeps the ordered text/image remainder, including image-only arguments. A bare command without text or images is rejected. Removing the prefix on recall clears orchestration intent; keeping it creates a new request and obligation. Cancelling recall restores the previous complete draft. Pre-acceptance failures restore the attempted draft without truncating prior history. A provider failure after acceptance instead leaves the submitted prompt in history.

Submitted images render as bounded labels, for example `[image 1 · png · 1440×900]`. There are no graphical thumbnails or attachment strips. Session previews and text copy/yank contain labels rather than image bytes.

## Formats and limits

| Limit | Value |
|---|---|
| Images per prompt | 8 |
| Encoded source bytes per image | At most 32 MiB before preparation; base64 is bounded before decoding |
| Prepared raster bytes per new image | At most 3,932,112 bytes after base64 decoding |
| Per-image base64 transport budget | 5,242,880 bytes (5 MiB), including a 64-byte data-URL reserve |
| Aggregate prepared raster bytes | 20 MiB per prompt; at most 8 ordered occurrences, including duplicates |
| Width or height | At most 16,384 pixels; nonzero |
| Decoded pixel/work budget | 40 million per image, including animation frame allocations/work |
| Clipboard bitmap encoding | Lossless PNG |
| Embedded ACP formats | PNG, JPEG, static WebP, GIF; compliant bytes and MIME retained exactly |
| Preparation work / buffers | At most 12 encodes, 640 million cumulative pixel-work units, 512 MiB major-buffer budget |
| Worker feedback / ACP skill prose | Separate 64 KiB text limit; image bytes are not prose |

### Automatic preparation of new images

Validated images within the prepared-raster cap remain byte-for-byte unchanged, including MIME. Only oversized new inputs are re-encoded: first at the same dimensions to remove nonessential metadata, then downscaled if the actual encoding still exceeds the target. Resizing preserves aspect ratio (within integer rounding), never upscales or crops, and samples each candidate from the original decoded pixels using the pinned image library's Catmull–Rom bicubic filter. No sharpening pass is applied; small text may lose readability after substantial reduction.

Clipboard/PNG and oversized single-frame GIF use lossless PNG; JPEG uses fixed quality 90; static WebP uses lossless WebP. Transparency is preserved with premultiplied-alpha resampling. Re-encoding applies EXIF orientation and keeps supported ICC color profiles up to 1 MiB. Submitted MIME, dimensions, byte counts, identity, and context accounting describe the actual output. Local source files and clipboard content are never overwritten.

The raster cap is derived as `3 * ((5 * 1024 * 1024 - 64) / 4)`; both the final raster length and `4 * ceil(raster_bytes / 3) + 64 <= 5,242,880` are checked. This conservative per-image compatibility policy is **not a whole-request size guarantee**, a provider limit claim, or an accounting rule. No model-name heuristics or provider-specific resizing settings are used. The separate eight-image / 20 MiB normalized aggregate budget still applies: Zevria does not drop occurrences or shrink an entire prompt to fit it.

GIF frames are validated within bounded work/allocation limits. Under-target GIF bytes remain unchanged, including animation. Oversized multi-frame GIF is rejected with a manual-conversion diagnostic rather than flattened. Animated WebP/APNG are explicitly unsupported, with no first-frame substitution. Inputs exceeding source, dimension, decoder, profile, preparation-buffer, work, or attempt limits may require manual conversion/downscaling into a temporary PNG/JPEG. Native clipboard APIs may allocate a bitmap before Zevria can inspect it; application-side checks cannot prevent that backend allocation.

Preparation happens once, before accepting/persisting new clipboard, ACP, programmatic, recalled-and-resubmitted, edited, skill, ensemble, or worker-feedback input. Already prepared attachments are a cheap no-op. Cancellation/preparation failure does not partially accept a prompt or evidence; retries, synthesis forwarding, replay, and resume reuse accepted bytes without another transformation.

## Model-aware context accounting

Encoded raster/PDF bytes are **never prose tokens**. Accounting recognizes direct
images, image tool results, Responses tool-output arrays, Messages nested
`tool_result` blocks, and supported native replay. Image-looking text and arbitrary
tool JSON remain literal content. Binary PDF processing is unmeasured locally;
there is no PDF tokenizer or encoded-byte allowance. Unknown provider-native
processing remains an unmeasured opaque provider item; its keys cannot invent
images or turn an unknown encoded blob into prose tokens.

Images not covered by compatible completed-response usage prefer provider input
counting. Once usage covers them, same-mode text/tool appends retain that baseline
and add only the suffix. Cached input is already part of input usage, never an
extra context charge. The last response's usage and the **next input projection**
are different quantities. History edits, compaction, incompatible request/profile
changes, and accounting-policy changes invalidate measurements. Resume derives
local facts again; it does not invent a durable measured baseline from old usage.

There is no universal per-image **token** allowance. Each `models.jsonc` model can set a
**local-only** `image_accounting` policy, for either protocol:

```jsonc
"image_accounting": { "rule": "auto" } // default
// Explicitly keep an unmapped model unresolved:
// "image_accounting": { "rule": "unresolved" }
// Only after verifying that this gateway alias uses this exact documented rule:
// "image_accounting": { "rule": "preset", "model": "gpt-4.1-mini-2025-04-14" }
```

Automatic lookup requires both an exact allowlisted model identity and an official
HTTPS endpoint. A gateway label, compatible protocol, or similar model name is not
evidence of equivalent processing. Presets/rule version 1 are pinned in
`crates/foundation/src/image_accounting.rs`, with provider-source provenance and
golden tests. Unsupported identities/details remain unknown. Verified tile-model
`auto` uses the documented high-detail sizing rule; verified patch presets map
low/high/auto according to their own model table, not another family's defaults.
The Anthropic renderer sends no detail preference, so Rig detail never changes
its local calculation.

Expert overrides can select `openai_patch`, `openai_tile`, or `anthropic_patch`.
Patch parameters include patch size, resizing edge/budget, rational multiplier,
and optional low/original-detail rules; tile parameters include base/tile costs,
tile size, maximum/short edge, and effective auto detail. Anthropic parameters
include patch size, resolution/patch limits, and the whole-request many-image
threshold/dimension constraint. Invalid, zero, excessive, or protocol-incompatible
parameters are rejected. These parameters affect **accounting only**: they never
resize images, change transmitted detail, insert instructions, or alter prompt
cache/continuation identity. Intrinsic facts are policy-independent; numerical
caches include the policy and rule version. Each duplicate occurrence is charged.
Image facts reuse validated attachments or bounded header parsing; URLs/file IDs
are not fetched to discover dimensions.

Local text/envelope estimates remain approximate even with complete media
coverage. Provider counts take precedence (Claude qualifies its counts as
estimates). HTTP 404/405/501 cache endpoint unsupported status; temporary counting
failures suppress retries for the turn, not the provider's capability forever.

When image costs remain unresolved after counting is unavailable, Zevria uses
**best effort**: it shows `estimated known portion: N; 3 unmeasured images`, not a
fabricated total or percentage, and permits a bounded ordinary completion. This
can exceed the configured **local input-token budget**, even if the provider
accepts it. A large known portion can spend one compaction attempt. Known complete
over-limit requests, invalid/unsupported content, persistence/replay failures, and
cancellation are not bypassed.

A confirmed **pre-execution token-window rejection** can spend the pending model
round's one remaining compaction/retry allowance. Admission/pre-dispatch
compaction spends the same allowance; changing the request fingerprint does not
replenish it. Completed local tool batches are not rerun, failed partial calls
never execute, and attempt evidence remains display-only. A second rejection,
spent/disarmed budget, unavailable/failed summary, or uncertain upstream effects
stops recovery. A bare HTTP 413 is an upload/body-size failure, not proof of token
overflow; auth/quota/output limits and possibly executed failed/incomplete
responses cannot trigger this replay. Synthetic summary calls and existing
bounded transport retries are separate from the one ordinary completion retry,
so recovery may involve more than two wire transmissions.

## Model image reads (`read_image`)

During ordinary root **Build** and **Plan** turns, the model can call
`read_image` with `{ "file_path": "capture.png" }` to inspect a local screenshot,
diagram, or UI capture. The result contains a readable summary (canonical path,
MIME format, dimensions, and byte count) followed by a real image content block,
not base64 text. The TUI shows `read_image · capture.png`, the summary, and
`[image]`; ACP clients receive an image-output omission placeholder rather than
the payload. Explore children, Build children, and ensemble workers do not
register this tool. Ensemble root synthesis keeps its narrower explicit tool
allow-lists and does not enable it.

Relative paths resolve against the tool's startup workspace. Absolute paths may
point **anywhere readable**, including outside that workspace; symlinks are
followed, and their targets must be regular files. No URL fetching is performed.
The model can therefore send a sensitive image from outside the repository to a
provider: check the requested path and protect local files and session history.
Use `command` to read text files.

The image format is detected from file contents, regardless of extension. PNG,
JPEG, WebP, and GIF use the same source/preparation limits as pasted images:
**32 MiB** encoded source bytes, **3,932,112 bytes** prepared raster,
**16,384 pixels** per side, and a **40 million** decoded-pixel/animation-work
budget. Oversized static images are automatically prepared without changing the
source file. When preparation changes an image, the summary also reports the
original-to-prepared dimensions and encoded size. Unsupported animations and
images beyond safety limits need manual conversion to a temporary PNG/JPEG.
Loading, validation, and preparation run on a blocking thread, not the async event loop.

Across all tool calls in **one model response**, at most **8 images / 20 MiB** of
image data (before base64 expansion) are admitted in assistant-call order. A
later result that would exceed either budget becomes an error result with a
request-fewer-images diagnostic; an individual result is not partially kept.
This is a per-response budget, not a cap on all images accumulated in a session.
Image results participate in the same context accounting as pasted images:
unmeasured images prefer exact endpoint counts when available, compatible
provider usage covers existing images, and local fallback uses the destination's
verified image policy or explicit incomplete-accounting best effort. Reading a
file again adds a new image occurrence and consumes context again.

**Read image bytes persist as plaintext base64 in session history**, like pasted
images, and are included in later model requests and restored on resume. There
is no automatic temporary-file cleanup or encrypted image store. Tool
declarations are registered last in the stable root registry; older resumed
sessions append the newly available declaration as `Additional` rather than
rewriting their pinned declaration prefix. Revised guidance likewise appends an
`Additional` declaration without rewriting the initial prefix. Availability does not vary by model
or turn.

There is no per-model vision-capability gate. A text-only model that calls
`read_image` receives a provider error; because the image stays in history,
subsequent requests can keep failing until compaction or a new session removes
it from active context. Providers may enforce stricter image/request limits,
and some Responses-compatible gateways may reject image-bearing
`function_call_output` arrays. These failures remain visible; Zevria never
silently drops images or rewrites accepted history to make a request succeed.

## ACP and ensembles

Root and worker ACP profiles advertise `promptCapabilities.image: true`. `session/prompt` accepts an ordered array of text and embedded image blocks, including image-only input. Audio and resource blocks are rejected; URLs and resource paths are not resolved. Skill extension **version 1** accepts the same ordered blocks in `args`. Generic prompt text does not invoke skills implicitly. An image-bearing `/implement` cannot authorize implementation.

External workers must advertise image input support. Unsupported initial requests fail explicitly. New image feedback is rejected until capability is negotiated; reconnection checks capability again. Already accepted input is retained if a changed agent cannot execute it. Explicit retry retains failed image bytes even if the worker allocated a session before rejecting the image; it never substitutes a text-only continuation for that failed input. Confirmation, baseline selection, abandonment, and exact-revision rules are unchanged.

Root synthesis carries each original image occurrence once, outside the quoted report JSON. Confirmed workers' successfully incorporated feedback images are associated with their worker and generation. Abandoned, failed, and unincorporated feedback is not synthesis authority. Potential evidence is checked against shared image count/byte limits and the current root context/record budgets before accepting feedback or retry and confirming proposals. Exact counts are preferred; a final complete check precedes sealing. Reports that are not yet published can only contribute known evidence to the earlier projection, so confirmation checks again. Image evidence is never silently truncated to fit report-text budgets.

## Persistence and privacy

**Submitted screenshots persist in plaintext session history as embedded base64.** Root transcripts and associated worker journals are self-contained for resume, recall, retry, and recovery; changing the clipboard later does not alter submitted images. Root and worker records have a 64 MiB bounded read/write ceiling to accommodate image-bearing mirrors and base64 expansion. Skills use dedicated `zevria_skill_invocation` records with ordered arguments; worker/review formats remain version 1. Unsupported owned formats are rejected, not automatically migrated.

Historical images are **not automatically migrated or newly rejected** for exceeding the new preparation target. Stored-image deserialization, recall, native/generic provider replay, transport, and accounting remain lossless; unchanged history, instructions, pinned declarations, and cache keys are not rewritten. If an old attachment causes provider failure, use compaction/new session and reattach, or deliberately replace it through the existing transcript-edit workflow. Recalling alone does not mutate history; submitting the recalled draft is new input and prepares only that replacement/suffix. Lower gateway limits, whole-request HTTP 413 errors, provider dimension restrictions, unsupported vision models, image URLs, and file IDs remain outside this per-image policy.

Normal image labels and diagnostic copies omit payloads. Known structured image fields, data URLs, and long opaque base64 diagnostic runs are redacted; this is not a general secret scrubber for arbitrary model or tool prose.

Drafts and pending clipboard results are memory-only. There is no temporary image store, upload service, blob garbage collector, encryption, or new cleanup policy. Images follow existing history retention/deletion behavior. Screenshots may contain secrets—review them before submission and protect session directories. Summaries can describe an image but cannot reconstruct its bytes; original durable history remains available for recall.

## Desktop and terminal limitations

Native reading uses pinned `arboard 3.6.1` with image support and Linux Wayland data-control support. X11 and Wayland access depends on the desktop environment, compositor protocols, clipboard owner, and process environment. On Windows clipboard contention may fail locally. Terminals may intercept Ctrl-V; configure them to forward the key to Zevria if needed.

Headless processes and SSH sessions usually cannot access the local terminal client's desktop image clipboard. No remote clipboard forwarding or image fetching is added. Use an ACP client capable of sending embedded images when native desktop access is unavailable.

Automated tests use fake clipboard backends, never the real system clipboard. Live macOS, Windows, Linux/X11, Linux/Wayland, terminal interception, headless/SSH, and vision-endpoint checks require separate manual acceptance; dependency API verification and unit tests do not establish that platform coverage.
