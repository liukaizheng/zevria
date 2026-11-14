---
name: commit
description: Create a detailed Git commit with a Conventional Commit subject
---

Inspect `git status`, `git diff`, and `git diff --cached` before changing anything. Preserve unrelated user work: do not reset, clean, amend, rebase, or overwrite changes that are not part of the requested commit. If the working tree contains unrelated changes or the intended scope is unclear, separate the relevant files or ask for clarification rather than mixing work silently.

Use one focused commit when the changes are coherent. Stage only the intended files or hunks, then review the staged diff.

The commit subject must use exactly this format:

```text
<type>(<scope>): <subject>
```

The scope is optional, so this form is also valid:

```text
<type>: <subject>
```

`<type>` must be exactly one of these lowercase values:

- `feat` — introduce user-visible functionality
- `fix` — correct a defect
- `docs` — change documentation only
- `style` — make non-functional formatting or style changes
- `refactor` — restructure code without changing behavior
- `test` — add or change tests without changing production behavior
- `chore` — maintenance, tooling, or other repository work

Use a concise, imperative `<subject>` that starts immediately after `: ` and has no trailing period. Do not add prefixes, ticket identifiers, or extra lines to the subject.

Write a detailed commit body separated from the subject by a blank line. Explain the important implementation or documentation changes and the motivation or user-visible effect. Use bullets when they make the summary easier to scan.

Create the commit with the subject and detailed body, for example:

```sh
git commit \
  -m "<type>(<scope>): <subject>" \
  -m "Describe the main change." \
  -m "Explain why it was needed."
```

After committing, verify the new commit with `git log -1 --format=fuller` and `git show --stat --oneline HEAD`, then check `git status --short`. Do not push unless explicitly requested. If there are no intended changes, secrets, or unresolved ambiguity, do not create a commit; explain the issue instead.
