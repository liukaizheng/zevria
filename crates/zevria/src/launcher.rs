//! Pre-application runtime selection. Probes never load configuration or create
//! application files. A successful handoff is terminal: no native retry exists.
#![cfg_attr(not(windows), allow(dead_code))]
use anyhow::Context as _;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;
use zevria_foundation::process::bounded_output;

const REVISION: u64 = 1;
const PROBE_LIMIT: usize = 32 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const HELP: &str = "usage: zevria [--runtime auto|wsl|native] [--wsl-distro NAME] [--continue | -c]\n       zevria [runtime options] --acp [--ensemble-worker]\n       zevria [runtime options] skills <command> | theme <command> | clean\n\nWindows: auto prefers a ready Linux Zevria in the default WSL distribution; ACP defaults to native.\n--runtime wsl requires WSL; --runtime native skips WSL probing.\n--wsl-distro selects only that distribution, without changing the default.\nExplicit WSL ACP requires a WSL-aware client using Linux paths.\n--help, --version do not load configuration.";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    #[default]
    Auto,
    Wsl,
    Native,
}
#[derive(Debug, Default, PartialEq, Eq)]
struct Controls {
    runtime: Runtime,
    distro: Option<String>,
    args: Vec<String>,
}
impl Controls {
    fn parse(args: Vec<String>) -> anyhow::Result<Self> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        let mut runtime_seen = false;
        while let Some(arg) = args.next() {
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--runtime" => {
                    anyhow::ensure!(!runtime_seen, "--runtime may be specified only once");
                    runtime_seen = true;
                    let value = inline
                        .map(str::to_owned)
                        .or_else(|| args.next())
                        .context("--runtime requires auto, wsl, or native")?;
                    result.runtime = match value.as_str() {
                        "auto" => Runtime::Auto,
                        "wsl" => Runtime::Wsl,
                        "native" => Runtime::Native,
                        _ => anyhow::bail!("invalid --runtime {value:?}; use auto, wsl, or native"),
                    };
                }
                "--wsl-distro" => {
                    anyhow::ensure!(
                        result.distro.is_none(),
                        "--wsl-distro may be specified only once"
                    );
                    let value = inline
                        .map(str::to_owned)
                        .or_else(|| args.next())
                        .context("--wsl-distro requires a distribution name")?;
                    anyhow::ensure!(
                        !value.is_empty()
                            && !value.starts_with('-')
                            && !value.contains(['\0', '\n', '\r', '/', '\\']),
                        "invalid --wsl-distro name"
                    );
                    result.distro = Some(value);
                }
                _ => result.args.push(arg),
            }
        }
        anyhow::ensure!(
            result.runtime != Runtime::Native || result.distro.is_none(),
            "--wsl-distro cannot be combined with --runtime native"
        );
        anyhow::ensure!(
            result.distro.is_none()
                || result.runtime != Runtime::Auto
                || !result.args.iter().any(|a| a == "--acp"),
            "--wsl-distro with ACP requires explicit --runtime wsl; automatic ACP is native"
        );
        Ok(result)
    }
    fn probes_wsl(&self, windows: bool) -> bool {
        windows
            && match self.runtime {
                Runtime::Native => false,
                Runtime::Wsl => true,
                Runtime::Auto => !self.args.iter().any(|a| a == "--acp"),
            }
    }
    fn permits_native_fallback(&self) -> bool {
        self.runtime == Runtime::Auto
    }
    fn needs_tools(&self) -> bool {
        !matches!(
            self.args.first().map(String::as_str),
            Some("clean" | "skills" | "theme")
        )
    }
}

pub enum Launch {
    Local(Vec<String>),
    Exit(i32),
}

