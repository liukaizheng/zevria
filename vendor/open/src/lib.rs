//! The native Unix opener interface used by GPUI's optional graphical backends.

use std::{
    ffi::{OsStr, OsString},
    process::Command,
};

/// Return the native opener commands in fallback order without spawning them.
pub fn commands(target: impl AsRef<OsStr>) -> Vec<Command> {
    let target = target.as_ref();
    let mut safe = OsString::new();
    // Not every opener accepts an end-of-options marker. A leading dash can
    // only occur in a relative path, so prefix it with `./` for those openers.
    if target.as_encoded_bytes().starts_with(b"-") {
        safe.push("./");
    }
    safe.push(target);
    let safe = safe.as_os_str();

    [
        ("xdg-open", &[safe][..]),
        ("gio", &[OsStr::new("open"), safe]),
        ("gnome-open", &[safe]),
        ("kde-open", &[OsStr::new("--"), target]),
    ]
    .into_iter()
    .map(|(program, args)| {
        let mut command = Command::new(program);
        command.args(args);
        command
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_order_and_arguments() {
        let url = "https://example.com/a?x=1&y=2";
        for (target, safe) in [
            (url, url),
            ("/tmp/project notes.txt", "/tmp/project notes.txt"),
            ("-file.txt", "./-file.txt"),
        ] {
            let expected = [
                ("xdg-open", &[safe][..]),
                ("gio", &["open", safe]),
                ("gnome-open", &[safe]),
                ("kde-open", &["--", target]),
            ];
            let actual = commands(target);
            assert_eq!(actual.len(), expected.len());
            for (command, (program, args)) in actual.iter().zip(expected) {
                assert_eq!(command.get_program(), program);
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    args.iter().map(OsStr::new).collect::<Vec<_>>()
                );
            }
        }
    }
}
