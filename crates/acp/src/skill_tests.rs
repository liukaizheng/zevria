use super::*;
use crate::skills::*;
use zevria_instructions::skill::SkillManagementResult;
use zevria_instructions::skill::SkillManagementView;

async fn list(
    connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    id: &agent_client_protocol::schema::v1::SessionId,
) -> SkillManagementView {
    let response = connection
        .send_request(SkillsListRequest {
            version: 1,
            session_id: id.clone(),
            query: String::new(),
        })
        .block_task()
        .await
        .unwrap();
    assert_eq!(response.version, 1);
    let SkillManagementResult::View { view: page } = response.result else {
        panic!("expected complete management view")
    };
    page
}

#[tokio::test]
async fn skills_extensions_dispatch_exact_underscore_methods_and_track_name_invocation() {
    let workspace = tempfile::tempdir().unwrap();
    let factory = Arc::new(FakeFactory::new());
    let changes = Arc::new(Mutex::new(Vec::<SkillsChangedNotification>::new()));
    let updates = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let (client_transport, server_transport) = Channel::duplex();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            factory,
            server_transport,
        )
        .await
    });
    let change_log = changes.clone();
    let update_log = updates.clone();
    let client = Client.builder()
        .on_receive_notification(async move |notification: SkillsChangedNotification, _connection| { change_log.lock().unwrap().push(notification); Ok(()) }, agent_client_protocol::on_receive_notification!())
        .on_receive_notification(async move |notification: SessionNotification, _connection| { update_log.lock().unwrap().push(notification); Ok(()) }, agent_client_protocol::on_receive_notification!())
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                let initialized = connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                let capabilities = serde_json::to_value(initialized.agent_capabilities).unwrap();
                assert_eq!(capabilities["_meta"]["zevria.skills"]["version"], 1);
                assert!(capabilities["_meta"]["zevria.skills"]["requests"].as_array().unwrap().iter().any(|method| method == "_zevria/skills/invoke"));
                let created = connection.send_request(NewSessionRequest::new(workspace.clone())).block_task().await?;
                let second = connection.send_request(NewSessionRequest::new(workspace)).block_task().await?;
                let id = created.session_id;
                let first = list(&connection, &id).await;
                let name = first.completions[0].name.clone();
                let digest = first.entries[0].digest;
                assert!(!serde_json::to_string(&first).unwrap().contains("PRIVATE MAIN BODY"));
                // Frontends never route ordinary intent (or $ syntax in ACP)
                // themselves. Model selection belongs to core disclosure.
                for prompt in ["$review literal", "commit the changes"] {
                    connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from(prompt)])).block_task().await?;
                }
                assert_eq!(list(&connection, &id).await.counts.active, 0);
                let result = connection.send_request(SkillInvokeRequest { version: 1, session_id: id.clone(), name: name.clone(), args: vec![ContentBlock::from("review this")] }).block_task().await?;
                assert_eq!(result.version, 1);
                assert_eq!(result.stop_reason, StopReason::EndTurn);
                let active = list(&connection, &id).await;
                assert_eq!(active.counts.active, 1);
                assert_eq!(active.entries[0].digest, digest);
                let disabled = connection.send_request(SkillsConfigWriteRequest { version: 1, session_id: id.clone(), expected_revision: first.revision.clone(), name: name.clone(), enabled: false }).block_task().await?;
                let SkillManagementResult::Changed { revision, .. } = disabled.result else { panic!("changed result") };
                assert_ne!(revision, first.revision);
                assert_eq!(list(&connection, &second.session_id).await.revision, first.revision, "other session isolated");
                let error = connection.send_request(SkillInvokeRequest { version: 1, session_id: id.clone(), name: name.clone(), args: Vec::new() }).block_task().await;
                assert!(error.is_err(), "disabled invocation must terminate with an error, not hang");
                let stale = connection.send_request(SkillsReloadRequest { version: 1, session_id: id.clone(), expected_revision: first.revision }).block_task().await?;
                assert!(matches!(stale.result, SkillManagementResult::Error { code, .. } if code == "stale_revision"));
                connection.send_request(SkillsConfigWriteRequest { version: 1, session_id: id.clone(), expected_revision: revision, name: name.clone(), enabled: true }).block_task().await?;
                let restored = list(&connection, &id).await;
                assert_eq!(restored.entries[0].digest, digest);
                assert_eq!(restored.counts.active, 1);
                let retry = connection.send_request(SkillInvokeRequest { version: 1, session_id: id.clone(), name: restored.completions[0].name.clone(), args: vec![ContentBlock::from("after rejection")] }).block_task().await?;
                assert_eq!(retry.stop_reason, StopReason::EndTurn);
                assert!(connection.send_request(SkillsListRequest { version: 2, session_id: id.clone(), query: String::new() }).block_task().await.is_err(), "v2 is rejected without an adapter");
                for version in [0, 3, 99] {
                    let error = connection.send_request(SkillsListRequest { version, session_id: id.clone(), query: String::new() }).block_task().await.unwrap_err();
                    assert!(error.data.unwrap().to_string().contains("expected 1"));
                }
                assert!(connection.send_request(SkillsListRequest { version: 1, session_id: "unknown".into(), query: String::new() }).block_task().await.is_err());
                connection.send_request(CloseSessionRequest::new(id)).block_task().await?;
                connection.send_request(CloseSessionRequest::new(second.session_id)).block_task().await?;
                Ok(())
            }
        });
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(changes.lock().unwrap().len(), 2);
    assert!(
        changes
            .lock()
            .unwrap()
            .iter()
            .all(|notice| notice.version == 1)
    );
    assert!(
        serde_json::to_string(&*updates.lock().unwrap())
            .unwrap()
            .contains("-status-rejection")
    );
    assert!(
        !serde_json::to_string(&*changes.lock().unwrap())
            .unwrap()
            .contains("PRIVATE MAIN BODY")
    );
}

