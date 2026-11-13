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

paths = [root / "crates/provider/src" / name for name in [
    "lib.rs", "connection.rs", "websocket_session.rs", "tests.rs", "router.rs", "resume_wire_tests.rs",
]] + sorted((root / "crates/provider/src/responses").glob("*.rs")) + [root / "crates/host/src/runtime.rs"] + sorted((root / "crates/host/src/runtime").glob("*.rs"))
for path in paths:
    text = path.read_text().replace('''// Temporary test-only preparation probe; included in both feature configurations.
#[cfg(test)]
#[path = "../cache_diagnostics/probe_support.rs"]
pub(crate) mod cache_preparation_probe_support;
''', '')
    text = remove_hooks(text)
    # Unwrap the identifier helper's tail expression before removing standalone
    # calls: it has no statement terminator and remains a harmless identity map.
    text = text.replace('Diagnostics::identifier(value)', 'value')
    text = text.replace('// Diagnostic builds also bound existing identifier log fields. The actual\n// headers, IDs, errors, and continuation values remain untouched.\n', '')
    # The facade owns all runtime instrumentation. Remove its import and each
    # standalone field/initializer/call, using the same balanced-token removal
    # as cfg hooks. The two shadowing dispatch/configure lets need no substitute:
    # the original prepared request/runtime remains in scope.
    text = text.replace('use crate::diagnostics::Diagnostics;\n', '').replace('mod diagnostics;\n', '')
    text = text.replace('pub(crate) use prepare::PreparedRequest;\n', '')
    text = text.replace('use continuation::{full_transmission, select_transmission};', 'use continuation::select_transmission;')
    text = re.sub(r'^ *diagnostics: Default::default\(\),\n', '', text, flags=re.M)
    text = re.sub(
        r'^( *[^\n]*(?:Diagnostics::|cache_diagnostics|diagnostics: crate::diagnostics|prior_terminal_id: crate::diagnostics)[^\n]*)$',
        '#[cfg(feature = "cache-diagnostics")]\n\\1', text, flags=re.M,
    )
    text = remove_hooks(text)
    # A fieldless Transmission no longer has a full-request reason. Those
    # initializers are already removed with their Diagnostics calls above.
    text = re.sub(r'^ *pub\(super\) full_reason: crate::diagnostics::TransmissionDiagnostics,\n', '', text, flags=re.M)
    text = re.sub(r'^ *if is_close \{\n *\}\n', '', text, flags=re.M)
    text = text.replace('#[path = "cache_diagnostics/probe.rs"]\nmod cache_preparation_probe;\n', '')
    text = text.replace('''// Temporary test-only preparation probe; included in both feature configurations.
#[cfg(test)]
#[path = "../cache_diagnostics/probe_support.rs"]
pub(crate) mod cache_preparation_probe_support;
''', '')
    text = re.sub(r'\n{3,}', '\n\n', text)
    path.write_text(text.rstrip() + "\n")
(root / "crates/provider/src/diagnostics.rs").unlink()

removals = {
    "crates/provider/Cargo.toml": '# Temporary, opt-in instrumentation. See docs/cache-diagnostics.md for removal.\n[features]\ncache-diagnostics = ["dep:libc"]\n\n',
    "crates/host/Cargo.toml": '# Temporary; never enable by default.\ncache-diagnostics = ["zevria-provider/cache-diagnostics"]\n',
    "crates/zevria/Cargo.toml": '# Temporary; release profile alone does not disable explicit feature activation.\n[features]\ncache-diagnostics = ["zevria-host/cache-diagnostics"]\n\n',
}
for name, removed in removals.items():
    path = root / name
    text = path.read_text()
    assert text.count(removed) == 1, name
    path.write_text(text.replace(removed, ""))

manifest = root / "crates/provider/Cargo.toml"
text = manifest.read_text()
for dependency in ['libc']:
    text = text.replace(f'{dependency} = {{ workspace = true, optional = true }}\n', '')
manifest.write_text(text)
lock = root / "Cargo.lock"
text = lock.read_text()
start = text.index('name = "zevria-provider"\n')
end = text.index('\n]\n', start) + 3
provider = text[start:end].replace(' "libc",\n', '')
lock.write_text(text[:start] + provider + text[end:])
(root / "crates/host/src/cache_diagnostics_runtime_tests.rs").unlink()
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
# Historical performance/release reports retain their original commands and
# measurements. Only source/manifests must be free of feature/module references;
# the live interoperability document loses its marked observer paragraph above.
for path in (root / "crates").rglob("*"):
    if path.is_file() and path.suffix in {".rs", ".toml"}:
        assert not re.search(r"cache[-_]diagnostic|cache_preparation_probe|crate::diagnostics", path.read_text()), path
print("Removed all listed temporary assets and hooks; permanent regressions remain.")
