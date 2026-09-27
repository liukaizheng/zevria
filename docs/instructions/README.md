# Embedded instruction documents

Every other file in this folder is compiled verbatim with `include_str!`:

- Instruction modules come from `crates/instructions/src/prompts.rs`.
- Context-checkpoint templates come from `crates/model/src/compaction.rs`.

Human-facing documentation belongs in `docs/`.

Any byte change, including whitespace and trailing newlines, changes the cacheable
prompt prefix. Edit these files only when you intend to change model behavior.

The rendered bytes are pinned by the instruction fixtures in `crates/instructions`,
`crates/core`, and `crates/provider`. Regenerate them with
`UPDATE_INSTRUCTION_FIXTURES=1` only after reviewing the change.

`crates/zevria/tests/instruction_documents.rs` enforces this folder's contents.