#[tokio::test]
async fn skill_invocation_uses_actual_plan_mode_ready_revision_and_cancellation() {
    let workspace = tempfile::tempdir().unwrap();
    let factory = Arc::new(FakeFactory::new());
    let (client_transport, server_transport) = Channel::duplex();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            factory,
            server_transport,
        )
        .await
    });
    let client = Client
        .builder()
        .on_receive_notification(
            async |_notification: SessionNotification, _connection| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                let id = created.session_id;
                connection
                    .send_request(SetSessionModeRequest::new(id.clone(), "plan"))
                    .block_task()
                    .await?;
                let name = list(&connection, &id).await.completions[0].name.clone();
                let result = connection
                    .send_request(SkillInvokeRequest {
                        version: 1,
                        session_id: id.clone(),
                        name: name.clone(),
                        args: vec![ContentBlock::from("make plan")],
                    })
                    .block_task()
                    .await?;
                assert_eq!(result.version, 1);
                assert_eq!(result.stop_reason, StopReason::EndTurn);
                // If the typed call used Build mode, the fake provider would
                // not submit a Plan and this mode change would be accepted.
                assert!(
                    connection
                        .send_request(SetSessionModeRequest::new(id.clone(), "build"))
                        .block_task()
                        .await
                        .is_err()
                );
                let revised = connection
                    .send_request(SkillInvokeRequest {
                        version: 1,
                        session_id: id.clone(),
                        name: name.clone(),
                        args: vec![ContentBlock::from("revision input")],
                    })
                    .block_task()
                    .await?;
                assert_eq!(revised.stop_reason, StopReason::EndTurn);
                connection
                    .send_request(SetSessionModeRequest::new(id.clone(), "build"))
                    .block_task()
                    .await?;
                let pending = connection.send_request(SkillInvokeRequest {
                    version: 1,
                    session_id: id.clone(),
                    name,
                    args: vec![ContentBlock::from("wait")],
                });
                // A read is legal while the tracked typed prompt is in flight.
                let page = list(&connection, &id).await;
                assert!(
                    connection
                        .send_request(SkillsReloadRequest {
                            version: 1,
                            session_id: id.clone(),
                            expected_revision: page.revision
                        })
                        .block_task()
                        .await
                        .is_err()
                );
                connection.send_notification(CancelNotification::new(id.clone()))?;
                assert_eq!(
                    pending.block_task().await?.stop_reason,
                    StopReason::Cancelled
                );
                connection
                    .send_request(PromptRequest::new(
                        id.clone(),
                        vec![ContentBlock::from("after cancel")],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new(id))
                    .block_task()
                    .await?;
                Ok(())
            }
        });
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_zevria/skills/list", response = serde_json::Value)]
struct RootOverrideRequest {
    version: u32,
    #[serde(rename = "sessionId")]
    session_id: agent_client_protocol::schema::v1::SessionId,
    roots: Vec<String>,
}

#[tokio::test]
async fn sdk_underscore_dispatch_rejects_unknown_root_parameters() {
    let workspace = tempfile::tempdir().unwrap();
    let (client_transport, server_transport) = Channel::duplex();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            Arc::new(FakeFactory::new()),
            server_transport,
        )
        .await
    });
    let client = Client.builder().connect_with(client_transport, {
        let workspace = workspace.path().to_path_buf();
        async move |connection| {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let created = connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await?;
            let error = connection
                .send_request(RootOverrideRequest {
                    version: 1,
                    session_id: created.session_id.clone(),
                    roots: vec!["/tmp/arbitrary".into()],
                })
                .block_task()
                .await
                .unwrap_err();
            assert_eq!(serde_json::to_value(error).unwrap()["code"], -32602);
            let page = list(&connection, &created.session_id).await;
            assert_eq!(
                page.entries.len(),
                1,
                "connection remains usable after invalid params"
            );
            connection
                .send_request(CloseSessionRequest::new(created.session_id))
                .block_task()
                .await?;
            Ok(())
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
