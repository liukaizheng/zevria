#!/usr/bin/env python3
"""Exercise the workflow's actual bundle and draft asset checks offline."""
import ast
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = (REPO / '.github/workflows/release.yml').read_text(encoding='utf-8')


def step_source(name):
    step = WORKFLOW.split(f'- name: {name}\n', 1)[1]
    return textwrap.dedent(step.split("python - <<'PY'\n", 1)[1].split('\n          PY', 1)[0])


class ReleaseContract(unittest.TestCase):
    def test_embedded_python_is_syntactically_valid(self):
        scripts = re.findall(r"          python - <<'PY'\n(.*?)^          PY$", WORKFLOW, re.S | re.M)
        self.assertGreaterEqual(len(scripts), 5)
        for script in scripts:
            ast.parse(textwrap.dedent(script))

    def test_assembly_accepts_only_three_archives_and_manifest(self):
        source = step_source('Verify exact inventory and assemble SHA256SUMS')
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

    def test_draft_requires_matching_uploaded_bytes_and_exact_inventory(self):
        source = step_source('Verify source and safely create or update only draft assets')
        with tempfile.TemporaryDirectory(prefix='zevria-draft-fixture-') as directory:
            root = Path(directory)
            bundle = root / 'bundle'
            bundle.mkdir()
            payloads = {f'zevria-v1.2.3-{suffix}': b'\x00\xff' + suffix.encode('ascii') for suffix in (
                'x86_64-pc-windows-msvc.zip', 'x86_64-unknown-linux-gnu.tar.gz', 'aarch64-apple-darwin.tar.gz')}
            payloads['SHA256SUMS'] = ''.join(
                f'{hashlib.sha256(payloads[name]).hexdigest()}  {name}\n' for name in sorted(payloads)
            ).encode('ascii')
            uploaded = []
            for index, (name, payload) in enumerate(payloads.items(), start=100):
                (bundle / name).write_bytes(payload)
                uploaded.append(dict(id=index, name=name, state='uploaded', size=len(payload),
                                     digest=f'sha256:{hashlib.sha256(payload).hexdigest()}'))
            sha = '0' * 40
            base = 'repos/example/zevria'
            release = dict(id=99, tag_name='v1.2.3', draft=True, prerelease=False,
                           body=f'<!-- zevria-release-source: {sha} -->', html_url='https://example.invalid/draft')
            responses = {
                f'{base}/git/ref/tags/v1.2.3': dict(object=dict(type='commit', sha=sha)),
                f'{base}/releases?per_page=100': [[release]],
                f'{base}/releases/99': release,
            }
            extra = dict(id=200, name='extra.zip', state='uploaded', size=1)
            summary = root / 'summary'
            env = dict(VERSION='1.2.3', SOURCE_SHA=sha, SOURCE_REF='refs/tags/v1.2.3', EVENT_NAME='push',
                       GH_REPO='example/zevria', TOOLCHAIN='fixture', GITHUB_STEP_SUMMARY=str(summary))

            for case, error in (
                ('verified', None),
                ('same-size corruption', 'Uploaded asset SHA-256 mismatch: SHA256SUMS'),
                ('unexpected on retry', 'Unexpected draft asset inventory; refusing to replace assets'),
                ('unexpected after upload', 'Unexpected uploaded draft asset inventory'),
                ('missing after upload', 'Unexpected uploaded draft asset inventory'),
            ):
                with self.subTest(case=case):
                    summary.unlink(missing_ok=True)
                    retry_calls, downloads = [], []

                    def gh_json(args, *, text):
                        self.assertTrue(text)
                        if args[:3] == ['gh', 'release', 'upload']:
                            self.assertEqual(args[3], 'v1.2.3')
                            retry_calls.append(args)
                            return ''
                        self.assertEqual(args[:2], ['gh', 'api'])
                        if args[2] == f'{base}/releases/99/assets?per_page=100':
                            self.assertEqual(args[3:], ['--paginate', '--slurp'])
                            if not retry_calls:
                                entries = [extra] if case == 'unexpected on retry' else uploaded[:2]
                            elif case == 'unexpected after upload':
                                entries = [*uploaded, extra]
                            elif case == 'missing after upload':
                                entries = uploaded[:-1]
                            else:
                                entries = uploaded
                            return json.dumps([entries[:2], entries[2:]])
                        return json.dumps(responses[args[2]])

                    def gh_download(args, *, stdout, check, timeout):
                        asset_id = int(args[2].rsplit('/', 1)[1])
                        self.assertEqual(args, ['gh', 'api', f'{base}/releases/assets/{asset_id}',
                                                '-H', 'Accept: application/octet-stream'])
                        self.assertTrue(check)
                        name = next(asset['name'] for asset in uploaded if asset['id'] == asset_id)
                        downloads.append(name)
                        payload = payloads[name]
                        stdout.write(b'!' + payload[1:] if case == 'same-size corruption' and name == 'SHA256SUMS'
                                     else payload)

                    with (contextlib.chdir(root), contextlib.redirect_stdout(io.StringIO()), patch.dict(os.environ, env),
                          patch.object(subprocess, 'check_output', side_effect=gh_json),
                          patch.object(subprocess, 'run', side_effect=gh_download)):
                        if error:
                            with self.assertRaises(SystemExit) as caught:
                                exec(source, {})
                            self.assertEqual(str(caught.exception), error)
                            self.assertFalse(summary.exists())
                        else:
                            exec(source, {})
                            self.assertCountEqual(downloads, payloads.keys())
                            receipt = summary.read_text(encoding='utf-8')
                            for asset in uploaded:
                                self.assertIn(f'{asset["digest"][7:]}  {asset["name"]} (asset ID {asset["id"]}', receipt)
                    self.assertEqual(len(retry_calls), 0 if case == 'unexpected on retry' else 1)
                    if error and case != 'same-size corruption':
                        self.assertEqual(downloads, [])


if __name__ == '__main__':
    unittest.main(verbosity=2)
