//! Shared isolated fixtures for CommandTool and real native ACP worker tests.
//! These verify cooperative investigative commands, NOT executor confinement.
#![allow(dead_code)] // Each consumer exercises a different subset.

use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const DIAGNOSTIC: &str = "external synthetic diagnostic: 41\n";
pub const DOWNLOAD: &str = "controlled fixture: 41\n";
pub const SCRIPT_RESULT: &str = "scratch script result: 42";
pub const BUILD_RESULT: &str = "scratch build/test result: 42";

// No dependencies, lifecycle hooks, network, or writes outside explicit cwd.
const SOURCE: &str = "def answer():\n    return 42\n";
const BUILD: &str = r#"import os, pathlib, py_compile, runpy, unittest
root = pathlib.Path.cwd().resolve()
for name in ('HOME', 'TMPDIR', 'XDG_CACHE_HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME'):
    assert pathlib.Path(os.environ[name]).resolve().is_relative_to(root), name
assert pathlib.Path(__file__).resolve().parent == root
assert (root / 'download.txt').read_text() == 'CONTROLLED FIXTURE: 41\n'
py_compile.compile('source.py', cfile='build/source.pyc', doraise=True)
class TestSource(unittest.TestCase):
    def test_answer(self):
        self.assertEqual(runpy.run_path('source.py')['answer'](), 42)
result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(TestSource))
assert result.wasSuccessful()
(root / 'build/test-result.txt').write_text('scratch build/test result: 42\n')
print('scratch build/test result: 42')
"#;

pub fn quote(value: impl AsRef<std::ffi::OsStr>) -> String {
    format!(
        "'{}'",
        value.as_ref().to_string_lossy().replace('\'', "'\\''")
    )
}

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub diagnostic: PathBuf,
    pub system_temp: PathBuf,
    protected: Vec<(PathBuf, Vec<u8>)>,
}
impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project with spaces");
        let home = root.path().join("normal home");
        let diagnostic = home.join(".zevria/logs/zevria.log");
        std::fs::create_dir_all(workspace.join("tmp")).unwrap();
        std::fs::create_dir_all(diagnostic.parent().unwrap()).unwrap();
        let protected = [
            (workspace.join("source.py"), SOURCE),
            (workspace.join("build.py"), BUILD),
            (workspace.join("source.txt"), "source remains unchanged\n"),
            (
                workspace.join("tmp/not-scratch.txt"),
                "project tmp is protected\n",
            ),
            (
                root.path().join("unrelated-temp.txt"),
                "unrelated temp is protected\n",
            ),
            (home.join("normal-config.toml"), "fixture = 'unchanged'\n"),
            (diagnostic.clone(), DIAGNOSTIC),
        ]
        .into_iter()
        .map(|(path, contents)| {
            std::fs::write(&path, contents).unwrap();
            (path, contents.as_bytes().to_vec())
        })
        .collect();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&diagnostic, workspace.join("external-link")).unwrap();
        Self {
            root,
            workspace,
            home,
            diagnostic,
            protected,
            system_temp: std::env::temp_dir().canonicalize().unwrap(),
        }
    }

    // RTK's raw `run` route does not track usage or write global RTK state.
    // Python's isolated mode and explicit cache prefix avoid user configuration.
    // This deliberately small script is inspected test content, not a sandbox.
    pub fn read_command(&self) -> String {
        let body = format!(
            r#"import pathlib
p = pathlib.Path({home:?})
print('CWD=' + str(pathlib.Path.cwd()))
for path in (pathlib.Path({absolute:?}), pathlib.Path('../normal home/.zevria/logs/zevria.log'), pathlib.Path('external-link')):
    print(path.read_text(), end='')
print((p / '.zevria/logs/zevria.log').read_text(), end='')
for name in ('source.txt', 'source.py', 'build.py'):
    print(pathlib.Path(name).read_text(), end='')
"#,
            home = self.home.to_str().unwrap(),
            absolute = self.diagnostic.to_str().unwrap()
        );
        format!(
            "rtk run {}",
            quote(format!("python3 -I -B -c {}", quote(body)))
        )
    }

    pub fn home_read_command(&self) -> String {
        // HOME is set only in this child command; never mutate process-global env.
        format!(
            "rtk run {}",
            quote(format!(
                "HOME={} /bin/sh -c 'cat ~/.zevria/logs/zevria.log'",
                quote(&self.home)
            ))
        )
    }

    pub fn missing_command(&self) -> String {
        format!(
            "rtk run {}",
            quote(format!(
                "cat {}",
                quote(self.home.join("missing-diagnostic"))
            ))
        )
    }

    pub fn create_command(&self) -> String {
        format!(
            "rtk run {}",
            quote(format!(
                "umask 077; scratch=$(mktemp -d {}) && mkdir \"$scratch/home\" \"$scratch/tmp\" \"$scratch/cache\" \"$scratch/config\" \"$scratch/data\" \"$scratch/build\" && printf 'SCRATCH=%s\\n' \"$scratch\"",
                quote(self.system_temp.join("zevria-inspection.XXXXXXXX"))
            ))
        )
    }

    pub fn own_scratch(&self, output: &str) -> Scratch {
        let path = output
            .lines()
            .find_map(|line| line.strip_prefix("SCRATCH="))
            .unwrap_or_else(|| {
                panic!(
                    "scratch path missing from ordinary command result; expected a `SCRATCH=` line:\n{output}"
                )
            });
        let path = PathBuf::from(path).canonicalize().unwrap();
        assert_eq!(path.parent().unwrap(), self.system_temp);
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("zevria-inspection.")
        );
        assert!(!path.starts_with(self.root.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        Scratch(path)
    }

    pub fn download_command(&self, scratch: &Path, url: &str) -> String {
        // The downloaded fixture is data only. No extraction or downloaded code.
        python(
            scratch,
            &format!(
                r#"import pathlib, urllib.request
url = {url:?}
assert url.startswith('http://127.0.0.1:')
data = urllib.request.build_opener(urllib.request.ProxyHandler({{}})).open(url, timeout=5).read(1024)
pathlib.Path('download.txt').write_bytes(data)
print('DOWNLOADED=' + data.decode().strip())
"#
            ),
        )
    }

    pub fn prepare_command(&self, scratch: &Path) -> String {
        python(
            scratch,
            &format!(
                r#"import pathlib, shutil
root = pathlib.Path.cwd()
for name in ('home', 'tmp', 'cache', 'config', 'data', 'build'):
    (root / name).mkdir(exist_ok=True)
for name in ('source.py', 'build.py'):
    source = pathlib.Path({workspace:?}) / name
    assert source.is_file() and not source.is_symlink()
    shutil.copyfile(source, root / name)
path = root / 'download.txt'
print('INSPECTED=' + path.read_text().strip())
path.write_text(path.read_text().upper())
(root / 'probe.py').write_text("print('scratch script result: 42')\n")
print((root / 'probe.py').read_text(), end='')
"#,
                workspace = self.workspace.to_str().unwrap()
            ),
        )
    }

    pub fn execute_command(&self, scratch: &Path) -> String {
        let body = "import runpy; runpy.run_path('probe.py', run_name='__main__'); runpy.run_path('build.py', run_name='__main__')";
        python(scratch, body)
    }

    pub fn cleanup_command(&self, scratch: &Path) -> String {
        assert_eq!(scratch.parent().unwrap(), self.system_temp);
        assert!(
            scratch
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("zevria-inspection.")
        );
        format!(
            "rtk run {}",
            quote(format!(
                "rm -rf -- {} && printf 'owned scratch cleaned\\n'",
                quote(scratch)
            ))
        )
    }

    pub fn assert_unchanged(&self) {
        for (path, expected) in &self.protected {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                expected,
                "protected fixture changed: {}",
                path.display()
            );
        }
        let mut project_entries = std::fs::read_dir(&self.workspace)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".zevria") // Engine transcript/lease writes are allowed.
            .collect::<Vec<_>>();
        project_entries.sort();
        assert_eq!(
            project_entries,
            [
                "build.py",
                "external-link",
                "source.py",
                "source.txt",
                "tmp"
            ]
        );
        assert_eq!(
            std::fs::read_dir(self.workspace.join("tmp"))
                .unwrap()
                .count(),
            1
        );
        for dir in [".cache", ".config", ".local", "Library", "__pycache__"] {
            assert!(
                !self.home.join(dir).exists(),
                "unexpected home state: {dir}"
            );
        }
    }
}

