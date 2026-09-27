# The `edit` Tool

The `edit` tool applies precise changes to one existing UTF-8 text file in the
workspace. It is the preferred tool for modifying source code, configuration,
and documentation when the intended change can be described as replacing
known text. It can also rename or move the edited file.

Unlike a shell-based editing command, `edit` receives structured arguments and
returns structured file-change metadata. Literal replacements are validated
before the file is changed, and the workspace boundary is checked for both the
source and destination paths.

## When to use `edit`

Use `edit` when:

- the target file already exists;
- the file is valid UTF-8 text;
- the change is localized and can be identified by an exact string; or
- an existing file needs to be renamed, optionally while its contents are
  updated.

Use the other file tools for different operations:

- `write` creates a new file or intentionally rewrites a complete existing
  file;
- `delete` removes an existing file; and
- `command` runs a non-interactive shell command when a file operation cannot
  be expressed as a structured edit.

For a small source change, prefer a replacement with enough surrounding context
to identify exactly the intended code rather than replacing a very common token.

## Call shape

```json
{
  "file_path": "src/lib.rs",
  "replacements": [
    {
      "old_string": "const LIMIT: usize = 10;",
      "new_string": "const LIMIT: usize = 20;",
      "replace_all": false
    }
  ],
  "move_to": "optional/new/path.rs"
}
```

The top-level object has three fields:

### `file_path`

`file_path` is required. It may be an absolute path or a path relative to the
startup workspace. The resolved path must remain inside the workspace and must
refer to an existing file, not a directory.

Parent directories must already exist. The tool resolves the workspace and the
path's parent directory, so paths that escape through `..`, an outside absolute
path, or a symlink that points outside the workspace are rejected. Leading or
trailing whitespace in the path is also invalid.

### `replacements`

`replacements` is an optional array of replacement objects. It defaults to an
empty array. At least one replacement or a real `move_to` operation is required.
Each object has the following fields:

- `old_string` — required literal text to find. It is not a regular expression,
  shell expression, or `sed` script. It may span multiple lines, but it must not
  be empty.
- `new_string` — required text to insert. It is inserted exactly as supplied;
  an empty string deletes the matched text.
- `replace_all` — optional boolean that defaults to `false`. When false, the
  old text must occur exactly once. When true, every occurrence is replaced.

`old_string` and `new_string` must differ. A replacement whose two strings are
identical is rejected, even if another replacement in the same request would
make a change.

### `move_to`

`move_to` is optional. When supplied, it is resolved using the same workspace
rules as `file_path`. The destination must not already exist, and its parent
directory must already exist. A different destination performs a rename after
any content replacement has been written.

To rename without changing content, provide an empty `replacements` array and a
different `move_to` value:

```json
{
  "file_path": "docs/old-name.md",
  "replacements": [],
  "move_to": "docs/new-name.md"
}
```

Passing the same path in `move_to` is not a move. It does not satisfy the
requirement for a change by itself, so it must be accompanied by a replacement
that changes the content.

## How replacements are applied

The tool reads the complete file into memory and applies replacements in array
order. Every later replacement sees the content produced by earlier ones.
Replacements are literal and case-sensitive.

For example, this request first changes the heading and then changes every
remaining occurrence of `Old project`:

```json
{
  "file_path": "README.md",
  "replacements": [
    {
      "old_string": "# Old project name",
      "new_string": "# New project name"
    },
    {
      "old_string": "Old project",
      "new_string": "New project",
      "replace_all": true
    }
  ]
}
```

A later replacement can therefore refer to text introduced by an earlier one:

```json
{
  "file_path": "config.toml",
  "replacements": [
    {
      "old_string": "port = 8000",
      "new_string": "port = 9000"
    },
    {
      "old_string": "9000",
      "new_string": "9443"
    }
  ]
}
```

### Choosing a safe `old_string`

With the default `replace_all: false`, the tool requires exactly one match.
This prevents an edit from silently changing the wrong occurrence. If the text
is found more than once, the request fails with an ambiguity error. Add nearby
text such as a function signature, heading, or configuration key to make the
match unique:

```json
{
  "file_path": "src/server.rs",
  "replacements": [
    {
      "old_string": "fn start() {\n    let port = 8000;\n}",
      "new_string": "fn start() {\n    let port = 9000;\n}"
    }
  ]
}
```