pub async fn dispatch(args: Vec<String>) -> anyhow::Result<Launch> {
    // These private entry points bypass auto-selection and all app initialization.
    if args.first().is_some_and(|s| s == "--__acp-job") {
        return acp_helper(&args[1..]).await;
    }
    if args == ["--__launcher-probe"] || args == ["--__launcher-probe-tools"] {
        if args == ["--__launcher-probe-tools"] {
            zevria_foundation::shell::frozen()?.check_rtk().await?;
        }
        println!(
            "{}",
            serde_json::json!({"os": std::env::consts::OS, "version": env!("CARGO_PKG_VERSION"), "launcher_revision": REVISION})
        );
        return Ok(Launch::Exit(0));
    }
    let controls = Controls::parse(args)?;
    if controls.args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return Ok(Launch::Exit(0));
    }
    if controls.args == ["--version"] || controls.args == ["-V"] {
        println!("zevria {}", env!("CARGO_PKG_VERSION"));
        return Ok(Launch::Exit(0));
    }
    // Validate syntax before any probe or configuration side effect.
    super::parse_args(controls.args.iter().cloned())?;
    #[cfg(windows)]
    if controls.probes_wsl(true) {
        anyhow::ensure!(
            std::env::var_os("_ZEVRIA_WSL_HANDOFF").is_none(),
            "refusing recursive Windows-to-WSL launch; install the Linux Zevria binary inside WSL"
        );
        let wsl = zevria_foundation::windows_process::system_executable("wsl.exe")?;
        let cwd = std::env::current_dir()?;
        let config = std::env::var_os("ZEVRIA_CONFIG")
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        match prepare(&wsl, &controls, &cwd, config.as_deref()).await {
            Ok(handoff) => {
                eprintln!(
                    "zevria: runtime WSL ({}); workspace {}; configuration {}. No native retry after handoff.",
                    handoff.distro,
                    handoff.workspace,
                    if handoff.config.is_empty() {
                        "Linux home/.zevria/config.toml"
                    } else {
                        &handoff.config
                    }
                );
                if controls.args.iter().any(|s| s == "--acp") {
                    eprintln!(
                        "zevria: WSL ACP requires Linux paths from the client; ACP messages are not translated."
                    );
                }
                let _interrupts =
                    zevria_foundation::windows_process::ForwardConsoleInterrupts::install()?;
                let status = handoff.command(&wsl).status().await.context("failed to hand control to Linux Zevria; invocation will not be retried natively")?;
                return Ok(Launch::Exit(status.code().unwrap_or(1)));
            }
            Err(error) if controls.permits_native_fallback() => {
                eprintln!(
                    "zevria: WSL is not ready ({error:#}); using native Windows with Git Bash. Use --runtime native to skip probing or --runtime wsl to require WSL."
                )
            }
            Err(error) => {
                return Err(
                    error.context("requested WSL runtime is not ready; no fallback was attempted")
                );
            }
        }
    }
    #[cfg(not(windows))]
    {
        anyhow::ensure!(
            controls.distro.is_none(),
            "--wsl-distro is a Windows launcher option; this process already runs locally"
        );
    }
    Ok(Launch::Local(controls.args))
}

pub fn diagnostics(workspace: &Path) -> anyhow::Result<()> {
    let config = zevria_foundation::config::config_path()?;
    let config = if config.is_absolute() {
        config
    } else {
        workspace.join(config)
    };
    let state = zevria_foundation::runtime_paths::workspace_state_root(workspace);
    let distro = if cfg!(target_os = "linux") {
        std::env::var("WSL_DISTRO_NAME")
            .ok()
            .map(|d| format!(" (WSL {d})"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    eprintln!(
        "zevria: runtime {}{}; configuration {}; state {}",
        std::env::consts::OS,
        distro,
        config.display(),
        state.display()
    );
    Ok(())
}

// Fixed source only. Values are positional arguments, never shell fragments.
// Linux timeout bounds the bootstrap itself even if killing wsl.exe cannot stop
// Linux descendants. No login profiles, installation, configuration, or providers.
const LINUX_SETUP: &str = include_str!("launcher_linux.sh");
fn linux_script(body: &str) -> String {
    format!("{LINUX_SETUP}\n{body}")
}
const PROBE_SCRIPT: &str = r#"zevria_path "$HOME/.zevria/bin"
map_path() { case "$1" in linux) printf '%s' "$2";; drive) wslpath -u "$2";; *) exit 31;; esac; }
cwd=$(map_path "$1" "$2")
config=''; if [ -n "$4" ]; then config=$(map_path "$3" "$4"); fi
skill=''; if [ -n "$6" ]; then skill=$(map_path "$5" "$6"); fi
cd -- "$cwd" || { echo 'workspace inaccessible in selected distribution' >&2; exit 32; }
zevria_select
probe=--__launcher-probe
if [ "$7" = tools ]; then probe=--__launcher-probe-tools; fi
printf '%s\000' "$exe" "${WSL_DISTRO_NAME:-}" "$cwd" "$config" "$skill"
"$exe" "$probe"
"#;
const HANDOFF_SCRIPT: &str = r#"exe=$1; cwd=$2; config=$3; shift 3
zevria_path "${exe%/*}"
cd -- "$cwd"
if [ -n "$config" ]; then export ZEVRIA_CONFIG="$config"; else unset ZEVRIA_CONFIG; fi
export _ZEVRIA_WSL_HANDOFF=1
exec "$exe" "$@"
"#;

