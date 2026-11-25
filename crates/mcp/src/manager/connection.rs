//! Transport admission, catalog refresh, and connection shutdown.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
use rmcp::{
    RoleClient,
    model::{ProtocolVersion, SubscriptionFilterBuilder},
    service::Peer,
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use zevria_foundation::{McpServerName, McpServerOrigin, McpServerStatus};

use super::{McpManager, state::Entry};
use crate::{
    catalog::Catalog,
    config::{McpConfig, McpServerConfig},
    transport::{self, Service, StderrTail},
};

pub(super) struct Connection {
    pub(super) peer: Peer<RoleClient>,
    pub(super) stop: CancellationToken,
    _stop_on_drop: DropGuard,
    pub(super) closed: CancellationToken,
    pub(super) staged: Arc<Mutex<Option<Catalog>>>,
}

pub(super) fn connection_live(
    stop: &CancellationToken,
    closed: &CancellationToken,
    peer: &Peer<RoleClient>,
) -> bool {
    !stop.is_cancelled() && !closed.is_cancelled() && !peer.is_transport_closed()
}

impl McpManager {
    pub(super) async fn open_servers<'a>(
        &self,
        global: &McpConfig,
        servers: impl IntoIterator<Item = (&'a McpServerName, &'a McpServerConfig, McpServerOrigin)>,
    ) -> BTreeMap<McpServerName, Entry> {
        join_all(servers.into_iter().map(|(name, spec, origin)| async move {
            (
                name.clone(),
                self.open(name.clone(), spec.clone(), origin, global).await,
            )
        }))
        .await
        .into_iter()
        .collect()
    }

    pub(super) async fn open(
        &self,
        name: McpServerName,
        config: McpServerConfig,
        origin: McpServerOrigin,
        global: &McpConfig,
    ) -> Entry {
        let mut entry = Entry {
            server: name,
            origin,
            config,
            status: McpServerStatus::Disabled,
            stderr: StderrTail::default(),
            catalog: Catalog::default(),
            wire_names: BTreeMap::new(),
            connection: None,
            connect_ms: None,
        };
        if !global.enabled || !entry.config.enabled || self.stop.is_cancelled() {
            return entry;
        }
        if entry.config.kind() == crate::config::McpTransportKind::Stdio {
            entry.stderr = StderrTail::new(&entry.config);
        }
        let (refresh, rx) = mpsc::channel(1);
        let start = Instant::now();
        let startup_timeout = entry.config.startup_timeout(global);
        let deadline = start + startup_timeout;
        let stop = self.stop.child_token();
        let opening = timeout_at(deadline, async {
            let service = transport::connect(
                &entry.server,
                &entry.config,
                &self.workspace,
                self.root.clone(),
                refresh.clone(),
                entry.stderr.clone(),
                &self.http,
            )
            .await?;
            let catalog = Catalog::fetch(
                &entry.server,
                service.peer(),
                deadline.saturating_duration_since(Instant::now()),
                &stop,
            )
            .await?;
            Ok::<_, anyhow::Error>((service, catalog))
        });
        // Dropping the handshake drops the transport; cancellation does not
        // have to wait for the server or hold the mutation lock until timeout.
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => return entry,
            result = opening => result,
        };
        entry.connect_ms = Some(start.elapsed().as_millis() as u64);
        match result {
            Ok(Ok((service, catalog))) => {
                entry.set_catalog(catalog);
                entry.status = McpServerStatus::Connected;
                entry.connection = Some(supervise(
                    service,
                    entry.server.clone(),
                    rx,
                    refresh,
                    stop,
                    startup_timeout,
                    self.changes.clone(),
                ));
            }
            failure => {
                let error = match failure {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => format!(
                        "MCP startup timed out after {} seconds",
                        startup_timeout.as_secs()
                    ),
                    _ => unreachable!(),
                };
                entry.status = McpServerStatus::Failed {
                    error,
                    stderr_tail: None,
                };
            }
        }
        entry
    }

    pub async fn shutdown(&self, grace: Duration) {
        {
            // Linearize cancellation against the state-locked mutation commit,
            // without waiting for a mutation that is still opening connections.
            let _state = self.state.write().unwrap();
            self.stop.cancel();
        }
        let _operation = self.mutations.lock().await;
        let conns = {
            let mut state = self.state.write().unwrap();
            let conns = state
                .entries
                .values_mut()
                .filter_map(|entry| entry.connection.take())
                .collect::<Vec<_>>();
            state.wire_index.clear();
            state.resource_access.clear();
            conns
        };
        self.publish_registry();
        close_connections(
            conns
                .iter()
                .map(|conn| (conn.stop.clone(), conn.closed.clone())),
            grace,
        )
        .await;
    }

    pub(super) async fn reconnect(
        &self,
        global: &McpConfig,
        name: McpServerName,
        config: McpServerConfig,
        origin: McpServerOrigin,
    ) -> Entry {
        // Keep the old entry visible until replacement. If this operation is
        // cancelled, its stopped connection can still be retired by `sync`.
        let old = self
            .state
            .read()
            .unwrap()
            .entries
            .get(&name)
            .and_then(|entry| entry.connection.as_ref())
            .map(|conn| (conn.stop.clone(), conn.closed.clone()));
        close_connections(old, Duration::from_secs(5)).await;
        self.open(name, config, origin, global).await
    }
}