Use `replace_all: true` only when every matching occurrence is intentionally
part of the change. It is useful for a deliberate global rename or a repeated
formatting change, but it can affect comments, strings, and unrelated sections
as well as code.

A replacement fails when:

- `old_string` is empty;
- `old_string` is not found;
- `replace_all` is false and more than one match exists; or
- `old_string` and `new_string` are identical.

After all replacements, the overall file must differ from the original unless
the operation is a real move. A sequence that changes text and then changes it
back is consequently rejected as a no-op.

## Validation and write behavior

The operation has a preparation phase and an apply phase.

During preparation, the tool:

1. validates `file_path` and `move_to` syntax;
2. resolves both paths inside the workspace;
3. verifies that the source exists and is not a directory;
4. reads the source as UTF-8;
5. applies and validates every replacement in memory;
6. checks that a content change or real move exists; and
7. verifies that the move destination does not already exist.

No file is written while these checks are running. If any replacement fails,
the source remains unchanged, including when an earlier replacement in the same
request was valid but a later replacement was not.

During the apply phase, a content-only edit is fully written and synced to a
unique sibling temporary file, then atomically renamed over the source. The
existing file permissions are retained. A rename-only operation remains one
filesystem rename.

For a combined content edit and move, the final content is staged and synced
beside the destination before the source is renamed there. The staged content
is then atomically installed over that moved original. If the final install
fails, the tool attempts to move the original file back to its source path.
The tool does not reformat the file: existing line endings and a missing final
newline are preserved because replacements operate directly on the original
string.

No sequence of filesystem operations can guarantee rollback after every
external race or I/O failure. If both the final install and the rollback fail,
the tool reports a `partial` outcome and attaches metadata describing the file
state that actually survived. Inspect that source/destination state before
retrying; do not assume an error means no mutation occurred.

## Examples

### Replace one unique block

```json
{
  "file_path": "src/lib.rs",
  "replacements": [
    {
      "old_string": "pub fn enabled() -> bool {\n    false\n}",
      "new_string": "pub fn enabled() -> bool {\n    true\n}"
    }
  ]
}
```

### Delete a line or phrase

Set `new_string` to an empty string:

```json
{
  "file_path": "notes.txt",
  "replacements": [
    {
      "old_string": "obsolete note\n",
      "new_string": ""
    }
  ]
}
```

### Apply several independent edits

```json
{
  "file_path": "docs/guide.md",
  "replacements": [
    {
      "old_string": "Status: draft",
      "new_string": "Status: published"
    },
    {
      "old_string": "TODO: add examples",
      "new_string": "Examples are included below."
    }
  ]
}
```

Each `old_string` is matched against the result of the previous replacement,
not against the original file.

### Edit and rename in one call

```json
{
  "file_path": "src/old_name.rs",
  "replacements": [
    {
      "old_string": "struct OldType",
      "new_string": "struct NewType"
    }
  ],
  "move_to": "src/new_name.rs"
}
```

## Results and file-change metadata

A successful ordinary tool call returns:

```text
applied edits (1 file changed)
```

A structured call also returns a `FileChanges` extension for the frontend. For
content edits, the extension normally contains a unified diff. For a rename,
it records the destination path; for a rename-only operation, the diff can be
empty because the content did not change.

For `edit`, very large diffs are represented by summary metadata instead of an
inline diff. The summary records the update operation, added and removed line
counts, the diff size, and the reason it was omitted. The file operation itself
is still performed; only the display metadata is abbreviated. This omission
and the TUI's 1,000 rendered-row limit are deliberately `edit` policies. They
do not apply to newly captured readable UTF-8 `write` or `delete` changes,
which retain and render their complete text (or one full-context overwrite
patch).

## Safety checklist

Before calling `edit`:

1. Read the relevant file and choose an `old_string` with enough context.
2. Confirm whether one match or every match should be changed.
3. Keep replacements ordered when a later edit depends on an earlier one.
4. Use `write` instead if creating a file or replacing the whole file is clearer.
5. After a rename, update references to the old path separately if needed.

The tool rejects directories, non-UTF-8 or binary files, missing parent
directories, paths outside the workspace, existing move destinations, empty or
whitespace-padded paths, and no-op edits. All successful changes remain within
the workspace and produce a change record for review.
