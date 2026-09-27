# Image prompts and clipboard paste

## Using the composer

1. Copy a screenshot or bitmap with your desktop application's Copy action.
2. Enter **Insert mode** (`i`). Type any surrounding text and move the caret.
3. Press **Ctrl-V**. Zevria reads the native clipboard off the terminal event loop and inserts a numbered inline **`[image N]`** token at the caret.
4. Press **Ctrl-Enter** to submit. The backed token becomes a real image content block at that position—not text sent to the model.

Image-only prompts and multiple images work. Pasting the same image twice creates two distinct attachment occurrences. Typed or terminal-pasted lookalikes such as `[image 1]` are only text and use ordinary text editing.

**Backspace** inside or immediately after a registered image token removes its whole marker and attachment. At the start of a token, Backspace targets the preceding content, not the image ahead of the caret. Other edits through a registered token, such as inserting text or deleting to the end of a line, still invalidate its attachment without automatically removing the remaining marker text. **Ctrl-C** clears the whole draft, including image-only drafts, and cancels pending paste results.

Ctrl-V acts only on an editable Insert-mode composer. It does not switch modes. Busy, selected-history, inspect-only, and modal views do not read the clipboard. During a paste the originating draft is briefly locked, but pane navigation and Ctrl-C remain available. Switching panes cannot redirect a pending result. Clipboard failures and timeouts leave the previous draft unchanged. One native operation may run at a time, including after a timeout or session switch.

Image retrieval has priority over accompanying clipboard text. If no bitmap is returned (including a native retrieval error), Zevria tries text. If a bitmap is returned but fails validation or PNG encoding, the paste is rejected without text substitution. Native text fallback uses multiline caret insertion. Terminal bracketed-paste events remain **text-only**. Copied paths, URLs, and data URLs are never opened or fetched as images. OSC 52 copying remains independent of native reading.

## Workflows, recall, and controls

Images are user arguments in Build/Plan turns, `/orchestrate <prompt>` requests, enabled `$skill` invocations, transcript replacements, `/ensemble-plan` and `/ensemble-review` initial requests, and Plan-worker feedback. Images do not enable skills, tools, mutations, implementation, or additional worker permissions.

Host controls such as `/build`, `/plan`, `/implement`, `/implement-fresh`, `/confirm`, `/baseline`, `/retry`, and `/new` do not accept attachments. Rejection retains the draft without executing the control. Leading-space literal escapes and worker `//` escapes remain available. A leading image cannot hide a host command that becomes exposed by removing its token.

Selecting a submitted text or image block and recalling it restores the **whole** ordered user prompt. Skill, ensemble, and typed orchestration prefixes are preserved. `/orchestrate` strips only its own leading prefix and keeps the ordered text/image remainder, including image-only arguments. A bare command without text or images is rejected. Removing the prefix on recall clears orchestration intent; keeping it creates a new request and obligation. Cancelling recall restores the previous complete draft. Pre-acceptance failures restore the attempted draft without truncating prior history. A provider failure after acceptance instead leaves the submitted prompt in history.

Submitted images render as bounded labels, for example `[image 1 · png · 1440×900]`. There are no graphical thumbnails or attachment strips. Session previews and text copy/yank contain labels rather than image bytes.

## Formats and limits

| Limit | Value |
|---|---|
| Images per prompt | 8 |
| Encoded raster bytes per image | 5 MiB, after base64 decoding |
| Aggregate encoded image bytes | 20 MiB per prompt |
| Width or height | At most 16,384 pixels; nonzero |
| Decoded pixel/work budget | 40 million per image, including animation frame allocations/work |
| Clipboard bitmap encoding | Lossless PNG |
| Embedded ACP formats | PNG, JPEG, WebP, GIF; original bytes and canonical MIME retained |
| Worker feedback / ACP skill prose | Separate 64 KiB text limit; image bytes are not prose |

