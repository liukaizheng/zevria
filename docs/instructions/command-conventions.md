RTK is installed. Always prefix shell commands with `rtk` so it can filter and compress command output before that output enters the model context. A command that already starts with `rtk` is already routed and must not be double-prefixed.

Commands run in the runtime selected at startup. Native Windows uses noninteractive Git Bash, never PowerShell, regardless of the parent terminal. Prefer Bash-relative paths; use explicit `rtk proxy cygpath -u 'C:\path with spaces'` conversion when a shell command needs an absolute Bash path. Structured file-tool arguments remain native Windows paths. WSL runs the entire application with Linux paths. Command strings are not automatically rewritten or sent across runtimes.

For example:

```bash
rtk rg "pattern" src
rtk git status
rtk cargo test
rtk read file.rs -l aggressive
rtk sed -n '120,220p' src/main.rs
```

Prefer one focused RTK invocation per command. Use `rtk proxy <command>` only when RTK cannot execute a command correctly or complete, unfiltered output is necessary for correctness; keep raw output bounded. Use the command tool's working-directory option instead of a shell `cd` when one is available.

For an initial structural view of a code file, prefer `rtk read file.rs -l aggressive`; it strips bodies and keeps signatures so reconnaissance uses less context. Treat an aggressive read as an outline, not as sufficient evidence about implementation behavior. Before editing a file or reasoning about its implementation details, inspect the complete relevant bodies with `rtk read file.rs` or a bounded command such as `rtk sed -n '120,220p' file.rs`.

When inspecting the workspace with shell commands, prefer these tools whenever available:

* Prefer `sed` to read files, selecting only the relevant line ranges when practical.
* Prefer `rg` over `grep` for text and content searches.
* Prefer `rg --files` over `fd` or `find` for file discovery.

Use another tool when a preferred command is unavailable or the task requires behavior it does not provide.

Keep terminal output bounded and relevant. Recursive searches and filesystem scans can easily produce far more output than is useful, so avoid unbounded commands by default.

* Narrow queries at the source whenever practical: restrict the path, pattern, file type, depth, glob, or other search criteria before relying on downstream truncation.
* When only a sample is needed, limit displayed results with `head`, `tail`, `sed`, or an equivalent mechanism.
* Prefer native limiting or filtering options when the underlying command provides them.
* For exploratory `rg --files`, `rg`, recursive listings, and similar commands, start with a small representative result set rather than dumping hundreds or thousands of lines into the context.
* Remember that `rg` uses regular expressions and honors ignore rules by default. Use `-F` for literal searches and add `--hidden` or `--no-ignore` only when the task requires those files.
* When checking whether something exists, what its naming looks like, or where it is located, return only enough output to answer that question.
* If both the beginning and end of a large result are useful, inspect them separately instead of printing the entire result.
* Expand the result set only when the current output is insufficient to continue the investigation.
* Do not truncate when the task genuinely requires the complete result set or when doing so could hide information necessary for correctness.

For example, prefer commands such as:

```bash
rtk rg --files src
rtk rg -m 20 "pattern" src
rtk sed -n '120,220p' src/main.rs
```

over equivalent commands that emit an unbounded recursive result set.

The goal is not to search less thoroughly, but to keep each individual command focused and its output small enough to reason about effectively.

### Command calls and batching

Use the callable tool definitions advertised for the current request, including definitions supplied outside prose system guidance. Follow their exact invocation names and arguments; the workflow's registered tool names determine what is allowed. An advertised, workflow-allowed `command` tool needs no separate permission or tool-version probe merely because prose guidance omits it. Honor Plan/Explore restrictions, actual errors, and task-relevant version checks.

For independent reads and searches, submit separate `command` calls together in one assistant response, with one focused RTK invocation per call. Native Zevria needs no generic parallel wrapper: call `command` directly instead of inventing an unadvertised wrapper or inferring one from model-specific habits. Do not combine unrelated reads into a shell script merely to avoid batching uncertainty.

Batching reduces model round trips; it does not promise wall-clock overlap. Ordinary calls execute sequentially in assistant-call order. Wait for earlier results when later commands depend on them. Skill-only responses, subtask response-shape requirements, and Plan submission boundaries still apply.
