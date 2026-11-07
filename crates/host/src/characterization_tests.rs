use crate::session_config::SessionConfig;

#[test]
fn session_configuration_serialization_is_pinned() {
    let actual = toml::to_string(&SessionConfig::default()).unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session-default.toml");
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    assert_eq!(actual, std::fs::read_to_string(path).unwrap());
}