fn python(scratch: &Path, body: &str) -> String {
    // No inherited credentials, proxies, user packages, or Python startup hooks.
    let mut env = format!(
        "env -i PATH={} ",
        quote(std::env::var_os("PATH").unwrap_or_default())
    );
    for (key, dir) in [
        ("HOME", "home"),
        ("TMPDIR", "tmp"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
    ] {
        env.push_str(&format!("{key}={} ", quote(scratch.join(dir))));
    }
    format!(
        "rtk run {}",
        quote(format!(
            "cd {} && {env}python3 -I -X pycache_prefix={} -c {}",
            quote(scratch),
            quote(scratch.join("cache")),
            quote(body)
        ))
    )
}

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn assert_results(&self, fixture: &Fixture) {
        assert_eq!(
            std::fs::read_to_string(self.0.join("build/test-result.txt"))
                .unwrap()
                .trim(),
            BUILD_RESULT
        );
        assert!(self.0.join("build/source.pyc").is_file());
        assert!(
            std::fs::read_dir(self.0.join("cache"))
                .unwrap()
                .next()
                .is_some()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = std::fs::metadata(fixture.workspace.join("source.py")).unwrap();
            let copied = std::fs::metadata(self.0.join("source.py")).unwrap();
            assert_ne!(
                (original.dev(), original.ino()),
                (copied.dev(), copied.ino())
            );
            assert_eq!(copied.nlink(), 1);
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct HttpFixture {
    pub url: String,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl HttpFixture {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/fixture.txt", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    assert!(bytes.len() < 4096, "bounded fixture request headers");
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                assert!(
                    std::str::from_utf8(&bytes)
                        .unwrap()
                        .starts_with("GET /fixture.txt HTTP/1.1\r\n")
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{DOWNLOAD}",
                    DOWNLOAD.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            stopped,
            thread: Some(thread),
        }
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