Zevria never silently resizes, crops, reduces quality, or substitutes an animation's first frame. GIF frames are validated within a bounded work/allocation budget. The pinned decoder APIs do not offer the required pre-frame guarantees for animated WebP/APNG, so those are explicitly rejected. Native clipboard APIs may allocate a decoded bitmap before Zevria can inspect its dimensions; application-side checks cannot prevent that backend allocation.

Image requests prefer exact endpoint input-token counting when configured and available. The local fallback adds **approximately 1,600 tokens per image**, plus text and protocol overhead—not the base64 string length. This is not an upper bound or a model-specific tokenizer. Large images can cost more. Count provenance, capacity failures, unsupported count endpoints, and model/provider errors remain visible.

## ACP and ensembles

Root and worker ACP profiles advertise `promptCapabilities.image: true`. `session/prompt` accepts an ordered array of text and embedded image blocks, including image-only input. Audio and resource blocks are rejected; URLs and resource paths are not resolved. Skill extension **version 1** accepts the same ordered blocks in `args`. Generic prompt text does not invoke skills implicitly. An image-bearing `/implement` cannot authorize implementation.

External workers must advertise image input support. Unsupported initial requests fail explicitly. New image feedback is rejected until capability is negotiated; reconnection checks capability again. Already accepted input is retained if a changed agent cannot execute it. Explicit retry retains failed image bytes even if the worker allocated a session before rejecting the image; it never substitutes a text-only continuation for that failed input. Confirmation, baseline selection, abandonment, and exact-revision rules are unchanged.

Root synthesis carries each original image occurrence once, outside the quoted report JSON. Confirmed workers' successfully incorporated feedback images are associated with their worker and generation. Abandoned, failed, and unincorporated feedback is not synthesis authority. Potential evidence is checked against shared image count/byte limits and the current root context/record budgets before accepting feedback or retry and confirming proposals. Exact counts are preferred; a final complete check precedes sealing. Reports that are not yet published can only contribute known evidence to the earlier projection, so confirmation checks again. Image evidence is never silently truncated to fit report-text budgets.

## Persistence and privacy

**Submitted screenshots persist in plaintext session history as embedded base64.** Root transcripts and associated worker journals are self-contained for resume, recall, retry, and recovery; changing the clipboard later does not alter submitted images. Root and worker records have a 64 MiB bounded read/write ceiling to accommodate image-bearing mirrors and base64 expansion. Skills use dedicated `zevria_skill_invocation` records with ordered arguments; worker/review formats remain version 1. Unsupported owned formats are rejected, not automatically migrated.

Normal image labels and diagnostic copies omit payloads. Known structured image fields, data URLs, and long opaque base64 diagnostic runs are redacted; this is not a general secret scrubber for arbitrary model or tool prose.

Drafts and pending clipboard results are memory-only. There is no temporary image store, upload service, blob garbage collector, encryption, or new cleanup policy. Images follow existing history retention/deletion behavior. Screenshots may contain secrets—review them before submission and protect session directories. Summaries can describe an image but cannot reconstruct its bytes; original durable history remains available for recall.

## Desktop and terminal limitations

Native reading uses pinned `arboard 3.6.1` with image support and Linux Wayland data-control support. X11 and Wayland access depends on the desktop environment, compositor protocols, clipboard owner, and process environment. On Windows clipboard contention may fail locally. Terminals may intercept Ctrl-V; configure them to forward the key to Zevria if needed.

Headless processes and SSH sessions usually cannot access the local terminal client's desktop image clipboard. No remote clipboard forwarding or image fetching is added. Use an ACP client capable of sending embedded images when native desktop access is unavailable.

Automated tests use fake clipboard backends, never the real system clipboard. Live macOS, Windows, Linux/X11, Linux/Wayland, terminal interception, headless/SSH, and vision-endpoint checks require separate manual acceptance; dependency API verification and unit tests do not establish that platform coverage.
