use super::*;

#[test]
fn command_models_load_lazily_with_stable_revisions_and_redacted_args() {
    let mut models = models_value();
    models["providers"]["openai"]["api_key"] = serde_json::json!({
        "command":"zevria-helper-that-is-not-installed",
        "args":["PRIVATE_ARGUMENT"],
    });
    let config = parse_value(&models).unwrap();
    let command = config.providers["openai"]
        .as_responses()
        .unwrap()
        .api_key
        .as_command()
        .unwrap();
    assert_eq!(command.command, "zevria-helper-that-is-not-installed");
    assert_eq!(command.timeout_seconds, 30);
    let debug = format!("{config:?}");
    assert!(debug.contains("zevria-helper-that-is-not-installed"));
    assert!(!debug.contains("PRIVATE_ARGUMENT") && !debug.contains("literal-openai-key"));
    let revision = crate::models::revision(&config).unwrap();
    let other = Config::parse_at(
        REQUIRED_CONFIG,
        &models.to_string(),
        Path::new("another/models.jsonc"),
    )
    .unwrap();
    assert_eq!(revision, crate::models::revision(&other).unwrap());
    let round_trip = serde_json::to_value(&config.providers["openai"]).unwrap();
    assert_eq!(round_trip["api_key"]["command"], command.command);
    assert_eq!(round_trip["api_key"]["args"][0], "PRIVATE_ARGUMENT");
}

#[tokio::test]
async fn models_directory_is_bound_and_each_reload_starts_with_an_empty_cache() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    std::fs::write(&config_path, REQUIRED_CONFIG).unwrap();
    std::fs::write(directory.path().join("helper.py"), "import pathlib\np = pathlib.Path('runs')\np.write_text(str(int(p.read_text()) + 1) if p.exists() else '1')\nprint(pathlib.Path('credential').read_text())\n").unwrap();
    std::fs::write(directory.path().join("credential"), "bound-directory-key").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut models = models_value();
    models["providers"]["openai"]["base_url"] = serde_json::json!(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    models["providers"]["openai"]["api_key"] = serde_json::json!({
        "command": if cfg!(windows) { "python" } else { "python3" },
        "args":["helper.py", "PRIVATE_ARGUMENT"],
    });
    std::fs::write(models_path_for(&config_path), models.to_string()).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer bound-directory-key")
            );
            stream.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        }
    });
    let mut revision = None;
    for expected_runs in 1..=2 {
        let config = crate::models::load(&config_path).unwrap();
        let before = crate::models::revision(&config).unwrap();
        assert_eq!(revision.get_or_insert_with(|| before.clone()), &before);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("runs")).unwrap_or_else(|_| "0".into()),
            (expected_runs - 1).to_string()
        );
        zevria_provider::OpenAiProvider::connect(
            config
                .routing()
                .for_role(zevria_foundation::ModelRole::Build),
            zevria_foundation::ReasoningLevel::Medium,
            "stable",
            rig_agent::tool::server::ToolServer::new().run(),
            "session",
            "cache-key",
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("runs")).unwrap(),
            expected_runs.to_string()
        );
        assert_eq!(crate::models::revision(&config).unwrap(), before);
        assert!(!format!("{config:?}").contains("PRIVATE_ARGUMENT"));
    }
    server.await.unwrap();
}
