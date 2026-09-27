#!/usr/bin/env python3
"""Offline, disposable installer fixtures. Run with macOS /bin/bash (3.2) or Bash 4+.
Only transport/platform helpers are injected; production verification always runs.
"""
import hashlib
import io
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tarfile
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
BASH = os.environ.get("INSTALL_TEST_BASH", "/bin/bash")
SOURCE = f'source {shlex.quote(str(REPO / "install.sh"))}\n'
TRANSPORT = f'source {shlex.quote(str(REPO / "tests/install/transport.sh"))}\n'


class Installer(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zevria-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.home = self.base / "home"
        self.home.mkdir()
        self.root = self.base / "root 雪 ';& $(touch INJECTED)"
        self.env = dict(os.environ, HOME=str(self.home), SHELL="/bin/bash",
                        TMPDIR=str(self.base), ZEVRIA_INSTALL=str(self.root),
                        FIXTURE_CALLS=str(self.base / "calls"))
        for key in ("BASH_ENV", "ENV", "ZDOTDIR", "XDG_CONFIG_HOME", "TAR_OPTIONS"):
            self.env.pop(key, None)
        self.archive()

    def archive(self, version="1.2.3", entries=None, binary_version=None, fmt=tarfile.PAX_FORMAT):
        self.env.update(FIXTURE_VERSION=version,
                        FIXTURE_ASSET=f"zevria-v{version}-x86_64-unknown-linux-gnu.tar.gz",
                        FIXTURE_ARCHIVE=str(self.base / "fixture.tar.gz"),
                        FIXTURE_MANIFEST=str(self.base / "SHA256SUMS"))
        body = f"#!/bin/sh\nprintf '%s\\n' {shlex.quote('zevria ' + (binary_version or version))}\n".encode()
        entries = entries if entries is not None else [("zevria", tarfile.REGTYPE, body)]
        with tarfile.open(self.env["FIXTURE_ARCHIVE"], "w:gz", format=fmt) as archive:
            for name, kind, data in entries:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.mode = 0o755
                info.mtime = 1.125  # Exercise workflow-style PAX metadata.
                if kind == tarfile.REGTYPE:
                    info.size = len(data)
                if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
                    info.linkname = "/bin/sh"
                archive.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)
        self.manifest()

    def manifest(self):
        self.digest = hashlib.sha256(Path(self.env["FIXTURE_ARCHIVE"]).read_bytes()).hexdigest()
        self.line = f"{self.digest}  {self.env['FIXTURE_ASSET']}\n"
        Path(self.env["FIXTURE_MANIFEST"]).write_text(self.line)

    def run_shell(self, code, args=(), expected=0):
        result = subprocess.run([BASH, "-c", SOURCE + code, "fixture", *args], env=self.env,
                                cwd=self.home, text=True, capture_output=True, timeout=30)
        if expected == 0:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def install(self, args=(), hooks="", expected=0):
        return self.run_shell(TRANSPORT + '\nzevria_target() { printf x86_64-unknown-linux-gnu; }\n'
                              + hooks + '\nzevria_main "$@"', args, expected)

    def test_help_and_invalid_arguments_are_side_effect_free(self):
        self.run_shell('zevria_main --help', expected=0)
        for args in (("--what",), ("--help", "--help"), ("1.2.3", "latest"),
                     ("--no-path-update", "--no-path-update"), ("",), ("1.2",),
                     ("v01.2.3",), ("1.2.3-01",), ("1.2.3-alpha.01",), ("1.2.3\n",), ("1.2.3+x/y",), ("LATEST",)):
            with self.subTest(args=args):
                self.install(args, expected=1)
        self.assertEqual(list(self.home.iterdir()), [])
        self.assertFalse(self.root.exists())
        self.assertFalse((self.base / "calls").exists())

    def test_supported_targets_and_rosetta(self):
        for osname, arch, libc, arm, target in (
            ("Linux", "x86_64", "glibc 2.35", "0", "x86_64-unknown-linux-gnu"),
            ("Darwin", "arm64", "", "1", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "", "1", "aarch64-apple-darwin"),
        ):
            code = f'''uname() {{ if [[ $1 == -s ]]; then echo {osname}; else echo {arch}; fi; }}
getconf() {{ echo {shlex.quote(libc)}; }}
sysctl() {{ echo {arm}; }}
zevria_target'''
            self.assertEqual(self.run_shell(code).stdout, target)

    def test_unsupported_targets(self):
        for osname, arch, libc in (("Linux", "aarch64", "glibc 2.35"), ("Linux", "x86_64", "musl"),
                                   ("Darwin", "x86_64", ""), ("FreeBSD", "x86_64", ""),
                                   ("MINGW64_NT", "x86_64", ""), ("CYGWIN_NT", "x86_64", "")):
            result = self.run_shell(f'''uname() {{ if [[ $1 == -s ]]; then echo {osname}; else echo {arch}; fi; }}
getconf() {{ echo {shlex.quote(libc)}; }}
sysctl() {{ echo 0; }}
zevria_target''', expected=1)
            if osname.startswith(("MINGW", "CYGWIN")):
                self.assertIn("PowerShell", result.stderr)

    def test_missing_utility(self):
        self.install(hooks='command() { if [[ $* == "-v curl" ]]; then return 1; fi; builtin command "$@"; }', expected=1)
        self.assertFalse(self.root.exists())

    def test_latest_is_resolved_once_and_pinned_urls_used(self):
        self.install(("--no-path-update",))
        calls = (self.base / "calls").read_text().splitlines()
        self.assertEqual(calls.count("latest"), 1)
        self.assertEqual(len(calls), 3)
        self.assertTrue(all("/download/v1.2.3/" in url for url in calls[1:]))
        self.assertEqual((self.home / ".zevria/install-root").read_text(), str(self.root) + "\n")
        self.assertFalse((self.home / ".bashrc").exists())

    def test_prerelease_build_metadata_and_optional_v(self):
        for value in ("1.2.3", "v1.2.3", "1.2.3-beta.1+build.00", "v1.2.3+hello"):
            self.archive(value.removeprefix("v"))
            self.install((value, "--no-path-update"))
        self.assertNotIn("latest", (self.base / "calls").read_text())

    def test_latest_preserves_encoded_build_metadata(self):
        self.archive("1.2.3+build.42")
        self.env["FIXTURE_LATEST"] = "https://github.com/liukaizheng/zevria/releases/tag/v1.2.3%2bbuild.42"
        self.install(("--no-path-update",))
        self.assertIn("/download/v1.2.3+build.42/", (self.base / "calls").read_text())

    def test_bad_latest_identity_and_tag(self):
        for url in ("http://github.com/liukaizheng/zevria/releases/tag/v1.2.3",
                    "https://github.com/other/zevria/releases/tag/v1.2.3",
                    "https://github.com/liukaizheng/zevria/releases/tag/v1.2.3/extra",
                    "https://github.com/liukaizheng/zevria/releases/tag/vwrong/v1.2.3",
                    "https://github.com/liukaizheng/zevria/releases/tag/v1.2.3-beta.1",
                    "https://github.com/liukaizheng/zevria/releases/tag/v1.02.3"):
            self.env["FIXTURE_LATEST"] = url
            self.install(expected=1)
        self.assertFalse(self.root.exists())

    def test_network_failures_leave_previous_binary(self):
        self.install(("1.2.3", "--no-path-update"))
        before = (self.root / "bin/zevria").read_bytes()
        for hook in ('zevria_latest() { return 22; }',
                     'zevria_download() { printf partial > "$2"; return 18; }'):
            self.install(hooks=hook, expected=1)
            self.assertEqual((self.root / "bin/zevria").read_bytes(), before)

    def test_checksum_rejections_before_replacement(self):
        self.install(("--no-path-update",))
        before = (self.root / "bin/zevria").read_bytes()
        for text in ("", self.line * 2, "not a digest  " + self.env["FIXTURE_ASSET"] + "\n",
                     "0" * 64 + "  " + self.env["FIXTURE_ASSET"] + "\n",
                     self.digest + "  other.tar.gz\n", self.line + "bad\n"):
            Path(self.env["FIXTURE_MANIFEST"]).write_text(text)
            self.install(expected=1)
            self.assertEqual((self.root / "bin/zevria").read_bytes(), before)

    def test_archive_inventory(self):
        for entries in (
            [], [("zevria", tarfile.SYMTYPE, b"")], [("zevria", tarfile.LNKTYPE, b"")],
            [("zevria", tarfile.DIRTYPE, b"")], [("zevria", tarfile.FIFOTYPE, b"")],
            [("../zevria", tarfile.REGTYPE, b"bad")], [("/zevria", tarfile.REGTYPE, b"bad")],
            [("nested/zevria", tarfile.REGTYPE, b"bad")],
            [("zevria", tarfile.REGTYPE, b"bad"), ("extra", tarfile.REGTYPE, b"bad")],
            [("zevria", tarfile.REGTYPE, b"bad")] * 2,
        ):
            with self.subTest(entries=entries):
                self.archive(entries=entries)
                self.install(expected=1)
                self.assertFalse(self.root.exists())

    def test_corrupt_archive_and_wrong_binary_version(self):
        Path(self.env["FIXTURE_ARCHIVE"]).write_bytes(b"not tar")
        self.manifest()
        self.install(expected=1)
        self.archive(binary_version="9.9.9")
        self.install(expected=1)
        self.assertFalse(self.root.exists())

    def test_repair_upgrade_downgrade_preserve_unrelated_data(self):
        (self.home / ".zevria").mkdir()
        sentinel = self.home / ".zevria/config.toml"
        sentinel.write_text("keep me")
        other = self.base / "other-root/bin"
        other.mkdir(parents=True)
        (other / "zevria").write_text("other installation")
        for version in ("1.2.3", "1.2.3", "2.0.0", "1.0.0"):
            self.archive(version)
            self.install((version, "--no-path-update"))
            self.assertEqual(subprocess.check_output([str(self.root / "bin/zevria"), "--version"], text=True), f"zevria {version}\n")
        self.assertEqual(sentinel.read_text(), "keep me")
        self.assertEqual((other / "zevria").read_text(), "other installation")
        self.assertFalse((self.home / "INJECTED").exists())
        self.assertFalse(list(self.root.glob("bin/.zevria-install.*")))

    def test_reject_unsafe_roots(self):
        for value in ("relative", "", str(self.root) + ":bad", str(self.root) + "\nbad", str(self.root) + "\tbad", str(self.root) + "\u0085bad"):
            self.env["ZEVRIA_INSTALL"] = value
            self.install(expected=1)
        self.assertFalse(self.root.exists())

    def test_default_root_and_locator(self):
        self.env.pop("ZEVRIA_INSTALL")
        self.install(("--no-path-update",))
        self.assertTrue((self.home / ".zevria/bin/zevria").is_file())
        self.assertEqual((self.home / ".zevria/install-root").read_text(), str(self.home / ".zevria") + "\n")

    def test_macos_does_not_write_linux_locator(self):
        self.env["FIXTURE_ASSET"] = "zevria-v1.2.3-aarch64-apple-darwin.tar.gz"
        self.manifest()
        self.install(("--no-path-update",), hooks='zevria_target() { printf aarch64-apple-darwin; }')
        self.assertFalse((self.home / ".zevria").exists())

    def test_refuse_destination_links_and_directories(self):
        binpath = self.root / "bin"
        binpath.mkdir(parents=True)
        destination = binpath / "zevria"
        destination.mkdir()
        self.install(expected=1)
        destination.rmdir()
        destination.symlink_to(self.base / "missing")
        self.install(expected=1)
        self.assertTrue(destination.is_symlink())
        destination.unlink()
        binpath.rmdir()
        binpath.symlink_to(self.home, target_is_directory=True)
        self.install(expected=1)
        self.assertFalse((self.home / "zevria").exists())

    def test_replacement_and_locator_failures_rollback(self):
        self.install(("--no-path-update",))
        old = (self.root / "bin/zevria").read_bytes()
        old_locator = (self.home / ".zevria/install-root").read_bytes()
        self.archive("2.0.0")
        for pattern in ("*/.zevria-install.*/new", "*/.install-root.*/new"):
            hook = f'mv() {{ case $1 in {pattern}) return 1;; esac; command mv "$@"; }}'
            self.install(("2.0.0", "--no-path-update"), hooks=hook, expected=1)
            self.assertEqual((self.root / "bin/zevria").read_bytes(), old)
            self.assertEqual((self.home / ".zevria/install-root").read_bytes(), old_locator)
        self.assertFalse(list(self.root.glob("bin/.zevria-install.*")))

    def test_first_install_locator_failure_leaves_no_executable(self):
        self.install(hooks='mv() { case $1 in */.install-root.*/new) return 1;; esac; command mv "$@"; }', expected=1)
        self.assertFalse((self.root / "bin/zevria").exists())
        self.assertFalse((self.home / ".zevria/install-root").exists())

    def test_failed_recovery_retains_backups(self):
        self.install(("--no-path-update",))
        self.archive("2.0.0")
        result = self.install(("2.0.0", "--no-path-update"), hooks='''mv() {
case "$*" in *install-root.*/new*|*previous*) if [[ $1 == -f || $1 == */new ]]; then return 1; fi;; esac
command mv "$@"
}''', expected=1)
        self.assertIn("Recovery failed", result.stderr)
        self.assertTrue(list(self.root.glob("bin/.zevria-install.*/previous")))

    def test_bash_profiles_login_precedence_and_deduplication(self):
        (self.home / ".profile").write_text("# preserve profile\n")
        (self.home / ".bashrc").write_text("# preserve rc\n")
        self.install()
        self.install()
        self.assertFalse((self.home / ".bash_profile").exists())
        for name in (".bashrc", ".profile"):
            text = (self.home / name).read_text()
            self.assertIn("# preserve", text)
            self.assertEqual(text.count("# >>> zevria installer >>>"), 1)
        self.env["EXPECTED_BIN"] = str(self.root / "bin")
        result = self.run_shell('PATH="/one:$EXPECTED_BIN:/two:$EXPECTED_BIN:/three:"; source "$HOME/.bashrc"; printf "%s" "$PATH"')
        self.assertEqual(result.stdout, f"{self.root}/bin:/one:/two:/three:")
        self.assertFalse((self.home / "INJECTED").exists())

    def test_missing_profiles_created_and_bash_profile_wins(self):
        self.install()
        self.assertTrue((self.home / ".bashrc").exists())
        self.assertTrue((self.home / ".bash_profile").exists())
        (self.home / ".bash_login").write_text("untouched")
        self.install()
        self.assertEqual((self.home / ".bash_login").read_text(), "untouched")

    def test_profiles_keep_symlinks_and_skip_broken_or_ambiguous(self):
        target = self.home / "real profile"
        target.write_text("# original\n")
        (self.home / ".bashrc").symlink_to(target.name)
        (self.home / ".bash_profile").symlink_to("absent")
        result = self.install()
        self.assertTrue((self.home / ".bashrc").is_symlink())
        self.assertIn("# original", target.read_text())
        self.assertIn("Could not safely update", result.stderr)
        target.write_text("# >>> zevria installer >>>\nuser content\n")
        self.install()
        self.assertEqual(target.read_text(), "# >>> zevria installer >>>\nuser content\n")

    def test_zdotdir_xdg_fish_and_unknown_shell(self):
        self.env.update(SHELL="/bin/zsh", ZDOTDIR=str(self.home / "z dot"))
        self.install()
        self.assertTrue((self.home / "z dot/.zshrc").exists())
        self.env.update(SHELL="/bin/fish", XDG_CONFIG_HOME=str(self.home / "x dg"))
        self.install()
        path = self.home / "x dg/fish/config.fish"
        self.assertTrue(path.exists())
        self.assertIn("set -gx PATH", path.read_text())
        if shutil.which("fish"):
            subprocess.run(["fish", "--no-config", "-c", 'source "$argv[1]"; test "$PATH[1]" = "$argv[2]"', str(path), str(self.root / "bin")], check=True, env=self.env)
        self.env["SHELL"] = "/bin/unknown"
        self.assertIn("Unknown shell", self.install().stderr)

    def test_unset_shell_and_unwritable_profile_are_nonfatal(self):
        result = self.install(hooks='unset SHELL')
        self.assertIn("Unknown shell", result.stderr)
        profile = self.home / ".bashrc"
        profile.write_text("# leave alone\n")
        profile.chmod(0o400)
        try:
            result = self.install()
            if not os.access(profile, os.W_OK):
                self.assertIn("Could not safely update", result.stderr)
                self.assertEqual(profile.read_text(), "# leave alone\n")
        finally:
            profile.chmod(0o600)

    def test_profile_update_keeps_block_position_and_is_idempotent(self):
        self.install()
        profile = self.home / ".bashrc"
        profile.write_text(profile.read_text() + "# code after installer block\n")
        before = profile.read_text()
        self.install()
        self.assertEqual(profile.read_text(), before)

    def test_bash_login_selection_and_failed_profile_creation(self):
        (self.home / ".bash_login").write_text("# login\n")
        (self.home / ".profile").write_text("# keep\n")
        self.install(hooks='zevria_shell_block() { return 1; }')
        self.assertFalse((self.home / ".bashrc").exists())
        self.install()
        self.assertIn("# >>> zevria installer >>>", (self.home / ".bash_login").read_text())
        self.assertEqual((self.home / ".profile").read_text(), "# keep\n")
        self.assertFalse((self.home / ".bash_profile").exists())

    def test_backslashes_and_zsh_profile_execution(self):
        self.root = self.base / "literal back\\slash and 'quote"
        self.env.update(ZEVRIA_INSTALL=str(self.root), SHELL="/bin/zsh")
        self.install()
        if shutil.which("zsh"):
            result = subprocess.run(["zsh", "-f", "-c", 'source "$1"; print -r -- "$path[1]"',
                                     "fixture", str(self.home / ".zshrc")], env=self.env,
                                    text=True, capture_output=True, check=True)
            self.assertEqual(result.stdout, str(self.root / "bin") + "\n")
        self.assertFalse((self.home / "INJECTED").exists())

    def test_gnu_tar_format_is_accepted(self):
        self.archive(fmt=tarfile.GNU_FORMAT)
        self.install(("--no-path-update",))

    def test_curl_security_and_bounded_options(self):
        result = self.run_shell('curl() { printf "%s\\n" "$@"; }; zevria_curl https://github.com')
        for option in ("=https", "--connect-timeout", "--max-time", "--retry", "--max-redirs", "--proto-redir"):
            self.assertIn(option, result.stdout)
        self.assertNotIn("--insecure", result.stdout)
        self.assertEqual(result.stdout.splitlines()[0], "-q", 'ignore user curlrc before any other option')


if __name__ == "__main__":
    unittest.main(verbosity=2)