#[derive(Debug, PartialEq, Eq)]
struct MappedPath {
    kind: &'static str,
    value: String,
    distro: Option<String>,
}
fn map_input(input: &str, cwd: &str) -> anyhow::Result<MappedPath> {
    anyhow::ensure!(
        !input.contains(['\0', '\n', '\r']),
        "path contains unsupported control characters"
    );
    let input = input.replace('/', "\\");
    let input = input
        .strip_prefix(r"\\?\UNC\")
        .map(|s| format!("\\\\{s}"))
        .unwrap_or(input);
    if let Some(verbatim) = input.strip_prefix(r"\\?\") {
        let bytes = verbatim.as_bytes();
        anyhow::ensure!(
            bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\",
            "unsupported Windows verbatim device path"
        );
    }
    let input = input.strip_prefix(r"\\?\").unwrap_or(&input).to_owned();
    if let Some(share) = input.strip_prefix(r"\\") {
        let parts: Vec<_> = share.split('\\').collect();
        anyhow::ensure!(
            parts.len() >= 2
                && (parts[0].eq_ignore_ascii_case("wsl.localhost")
                    || parts[0].eq_ignore_ascii_case("wsl$"))
                && !parts[1].is_empty(),
            "unmappable network/device path; only local drives and WSL shares are supported"
        );
        anyhow::ensure!(
            parts.iter().skip(2).all(|s| *s != ".." && !s.contains(':')),
            "invalid WSL share path"
        );
        return Ok(MappedPath {
            kind: "linux",
            value: format!("/{}", parts[2..].join("/")),
            distro: Some(parts[1].into()),
        });
    }
    let bytes = input.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        anyhow::ensure!(
            !input[2..].contains(':'),
            "alternate data streams are not mappable to WSL"
        );
        return Ok(MappedPath {
            kind: "drive",
            value: input,
            distro: None,
        });
    }
    anyhow::ensure!(
        !input.starts_with('\\') && !input.contains(':'),
        "path must be drive-absolute, relative, or a recognized WSL share"
    );
    anyhow::ensure!(
        !cwd.is_empty(),
        "cannot resolve relative path without a workspace"
    );
    map_input(&format!("{cwd}\\{input}"), "")
}

#[derive(Debug)]
struct Handoff {
    executable: String,
    distro: String,
    workspace: String,
    config: String,
    args: Vec<String>,
}
fn linux_wslenv(env: &str) -> String {
    env.split(':')
        .filter(|entry| {
            !matches!(
                entry
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "HOME"
                    | "SHELL"
                    | "ZEVRIA_CONFIG"
                    | "ZEVRIA_INSTALL"
                    | "BASH_ENV"
                    | "ENV"
                    | "_ZEVRIA_WSL_HANDOFF"
            )
        })
        .collect::<Vec<_>>()
        .join(":")
}
fn wsl_command(wsl: &Path, distro: Option<&str>) -> Command {
    let mut command = Command::new(wsl);
    if let Some(distro) = distro {
        command.args(["--distribution", distro]);
    }
    command.args(["--cd", "/", "--exec"]);
    // WSLENV is an explicit bridge. Remove runtime-home/config entries instead
    // of letting Windows HOME/SHELL become Linux defaults. Other entries survive.
    if let Ok(env) = std::env::var("WSLENV") {
        command.env("WSLENV", linux_wslenv(&env));
    }
    for name in [
        "HOME",
        "SHELL",
        "ZEVRIA_CONFIG",
        "ZEVRIA_INSTALL",
        "BASH_ENV",
        "ENV",
    ] {
        command.env_remove(name);
    }
    command
}
impl Handoff {
    fn command(&self, wsl: &Path) -> Command {
        let mut command = wsl_command(wsl, Some(&self.distro));
        command.args([
            "/bin/sh",
            "-c",
            &linux_script(HANDOFF_SCRIPT),
            "zevria-handoff",
            &self.executable,
            &self.workspace,
            &self.config,
        ]);
        command
            .args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
    }
}
fn skill_path_index(args: &[String]) -> Option<usize> {
    if args.first()?.as_str() != "skills" {
        return None;
    }
    let mut positional = args
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, a)| a.as_str() != "--json");
    if positional.next()?.1 != "validate" {
        return None;
    }
    Some(positional.next()?.0)
}

