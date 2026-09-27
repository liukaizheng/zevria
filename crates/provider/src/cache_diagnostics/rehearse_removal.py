#!/usr/bin/env python3
"""Remove temporary instrumentation ONLY from an explicitly marked disposable copy.
Run this script from the original tree, passing an independent source-copy root.
It deliberately leaves permanent tests, ordinary fixtures, and sha2 untouched.
"""
from pathlib import Path
import re
import shutil
import sys

root = Path(sys.argv[1]).resolve()
original = Path(__file__).resolve().parents[4]
assert root != original and root not in original.parents and original not in root.parents
assert (root / ".cache-diagnostics-disposable").is_file(), "missing disposable-copy marker"
assert not (root / ".git").exists(), "refusing to modify a Git worktree"

# Hooks are standalone cfg attributes followed by one field, argument,
# statement, item, or if block. Skip quoted strings when balancing Rust tokens.
pattern = re.compile(r'^ *#\[cfg\((?!not\().*feature = "cache-diagnostics".*\)\]\n', re.M)
def remove_hooks(text):
    while match := pattern.search(text):
        start, pos = match.span()
        depth = 0
        quoted = False
        escaped = False
        saw_block = False
        while pos < len(text):
            char = text[pos]
            if quoted:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char in "([{":
                depth += 1
                saw_block |= char == "{"
            elif char in ")]}":
                depth -= 1
                if char == "}" and depth == 0 and saw_block:
                    pos += 1
                    break
            elif char in ",;" and depth == 0:
                pos += 1
                break
            pos += 1
        assert depth == 0 and pos < len(text), "unrecognized hook; inspect manually"
        if text[pos:pos + 1] == "\n":
            pos += 1
        text = text[:start] + text[pos:]
    # Retain the original HTTP .send() and no-op ID formatter, unconditionally.
    text = re.sub(r'^ *#\[cfg\(not\(feature = "cache-diagnostics"\)\)\]\n', '', text, flags=re.M)
    # A cfg-gated `let x = { ... };` leaves its statement terminator behind.
    return re.sub(r'^ *;\n', '', text, flags=re.M)

paths = ["crates/provider/src/" + name for name in [
    "lib.rs", "connection.rs", "turn.rs", "websocket_session.rs", "tests.rs", "router.rs", "resume_wire_tests.rs",
]] + ["crates/app/src/runtime.rs"]
for name in paths:
    path = root / name
    text = remove_hooks(path.read_text())
    if name.endswith('/connection.rs'):
        text = text.replace('// Diagnostic builds also bound existing identifier log fields. The actual\n// headers, IDs, errors, and continuation values remain untouched.\npub(crate) fn log_identifier(value: &str) -> &str {\n    {\n        value\n    }\n}\n', '')
    if name.endswith(('/connection.rs', '/turn.rs')):
        text = text.replace('.map(log_identifier)', '').replace('log_identifier, ', '')
    text = re.sub(r'\n{3,}', '\n\n', text)
    text = text.replace('#[path = "cache_diagnostics/probe.rs"]\nmod cache_preparation_probe;\n', '')
    text = text.replace('// Temporary test-only preparation probe; included in both feature configurations.\n#[cfg(test)]\n#[path = "cache_diagnostics/probe_support.rs"]\npub(crate) mod cache_preparation_probe_support;\n', '')
    path.write_text(text.rstrip() + "\n")

removals = {
    "crates/provider/Cargo.toml": '# Temporary, opt-in instrumentation. See docs/cache-diagnostics.md for removal.\n[features]\ncache-diagnostics = ["dep:libc", "dep:uuid"]\n\n',
    "crates/app/Cargo.toml": '# Temporary; never enable by default.\ncache-diagnostics = ["zevria-provider/cache-diagnostics"]\n',
    "crates/zevria/Cargo.toml": '# Temporary; release profile alone does not disable explicit feature activation.\n[features]\ncache-diagnostics = ["zevria-app/cache-diagnostics"]\n\n',
}
for name, removed in removals.items():
    path = root / name
    text = path.read_text()
    assert text.count(removed) == 1, name
    path.write_text(text.replace(removed, ""))

manifest = root / "crates/provider/Cargo.toml"
text = manifest.read_text()
for dependency in ['libc', 'uuid']:
    text = text.replace(f'{dependency} = {{ workspace = true, optional = true }}\n', '')
manifest.write_text(text)
lock = root / "Cargo.lock"
text = lock.read_text()
start = text.index('name = "zevria-provider"\n')
end = text.index('\n]\n', start) + 3
provider = text[start:end].replace(' "libc",\n', '').replace(' "uuid",\n', '')
lock.write_text(text[:start] + provider + text[end:])
(root / "crates/app/src/cache_diagnostics_runtime_tests.rs").unlink()
shutil.rmtree(root / "crates/provider/src/cache_diagnostics")
(root / "docs/cache-diagnostics.md").unlink()
compatible_doc = root / "docs/responses-compatible.md"
text = compatible_doc.read_text()
text, count = re.subn(r'<!-- BEGIN TEMPORARY CACHE OBSERVER -->\n.*?<!-- END TEMPORARY CACHE OBSERVER -->\n\n', '', text, flags=re.S)
assert count == 1, "missing or duplicated temporary observer documentation"
compatible_doc.write_text(text)
# Explicit comparison request interoperability is intentionally feature-independent.
# Its ordinary wire/maintenance regressions must survive observer removal.
for name in ["prompt_cache.rs", "prompt_cache_tests.rs"]:
    assert (root / "crates/provider/src" / name).is_file(), name
for directory in ["crates", "docs"]:
    for path in (root / directory).rglob("*"):
        if path.is_file() and path.suffix in {".rs", ".toml", ".md"}:
            assert not re.search(r"cache[-_]diagnostic|cache_preparation_probe", path.read_text()), path
print("Removed all listed temporary assets and hooks; permanent regressions remain.")