pub(super) async fn close_connections(
    conns: impl IntoIterator<Item = (CancellationToken, CancellationToken)>,
    grace: Duration,
) {
    let conns: Vec<_> = conns.into_iter().collect();
    for (stop, _) in &conns {
        stop.cancel();
    }
    let _ = timeout(
        grace,
        join_all(conns.iter().map(|(_, closed)| closed.cancelled())),
    )
    .await;
}

pub(super) fn signal(changes: &watch::Sender<u64>) {
    changes.send_modify(|version| *version = version.wrapping_add(1));
}

fn supervise(
    service: Service,
    server: McpServerName,
    mut updates: mpsc::Receiver<()>,
    refresh: mpsc::Sender<()>,
    stop: CancellationToken,
    limit: Duration,
    changes: watch::Sender<u64>,
) -> Connection {
    let conn = Connection {
        peer: service.peer().clone(),
        stop: stop.clone(),
        _stop_on_drop: stop.clone().drop_guard(),
        closed: CancellationToken::new(),
        staged: Arc::new(Mutex::new(None)),
    };
    let (peer, staged, closed) = (conn.peer.clone(), conn.staged.clone(), conn.closed.clone());
    tokio::spawn(async move {
        let done = closed.drop_guard();
        let token = service.cancellation_token();
        let subscriptions = listen(&peer, refresh);
        let refresh_catalog = async {
            while updates.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                while updates.try_recv().is_ok() {}
                match Catalog::fetch(&server, &peer, limit, &stop).await {
                    Ok(catalog) => {
                        *staged.lock().unwrap() = Some(catalog);
                        signal(&changes);
                    }
                    Err(error) if !stop.is_cancelled() => {
                        tracing::warn!(target: "zevria::mcp", server = %server, %error, "could not refresh catalog; keeping last snapshot")
                    }
                    Err(_) => {}
                }
            }
        };
        let jobs = async {
            tokio::join!(subscriptions, refresh_catalog);
        };
        let waiting = service.waiting();
        tokio::pin!(waiting);
        let mut exited = false;
        tokio::select! {
            _ = &mut waiting => { exited = true; }
            _ = stop.cancelled() => {}
            _ = jobs => {}
        }
        if !exited {
            token.cancel();
            let _ = waiting.await;
        }
        drop(done);
        signal(&changes);
    });
    conn
}

async fn listen(peer: &Peer<RoleClient>, refresh: mpsc::Sender<()>) {
    let Some(info) = peer.peer_info() else {
        return;
    };
    if info.protocol_version < ProtocolVersion::V_2026_07_28 {
        return;
    }
    let mut filter = SubscriptionFilterBuilder::default();
    let mut enabled = false;
    if info
        .capabilities
        .tools
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.tools_list_changed();
        enabled = true;
    }
    if info
        .capabilities
        .resources
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.resources_list_changed();
        enabled = true;
    }
    if info
        .capabilities
        .prompts
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.prompts_list_changed();
        enabled = true;
    }
    if !enabled {
        return;
    }
    if let Ok(Ok(mut stream)) = timeout(Duration::from_secs(10), peer.listen(filter.build())).await
    {
        // Close the discovery-to-subscribe race with a fresh snapshot.
        let _ = refresh.try_send(());
        while let Ok(Some(_)) = stream.next().await {
            let _ = refresh.try_send(());
        }
    }
}