async fn prepare(
    wsl: &Path,
    controls: &Controls,
    cwd: &Path,
    config: Option<&Path>,
) -> anyhow::Result<Handoff> {
    let cwd = cwd
        .to_str()
        .context("workspace path is not valid Unicode")?;
    let workspace = map_input(cwd, "")?;
    let empty = MappedPath {
        kind: "linux",
        value: String::new(),
        distro: None,
    };
    let config = config
        .map(|p| {
            map_input(
                p.to_str().context("ZEVRIA_CONFIG is not valid Unicode")?,
                cwd,
            )
        })
        .transpose()?
        .unwrap_or_else(|| MappedPath {
            kind: "linux",
            value: String::new(),
            distro: None,
        });
    let skill_index = skill_path_index(&controls.args);
    let skill = skill_index
        .map(|i| map_input(&controls.args[i], cwd))
        .transpose()?
        .unwrap_or(empty);
    let mut command = wsl_command(wsl, controls.distro.as_deref());
    command.args([
        "/usr/bin/timeout",
        "--kill-after=1s",
        // The companion's RTK probe has an 8s deadline; let it clean up its
        // own Unix process group before the outer bootstrap deadline fires.
        "10s",
        "/bin/sh",
        "-c",
        &linux_script(PROBE_SCRIPT),
        "zevria-probe",
        workspace.kind,
        &workspace.value,
        config.kind,
        &config.value,
        skill.kind,
        &skill.value,
        if controls.needs_tools() {
            "tools"
        } else {
            "offline"
        },
    ]);
    let output = bounded_output(&mut command, PROBE_TIMEOUT, PROBE_LIMIT)
        .await
        .context("WSL distribution/bootstrap unavailable")?;
    anyhow::ensure!(
        output.status.success(),
        "WSL readiness failed ({}): {}{}",
        output.status,
        wsl_diagnostic(&output.stderr),
        wsl_diagnostic(&output.stdout)
    );
    decode_probe(
        &output.stdout,
        controls,
        [&workspace, &config, &skill],
        skill_index,
    )
}
// wsl.exe startup errors may be UTF-16LE, while Linux/bootstrap diagnostics
// are UTF-8. Decode both without letting NUL-delimited partial probes garble stderr.
fn wsl_diagnostic(bytes: &[u8]) -> String {
    let wide = bytes.starts_with(&[0xff, 0xfe])
        || (bytes.len() >= 4
            && bytes.len().is_multiple_of(2)
            && bytes
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|pair| pair[1] == 0)
                .count()
                > bytes.len() / 4);
    let text = if wide {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.replace('\0', " | ")
}

