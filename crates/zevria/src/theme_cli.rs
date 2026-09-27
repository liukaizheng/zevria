//! Offline theme commands: dispatch before configuration/provider/TTY startup.
use zevria_theme::{HexRgb, generate_theme};

use crate::theme_store::{self, ThemeStore, validate_name};

const USAGE: &str = "usage: zevria theme generate --name <name> --background '#RRGGBB' | zevria theme reset\nSaves ~/.zevria/themes/<name>.toml and selects it for future TUI launches.\nNames: 1–64 lowercase ASCII letters/digits, underscores or hyphens; begin with a letter/digit.\nReset removes only the selection; saved theme files are preserved.";

#[derive(Debug, PartialEq)]
pub(crate) enum ThemeCli {
    Generate { name: String, background: HexRgb },
    Reset,
    Help,
}

impl ThemeCli {
    pub(crate) fn parse(args: &[String]) -> anyhow::Result<Self> {
        match args.first().map(String::as_str) {
            Some("--help" | "-h") if args.len() == 1 => Ok(Self::Help),
            Some("reset") if args.len() == 1 => Ok(Self::Reset),
            Some("generate") => {
                if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
                    return Ok(Self::Help);
                }
                let (mut name, mut background) = (None, None);
                let mut flags = args[1..].iter();
                while let Some(flag) = flags.next() {
                    anyhow::ensure!(
                        matches!(flag.as_str(), "--name" | "--background"),
                        "unknown theme argument {flag:?}; {USAGE}"
                    );
                    let value = flags
                        .next()
                        .filter(|v| !v.starts_with("--"))
                        .ok_or_else(|| anyhow::anyhow!("missing value for {flag}; {USAGE}"))?;
                    match flag.as_str() {
                        "--name" => {
                            anyhow::ensure!(name.is_none(), "duplicate --name; {USAGE}");
                            validate_name(value)?;
                            name = Some(value.clone());
                        }
                        "--background" => {
                            anyhow::ensure!(
                                background.is_none(),
                                "duplicate --background; {USAGE}"
                            );
                            background = Some(value.parse()?);
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(Self::Generate {
                    name: name.ok_or_else(|| anyhow::anyhow!("--name is required; {USAGE}"))?,
                    background: background
                        .ok_or_else(|| anyhow::anyhow!("--background is required; {USAGE}"))?,
                })
            }
            _ => anyhow::bail!("invalid theme command or argument combination; {USAGE}"),
        }
    }

    pub(crate) fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Help => println!("{USAGE}"),
            Self::Generate { name, background } => {
                // Resolve both locations before writes; ZEVRIA_CONFIG cannot
                // substitute for HOME because named themes have a fixed root.
                let store = ThemeStore::global()?;
                let config = zevria_foundation::config::config_path()?;
                let definition = generate_theme(background)?;
                let path = store.save_and_select(&name, &definition, &config)?;
                println!(
                    "Generated and selected theme {name}\nBackground: {background}\nSaved theme: {}\nSelected configuration: {}\nRestart Zevria to activate it. Already-running TUI processes are unchanged.",
                    path.display(),
                    config.display()
                );
            }
            Self::Reset => {
                let config = zevria_foundation::config::config_path()?;
                theme_store::reset(&config)?;
                println!(
                    "Theme selection reset to built-in Zevria Dark in {}. Saved themes are preserved. Restart Zevria to activate it; running TUI processes are unchanged.",
                    config.display()
                );
            }
        }
        Ok(())
    }
}
