use zevria_desktop::cli::{Cli, USAGE};

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let cli = Cli::parse(args)?;
    #[cfg(target_os = "macos")]
    return zevria_desktop::launch::run(cli);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = cli;
        anyhow::bail!(
            "Zevria Desktop currently supports macOS only; use the zevria CLI on this platform"
        )
    }
}