fn decode_probe(
    output: &[u8],
    controls: &Controls,
    paths: [&MappedPath; 3],
    skill_index: Option<usize>,
) -> anyhow::Result<Handoff> {
    let fields = output
        .splitn(6, |b| *b == 0)
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(fields.len() == 6, "invalid Linux launcher probe response");
    let probe: serde_json::Value = serde_json::from_str(fields[5])
        .context("incompatible Linux Zevria: missing launcher probe protocol")?;
    anyhow::ensure!(
        probe["os"] == "linux"
            && probe["version"] == env!("CARGO_PKG_VERSION")
            && probe["launcher_revision"] == REVISION,
        "incompatible Linux companion; install Linux Zevria {} (launcher revision {REVISION})",
        env!("CARGO_PKG_VERSION")
    );
    anyhow::ensure!(
        fields[0].starts_with('/')
            && !fields[0].ends_with(".exe")
            && fields[2].starts_with('/')
            && !fields[1].is_empty(),
        "invalid Linux executable, distribution, or workspace in launcher probe"
    );
    for expected in controls
        .distro
        .iter()
        .chain(paths.iter().filter_map(|p| p.distro.as_ref()))
    {
        anyhow::ensure!(
            expected.eq_ignore_ascii_case(fields[1]),
            "path/distribution conflict: selected WSL distribution is {:?}, but input requires {expected:?}; select the matching --wsl-distro",
            fields[1]
        );
    }
    let mut args = controls.args.clone();
    if let Some(index) = skill_index {
        args[index] = fields[4].into();
    }
    Ok(Handoff {
        executable: fields[0].into(),
        distro: fields[1].into(),
        workspace: fields[2].into(),
        config: fields[3].into(),
        args,
    })
}

