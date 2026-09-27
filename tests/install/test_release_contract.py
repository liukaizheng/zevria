#!/usr/bin/env python3
"""Exercise the workflow's actual four-asset assembly code, without publishing."""
import ast
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = (REPO / '.github/workflows/release.yml').read_text()


class ReleaseContract(unittest.TestCase):
    def test_embedded_python_is_syntactically_valid(self):
        scripts = re.findall(r"          python - <<'PY'\n(.*?)^          PY$", WORKFLOW, re.S | re.M)
        self.assertGreaterEqual(len(scripts), 5)
        for script in scripts:
            ast.parse(textwrap.dedent(script))

    def test_assembly_accepts_only_three_archives_and_manifest(self):
        step = WORKFLOW.split('- name: Verify exact inventory and assemble SHA256SUMS', 1)[1]
        source = textwrap.dedent(step.split("python - <<'PY'\n", 1)[1].split('\n          PY', 1)[0])
        with tempfile.TemporaryDirectory(prefix='zevria-bundle-fixture-') as directory:
            root = Path(directory)
            names = []
            for target, extension in (
                ('x86_64-pc-windows-msvc', 'zip'),
                ('x86_64-unknown-linux-gnu', 'tar.gz'),
                ('aarch64-apple-darwin', 'tar.gz'),
            ):
                artifact = root / 'incoming' / ('release-' + target)
                artifact.mkdir(parents=True)
                name = f'zevria-v1.2.3-{target}.{extension}'
                names.append(name)
                # Only assembly/inventory is tested here; native smoke tests use
                # real packages, and never substitute these bytes for executables.
                payload = target.encode()
                (artifact / name).write_bytes(payload)
                (artifact / (name + '.sha256')).write_bytes(
                    f'{hashlib.sha256(payload).hexdigest()}  {name}\n'.encode('ascii')
                )
            env = dict(os.environ, VERSION='1.2.3', SOURCE_SHA='0' * 40, TOOLCHAIN='fixture',
                       GITHUB_STEP_SUMMARY=str(root / 'summary'))
            result = subprocess.run([sys.executable, '-c', source], cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual({p.name for p in (root / 'bundle').iterdir()}, set(names) | {'SHA256SUMS'})
            self.assertEqual(len((root / 'bundle/SHA256SUMS').read_text().splitlines()), 3)
            for path in (root / 'bundle').iterdir():
                path.unlink()
            (root / 'bundle').rmdir()
            (artifact / 'install.sh').write_text('not a release asset')
            result = subprocess.run([sys.executable, '-c', source], cwd=root, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Unexpected artifact inventory', result.stderr)


if __name__ == '__main__':
    unittest.main(verbosity=2)
