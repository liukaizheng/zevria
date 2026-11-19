#!/usr/bin/env python3
"""Build and verify an unsigned Windows development ZIP, without publishing it."""

import argparse
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import zipfile
import json


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"
EXE = "zevria-desktop.exe"
USAGE = b"Usage: zevria-desktop [WORKSPACE] [--continue|-c]\n"


def build() -> Path:
    result = subprocess.run(
        ["cargo", "build", "--locked", "--release", "-p", "zevria-desktop",
         "--bin", "zevria-desktop", "--target", TARGET,
         "--message-format=json-render-diagnostics"],
        cwd=ROOT, stdout=subprocess.PIPE, encoding="utf-8", check=True,
    )
    executables = []
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if (message.get("reason") == "compiler-artifact"
                and message["target"]["name"] == "zevria-desktop"
                and message["target"]["kind"] == ["bin"]
                and message.get("executable")):
            executables.append(Path(message["executable"]))
    if len(executables) != 1 or executables[0].name != EXE:
        raise RuntimeError(f"Expected one built {EXE}, got {executables}")
    return executables[0]


def verify_pe(executable: Path) -> None:
    with executable.open("rb") as stream:
        dos = stream.read(64)
        if len(dos) != 64 or dos[:2] != b"MZ":
            raise RuntimeError("Missing DOS header in the extracted executable")
        stream.seek(struct.unpack_from("<I", dos, 0x3C)[0])
        header = stream.read(24)
        if len(header) != 24 or header[:4] != b"PE\0\0":
            raise RuntimeError("Missing PE/COFF header in the extracted executable")
        machine = struct.unpack_from("<H", header, 4)[0]
        optional_size, characteristics = struct.unpack_from("<HH", header, 20)
        optional = stream.read(optional_size)
    # IMAGE_FILE_MACHINE_AMD64, executable image (not a DLL), PE32+, GUI subsystem.
    if (machine != 0x8664 or not characteristics & 0x0002 or characteristics & 0x2000
            or len(optional) != optional_size or len(optional) < 70
            or struct.unpack_from("<H", optional)[0] != 0x20B
            or struct.unpack_from("<H", optional, 68)[0] != 2):
        raise RuntimeError("ZIP entry must be an x64 Windows GUI executable")


def smoke_test(executable: Path, isolated_home: Path) -> None:
    isolated_home.mkdir()
    env = {
        "SYSTEMROOT": os.environ["SYSTEMROOT"],
        "HOME": str(isolated_home),
        "USERPROFILE": str(isolated_home),
        "TEMP": str(isolated_home),
        "TMP": str(isolated_home),
        "CAPTURE_TEST": "space = 雪\nnext\r\nline",
    }

    def probe(*args: str, code: int = 0, stdout: bytes | None = b"",
              error: bytes | None = None) -> bytes:
        result = subprocess.run(
            [executable, *args], cwd=executable.parent, env=env,
            stdin=subprocess.DEVNULL, capture_output=True, timeout=20,
        )
        if (result.returncode != code or (stdout is not None and result.stdout != stdout)
                or (error not in result.stderr if error is not None else bool(result.stderr))):
            raise RuntimeError(
                f"Unexpected result for {args!r}: exit {result.returncode}; "
                f"stdout={result.stdout!r}; stderr={result.stderr!r}"
            )
        return result.stdout

    for flag in ("--help", "-h"):
        probe(flag, stdout=USAGE)
    probe("--bogus", code=1, error=b"unknown option: --bogus")
    probe("--", "--help", "extra", code=1, error=b"only one workspace may be opened")
    probe("--__print-env", code=1, error=b"private environment exporter expects one marker")
    marker = "zip-probe"
    output = probe("--__print-env", marker, stdout=None)
    head, *records, tail = output.split(b"\0")
    expected = sorted(f"{key}={value}".encode("utf-8") for key, value in env.items())
    if head != marker.encode("ascii") or tail or sorted(records) != expected:
        raise RuntimeError(f"Private environment export is not byte-exact: {output!r}")
    if any(isolated_home.iterdir()) or list(executable.parent.iterdir()) != [executable]:
        raise RuntimeError("Noninteractive probes wrote config, logs or workspace state")


def package(executable: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="desktop-dev-", dir=destination.parent) as temp:
        sandbox = Path(temp)
        scratch = sandbox / destination.name
        entry = zipfile.ZipInfo(EXE)
        entry.create_system = 3  # Record a portable regular-file mode, not a link or directory.
        entry.external_attr = (stat.S_IFREG | 0o755) << 16
        entry.compress_type = zipfile.ZIP_DEFLATED
        with zipfile.ZipFile(scratch, "w") as archive:
            with executable.open("rb") as source, archive.open(entry, "w") as bundle_file:
                shutil.copyfileobj(source, bundle_file)

        with zipfile.ZipFile(scratch) as archive:
            entries = archive.infolist()
            if (len(entries) != 1 or entries[0].filename != EXE
                    or not stat.S_ISREG(entries[0].external_attr >> 16)):
                raise RuntimeError(f"ZIP must contain exactly one regular file named {EXE}")
            extracted = Path(archive.extract(entries[0], sandbox / "Extracted desktop 雪"))
        verify_pe(extracted)
        smoke_test(extracted, sandbox / "isolated home")
        scratch.replace(destination)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target" / "desktop-dev")
    args = parser.parse_args()
    if sys.platform != "win32":
        parser.error("this package and its smoke tests must run on native Windows")
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    destination = args.output_dir.resolve() / f"zevria-desktop-dev-{TARGET}-{sha}.zip"
    package(build(), destination)
    print(f"Verified development ZIP: {destination}")


if __name__ == "__main__":
    main()