async fn acp_helper(args: &[String]) -> anyhow::Result<Launch> {
    #[cfg(not(windows))]
    {
        let _ = args;
        anyhow::bail!("the ACP job helper is Windows-only");
    }
    #[cfg(windows)]
    {
        anyhow::ensure!(!args.is_empty(), "ACP job helper requires a command");
        let shell = zevria_foundation::shell::frozen()?;
        // Positional argv supports npm/npx shell scripts without string rewriting.
        let mut command = shell.command(r#"program=$1; shift; case "$program" in [A-Za-z]:*|\\\\*) program=$(cygpath -u -- "$program") || exit;; esac; "$program" "$@""#);
        command
            .arg("zevria-acp-job")
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let (mut child, job) = zevria_foundation::process::ProcessTree::spawn(&mut command)?;
        let status = child.wait().await?;
        job.kill();
        Ok(Launch::Exit(status.code().unwrap_or(1)))
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "launcher_linux_tests.rs"]
mod linux_tests;

#[cfg(test)]
#[path = "launcher_mock_tests.rs"]
mod mocked_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn controls(args: &[&str]) -> Controls {
        Controls::parse(args.iter().map(|s| s.to_string()).collect()).unwrap()
    }
    #[test]
    fn wsl_errors_decode_both_windows_utf16_and_linux_utf8() {
        let message = "Wsl/Service/WSL_E_DISTRO_NOT_FOUND";
        let wide: Vec<_> = message.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(wsl_diagnostic(&wide), message);
        assert_eq!(wsl_diagnostic(message.as_bytes()), message);
        assert_eq!(
            wsl_diagnostic(b"executable\0Ubuntu\0"),
            "executable | Ubuntu | "
        );
    }

    #[test]
    fn selection_contract_and_argument_vectors() {
        assert!(controls(&[]).probes_wsl(true));
        assert!(!controls(&["--acp"]).probes_wsl(true));
        assert!(!controls(&["--runtime", "native"]).probes_wsl(true));
        assert!(controls(&["--runtime=wsl", "--acp"]).probes_wsl(true));
        assert!(!controls(&["--runtime=wsl"]).probes_wsl(false));
        for args in [
            vec![],
            vec!["--runtime=auto"],
            vec!["--runtime=auto", "--wsl-distro", "Ubuntu"],
            vec!["--wsl-distro", "Ubuntu"],
        ] {
            let automatic = controls(&args);
            assert!(automatic.probes_wsl(true));
            assert!(automatic.permits_native_fallback());
        }
        for args in [
            vec!["--runtime=wsl"],
            vec!["--runtime=wsl", "--wsl-distro", "Ubuntu"],
            vec!["--runtime=native"],
        ] {
            assert!(!controls(&args).permits_native_fallback());
        }
        let c = controls(&[
            "skills",
            "validate",
            "C:\\a b\\雪';&.md",
            "--runtime",
            "wsl",
            "--wsl-distro",
            "Ubuntu",
        ]);
        assert_eq!(c.args, ["skills", "validate", "C:\\a b\\雪';&.md"]);
        assert!(!c.needs_tools());
        assert!(
            Controls::parse(vec![
                "--runtime".into(),
                "native".into(),
                "--wsl-distro=X".into()
            ])
            .is_err()
        );
    }
    #[test]
    fn maps_only_known_path_forms_without_shell_interpolation() {
        for args in [
            vec!["skills", "validate", "--json", "C:/file"],
            vec!["skills", "--json", "validate", "C:/file"],
            vec!["skills", "validate", "C:/file", "--json"],
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert_eq!(args[skill_path_index(&args).unwrap()], "C:/file");
        }
        for path in [r"C:\a b\雪';&.md", r"\\?\C:\a b\雪';&.md"] {
            assert_eq!(map_input(path, "").unwrap().value, r"C:\a b\雪';&.md");
        }
        assert_eq!(
            map_input("skills/x", r"D:\work").unwrap().value,
            r"D:\work\skills\x"
        );
        for host in ["wsl$", "wsl.localhost"] {
            let mapped = map_input(&format!(r"\\{host}\Ubuntu\home\a b\雪"), "").unwrap();
            assert_eq!(mapped.value, "/home/a b/雪");
            assert_eq!(mapped.distro.as_deref(), Some("Ubuntu"));
        }
        for path in [
            r"\\server\share\x",
            r"\\.\PIPE\foo",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            "C:relative",
            "C:\\file:stream",
            "\\relative",
        ] {
            assert!(map_input(path, r"C:\work").is_err(), "{path}");
        }
    }
    #[test]
    fn installation_environment_never_crosses_the_wsl_bridge() {
        assert_eq!(
            linux_wslenv(
                "KEEP/p:ZEVRIA_INSTALL/p:zevria_install/u:ZeVrIa_InStAlL/lw:HOME/p:SHELL:ZEVRIA_CONFIG/p:BASH_ENV:env:_ZEVRIA_WSL_HANDOFF:OTHER/l"
            ),
            "KEEP/p:OTHER/l"
        );
        let command = wsl_command(Path::new("wsl.exe"), None);
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "ZEVRIA_INSTALL" && value.is_none())
        );
    }
    fn probe(os: &str, version: &str) -> Vec<u8> {
        format!("/home/me/.cargo/bin/zevria\0Ubuntu\0/mnt/c/work space\0/mnt/c/config.toml\0/mnt/c/x';&.md\0{}", serde_json::json!({"os":os,"version":version,"launcher_revision":REVISION})).into_bytes()
    }
    #[test]
    fn compatible_linux_only_and_distribution_conflicts() {
        let c = controls(&["skills", "validate", "file", "--json"]);
        let path = map_input(r"C:\work", "").unwrap();
        let ready = decode_probe(
            &probe("linux", env!("CARGO_PKG_VERSION")),
            &c,
            [&path, &path, &path],
            Some(2),
        )
        .unwrap();
        assert_eq!(ready.args[2], "/mnt/c/x';&.md");
        let command = ready.command(Path::new("wsl.exe"));
        let argv: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv.last().unwrap(), "--json");
        assert!(argv.contains(&linux_script(HANDOFF_SCRIPT)));
        assert!(
            decode_probe(
                &probe("windows", env!("CARGO_PKG_VERSION")),
                &c,
                [&path, &path, &path],
                None
            )
            .is_err()
        );
        assert!(decode_probe(&probe("linux", "0.0.0"), &c, [&path, &path, &path], None).is_err());
        let share = map_input(r"\\wsl$\Debian\home", "").unwrap();
        assert!(
            decode_probe(
                &probe("linux", env!("CARGO_PKG_VERSION")),
                &c,
                [&share, &path, &path],
                None
            )
            .is_err()
        );
        assert!(decode_probe(b"no distribution", &c, [&path, &path, &path], None).is_err());
    }
}
