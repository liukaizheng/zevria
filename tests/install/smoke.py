#!/usr/bin/env python3
"""Install a freshly packaged native release through production installer logic.
Usage: smoke.py ARCHIVE SIDECAR VERSION
No network, registry writes, profiles, real user homes, or previously released binary.
"""
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[2]


def run(command, env, cwd, expected=None):
    result = subprocess.run(command, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                            text=True, capture_output=True, timeout=60)
    if result.returncode != 0 or (expected is not None and result.stdout != expected):
        raise SystemExit(f"{command!r}: status={result.returncode}, stdout={result.stdout!r}, stderr={result.stderr!r}")
    return result.stdout


def main():
    archive, sidecar = (Path(path).resolve(strict=True) for path in sys.argv[1:3])
    version = sys.argv[3]
    # The job's sidecar becomes the correctly named release manifest in transport.
    expected = f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}"
    if sidecar.read_text().strip() != expected:
        raise SystemExit("Fresh-package checksum sidecar does not match archive")
    with tempfile.TemporaryDirectory(prefix="zevria-install-smoke-") as temp:
        base = Path(temp).resolve()
        home, root, workspace = base / "home", base / "custom root", base / "workspace"
        home.mkdir()
        workspace.mkdir()
        manifest = base / "SHA256SUMS"
        manifest.write_text(sidecar.read_text())
        env = {key: value for key, value in os.environ.items()
               if not key.upper().startswith(('ZEVRIA_', '_ZEVRIA_'))
               and key.upper() not in ('HOME', 'USERPROFILE', 'BASH_ENV', 'ENV', 'FIXTURE_LATEST')}
        env.update(HOME=str(home), USERPROFILE=str(home), ZEVRIA_INSTALL=str(root),
                   FIXTURE_VERSION=version, FIXTURE_ASSET=archive.name, FIXTURE_ARCHIVE=str(archive),
                   FIXTURE_MANIFEST=str(manifest), FIXTURE_CALLS=str(base / "calls"))
        if os.name == "nt":
            # Both installed Windows editions must accept the just-packaged binary.
            for shell in ("powershell.exe", "pwsh.exe"):
                run([shell, "-NoProfile", "-NonInteractive", "-File", str(REPO / "tests/install/smoke_windows.ps1")], env, workspace)
            executable = root / "bin/zevria.exe"
        else:
            script = f'''source {shlex.quote(str(REPO / 'install.sh'))}
source {shlex.quote(str(REPO / 'tests/install/transport.sh'))}
zevria_main "$FIXTURE_VERSION" --no-path-update
'''
            run(["/bin/bash", "-c", script], env, workspace)
            executable = root / "bin/zevria"
        run([str(executable), "--version"], env, workspace, f"zevria {version}\n")
        run([str(executable), "--help"], env, workspace)
        json.loads(run([str(executable), "--runtime", "native", "skills", "list", "--json"], env, workspace))
        if (home / ".zevria/config.toml").exists() or (workspace / ".zevria").exists():
            raise SystemExit("Installer smoke unexpectedly created application configuration/state")
        print(f"Fresh-package installer smoke passed: {archive.name}")


if __name__ == "__main__":
    main()
