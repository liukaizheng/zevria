//! Owns Side Chat forks and their independently cancellable engines.
use anyhow::Context as _;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::{Id, JoinSet},
};
use tokio_util::sync::CancellationToken;
use zevria_content::UserPrompt;
use zevria_core::{ParentForkHandle, SessionEngine, SideChatReadOnlyConfig};
use zevria_foundation::{
    SIDE_CHAT_EXCERPT_BYTES, SideChatDescriptor, SideChatId, SideChatOrigin, SideChatStatus,
    bound_side_chat_text,
};
use zevria_model::models::SessionModels;
use zevria_provider::{ProviderRouter, ResolvedModelProfile};
use zevria_session_api::{
    ControlCommand, SessionCommand, SessionEvent, SessionEventSender, SessionUpdate,
    SideChatCommand, SideChatEvent, TurnCommand, TurnEvent, session_event_channel,
};
use zevria_transcript::{
    SideChatFork, TranscriptItem, TranscriptLoadOutcome, TranscriptWriter, transcript,
};

/// A bounded, post-fork UI projection. The supervisor separately owns the
/// validated load report, including the complete inherited model prefix.
pub struct RestoredSideChat {
    pub origin: SideChatOrigin,
    pub descriptor: SideChatDescriptor,
    pub items: Vec<TranscriptItem>,
}

#[derive(Clone)]
pub(crate) struct SideChatServices {
    pub workspace: PathBuf,
    pub profiles: Vec<ResolvedModelProfile>,
    pub models: SessionModels,
    pub command: crate::config::CommandConfig,
    pub web_search: Option<Arc<dyn zevria_session_api::WebSearchExecutor>>,
    pub mcp: Option<Arc<zevria_mcp::McpManager>>,
    pub mcp_tools: rig_agent::tool::server::ToolServerHandle,
}

#[derive(Clone)]
enum History {
    Fork(Arc<Vec<TranscriptItem>>),
    Restored(Arc<TranscriptLoadOutcome>),
    /// Load the new durable suffix when a previously open engine is reopened.
    Reopen(Option<Arc<Vec<TranscriptItem>>>),
}

struct Thread {
    fork: SideChatFork,
    history: History,
    live: Option<Live>,
}

struct Live {
    commands: mpsc::UnboundedSender<SessionCommand>,
    active: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    closing: bool,
    /// Receiver closed before its actor finished tearing down the writer.
    reopen_prompt: Option<UserPrompt>,
}

pub(crate) type Restore = (RestoredSideChat, Arc<TranscriptLoadOutcome>);

pub(crate) fn read_restored(path: &Path, root_id: &str, file_id: &str) -> anyhow::Result<Restore> {
    let report = transcript::load_report(path)?;
    report.ensure_resumable()?;
    let Some(TranscriptItem::SideChatOrigin(fork)) = report.items().first() else {
        anyhow::bail!("missing Side Chat origin in {}", path.display());
    };
    anyhow::ensure!(
        fork.origin.parent_session_id == root_id && fork.origin.id.to_string() == file_id,
        "Side Chat transcript identity does not match {}",
        path.display()
    );
    let after = report
        .items()
        .iter()
        .skip_while(|item| !matches!(item, TranscriptItem::SideChatForkBoundary))
        .skip(1);
    let descriptor = fork.origin.descriptor(
        after
            .clone()
            .filter(|item| transcript::is_prompt_item(item))
            .count(),
    );
    Ok((
        RestoredSideChat {
            origin: fork.origin.clone(),
            descriptor,
            items: after.cloned().collect(),
        },
        Arc::new(report),
    ))
}

pub(crate) struct SideChatSupervisor {
    pub(crate) stop: CancellationToken,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

async fn emit(events: &SessionEventSender, stop: &CancellationToken, event: SideChatEvent) -> bool {
    tokio::select! {
        biased;
        () = stop.cancelled() => false,
        result = events.send(SessionEvent::SideChat(event)) => result.is_ok(),
    }
}

async fn fork_thread(
    mut parent: ParentForkHandle,
    models: SessionModels,
    prompt: Option<&UserPrompt>,
    excerpt: Option<String>,
) -> anyhow::Result<Thread> {
    let mut retries = 0;
    let (meta, mut items) = loop {
        let current = parent
            .borrow_and_update()
            .clone()
            .context("main session is not ready to fork")?;
        anyhow::ensure!(
            !current.persistence_degraded,
            "repair main session persistence before forking"
        );
        let snapshot = current.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            transcript::load_prefix_items(&snapshot.transcript_path, snapshot.safe_len)
        })
        .await?;
        if current.still_current() {
            break (current, loaded?);
        }
        anyhow::ensure!(
            retries == 0,
            "main history changed during the fork; try again"
        );
        retries += 1;
        tokio::time::timeout(Duration::from_secs(5), parent.changed()).await??;
    };
    anyhow::ensure!(
        !matches!(items.first(), Some(TranscriptItem::SideChatOrigin(_))),
        "cannot fork another Side Chat"
    );
    let selection = meta
        .model
        .as_ref()
        .context("no model selected for the active main session mode")?;
    let quoted_excerpt = excerpt
        .as_deref()
        .map(|text| bound_side_chat_text(text, SIDE_CHAT_EXCERPT_BYTES))
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let preview = prompt.map(UserPrompt::text_projection);
    let title = preview
        .as_deref()
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .or_else(|| {
            quoted_excerpt
                .as_deref()
                .and_then(|text| text.lines().next())
        })
        .unwrap_or("New side chat");
    let fork = SideChatFork {
        origin: SideChatOrigin {
            version: 1,
            id: SideChatId::random(),
            parent_session_id: meta.session_id.clone(),
            title: bound_side_chat_text(title, 160).to_owned(),
            forked_mode: meta.mode,
            forked_model_role: meta.policy.model_role,
            forked_profile: selection.profile.clone(),
            forked_reasoning: selection.reasoning_level,
            inherited_turn_count: meta.inherited_turn_count,
            quoted_excerpt,
        },
        instructions: meta.instructions.to_string(),
        initial_tools: (*meta.initial_tools).clone(),
        skills_enabled: meta.skills_enabled,
    };
    items.insert(0, TranscriptItem::SideChatOrigin(Box::new(fork.clone())));
    if meta.safe_len == 0 {
        items.push(TranscriptItem::SessionModels(Box::new(
            meta.session_models.clone().unwrap_or(models),
        )));
        items.push(TranscriptItem::SessionMode(meta.mode));
    }
    items.push(TranscriptItem::SideChatForkBoundary);
    let items = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let input = transcript::model_input(&items);
        anyhow::ensure!(
            zevria_model::compaction::replay::replay_safe_prefixes(&input)?.last() == Some(&true),
            "the parent prefix has unpaired provider tool calls"
        );
        zevria_transcript::validate_session_replay(&items)?;
        Ok(items)
    })
    .await??;
    Ok(Thread {
        fork,
        history: History::Fork(Arc::new(items)),
        live: None,
    })
}

pub(crate) async fn run(
    parent: ParentForkHandle,
    mut requests: mpsc::UnboundedReceiver<SideChatCommand>,
    events: SessionEventSender,
    services: SideChatServices,
    restored: Vec<Arc<TranscriptLoadOutcome>>,
    limit: usize,
    stop: CancellationToken,
) {
    let capacity = Arc::new(Semaphore::new(limit));
    let mut threads: HashMap<_, _> = restored
        .into_iter()
        .filter_map(|history| {
            let TranscriptItem::SideChatOrigin(fork) = history.items().first()? else {
                return None;
            };
            Some((
                fork.origin.id,
                Thread {
                    fork: (**fork).clone(),
                    history: History::Restored(history),
                    live: None,
                },
            ))
        })
        .collect();
    let mut actors = JoinSet::new();
    let mut task_ids: HashMap<Id, SideChatId> = HashMap::new();
    let mut forks: JoinSet<(String, Option<UserPrompt>, anyhow::Result<Thread>)> = JoinSet::new();
    let mut fork_requests: HashMap<Id, String> = HashMap::new();
    loop {
        let command = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            result = actors.join_next_with_id(), if !actors.is_empty() => {
                let (task, failed, error) = match result {
                    Some(Ok((task, failed))) => (task, failed, None),
                    Some(Err(error)) => (error.id(), true, Some(error)),
                    None => continue,
                };
                let Some(id) = task_ids.remove(&task) else { continue; };
                let Some(thread) = threads.get_mut(&id) else { continue; };
                // Dropping Live also drops the actor's last permit on unwind.
                let retry = thread.live.take().and_then(|live| live.reopen_prompt);
                let fallback = match &thread.history {
                    History::Fork(items) => Some(items.clone()),
                    History::Reopen(items) => items.clone(),
                    History::Restored(_) => None,
                };
                thread.history = History::Reopen(fallback);
                if let Some(error) = error {
                    tracing::warn!(%id, %error, "Side Chat actor stopped unexpectedly");
                    let _ = emit(&events, &stop, SideChatEvent::Failed { id, error: "The Side Chat stopped unexpectedly. Submit again to reopen it.".into() }).await;
                    continue;
                }
                let Some(text) = retry else {
                    if !failed {
                        // Emit Unloaded only after `live` is cleared so an immediate
                        // reopen sees a settled thread instead of `live.closing`.
                        let _ = emit(&events, &stop, SideChatEvent::Unloaded { id }).await;
                    }
                    continue;
                };
                SideChatCommand::Submit { id, text }
            }
            result = forks.join_next_with_id(), if !forks.is_empty() => {
                let (task, (request_id, prompt, result)) = match result {
                    Some(Ok(result)) => result,
                    Some(Err(error)) => {
                        if let Some(request_id) = fork_requests.remove(&error.id()) {
                            tracing::warn!(%error, "Side Chat fork task stopped unexpectedly");
                            let _ = emit(&events, &stop, SideChatEvent::CreateFailed { request_id, error: "Could not finish creating the Side Chat. Try again.".into() }).await;
                        }
                        continue;
                    }
                    None => continue,
                };
                fork_requests.remove(&task);
                match result {
                    Ok(thread) => {
                        let id = thread.fork.origin.id;
                        let origin = &thread.fork.origin;
                        if !emit(&events, &stop, SideChatEvent::Created { request_id,
                            descriptor: Box::new(origin.descriptor(0)), quoted_excerpt: origin.quoted_excerpt.clone(), awaiting_initial_turn: prompt.is_some(), }).await { break; }
                        threads.insert(id, thread);
                        let Some(text) = prompt else { continue; };
                        SideChatCommand::Submit { id, text }
                    }
                    Err(error) => {
                        if !emit(&events, &stop, SideChatEvent::CreateFailed { request_id, error: format!("{error:#}") }).await { break; }
                        continue;
                    }
                }
            }
            command = requests.recv() => match command { Some(command) => command, None => break },
        };
        match command {
            SideChatCommand::Create {
                request_id,
                initial_prompt,
                quoted_excerpt,
            } => {
                let parent = parent.clone();
                let models = services.models.clone();
                let correlation = request_id.clone();
                let task = forks.spawn(async move {
                    let result =
                        fork_thread(parent, models, initial_prompt.as_ref(), quoted_excerpt).await;
                    (request_id, initial_prompt, result)
                });
                fork_requests.insert(task.id(), correlation);
            }
            SideChatCommand::Submit { id, text } => {
                let Some(thread) = threads.get_mut(&id) else {
                    reject_submit(&events, &stop, id, "This Side Chat no longer exists.").await;
                    continue;
                };
                if let Some(live) = thread.live.as_mut() {
                    if live.closing {
                        reject_submit(
                            &events,
                            &stop,
                            id,
                            "This Side Chat is closing. Retry when it has stopped.",
                        )
                        .await;
                        continue;
                    }
                    if live.reopen_prompt.is_some() || live.active.lock().unwrap().is_some() {
                        reject_submit(&events, &stop, id, "This Side Chat is already answering.")
                            .await;
                        continue;
                    }
                    if live.commands.is_closed() {
                        // Wait for the old actor to release its transcript writer.
                        live.reopen_prompt = Some(text);
                        continue;
                    }
                }
                let Ok(permit) = capacity.clone().try_acquire_owned() else {
                    reject_submit(
                        &events,
                        &stop,
                        id,
                        "All Side Chat slots are busy. Retry when a thread finishes.",
                    )
                    .await;
                    continue;
                };
                let submission = SessionCommand::Turn(TurnCommand::Submit {
                    text,
                    mode: thread.fork.origin.forked_mode,
                    behavior: zevria_foundation::RequestBehavior::SideChat,
                    references: Vec::new(),
                });
                let admitted = if let Some(live) = thread.live.as_mut() {
                    // The relay releases under the same lock, including a fast rejection.
                    let mut active = live.active.lock().unwrap();
                    let sent = live.commands.send(submission);
                    sent.is_ok().then(|| *active = Some(permit)).is_some()
                } else {
                    // Queue the opening turn and install its permit before spawning so a
                    // fast `build_engine` failure cannot race `send` and emit a second error.
                    let (commands, rx) = mpsc::unbounded_channel();
                    commands
                        .send(submission)
                        .expect("fresh Side Chat command receiver is still held");
                    let active = Arc::new(Mutex::new(Some(permit)));
                    let task = actors.spawn(run_engine(
                        thread.fork.clone(),
                        thread.history.clone(),
                        rx,
                        active.clone(),
                        services.clone(),
                        parent.clone(),
                        events.clone(),
                        stop.clone(),
                    ));
                    task_ids.insert(task.id(), id);
                    thread.live = Some(Live {
                        commands,
                        active,
                        closing: false,
                        reopen_prompt: None,
                    });
                    true
                };
                if !admitted {
                    reject_submit(&events, &stop, id, "The Side Chat stopped before accepting the message. Submit again to reopen it.").await;
                }
            }
            SideChatCommand::Cancel { id } => {
                if let Some(live) = threads.get_mut(&id).and_then(|thread| thread.live.as_mut()) {
                    let cancelled_pending = live.reopen_prompt.take().is_some();
                    let _ =
                        live.commands
                            .send(SessionCommand::Control(ControlCommand::CancelTurn {
                                turn_id: None,
                            }));
                    if cancelled_pending {
                        reject_submit(
                            &events,
                            &stop,
                            id,
                            "The Side Chat turn was cancelled before it started.",
                        )
                        .await;
                    }
                }
            }
            SideChatCommand::Close { id } => {
                if let Some(live) = threads.get_mut(&id).and_then(|thread| thread.live.as_mut()) {
                    live.closing = true;
                    let discarded = live.reopen_prompt.take().is_some();
                    let _ = live
                        .commands
                        .send(SessionCommand::Control(ControlCommand::Shutdown));
                    if discarded {
                        reject_submit(
                            &events,
                            &stop,
                            id,
                            "The Side Chat was closed before the message could be sent.",
                        )
                        .await;
                    }
                }
            }
        }
    }
    forks.abort_all();
    stop.cancel();
    for thread in threads.values() {
        if let Some(live) = &thread.live {
            let _ = live
                .commands
                .send(SessionCommand::Control(ControlCommand::Shutdown));
        }
    }
    while actors.join_next().await.is_some() {}
}

#[allow(clippy::too_many_arguments)]
async fn run_engine(
    fork: SideChatFork,
    history: History,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    active: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
    services: SideChatServices,
    parent: ParentForkHandle,
    events: SessionEventSender,
    stop: CancellationToken,
) -> bool {
    let id = fork.origin.id;
    let catalog = parent
        .borrow()
        .as_ref()
        .map(|meta| meta.skill_catalog.clone())
        .unwrap_or_default();
    let opening =
        tokio::task::spawn_blocking(move || build_engine(fork, history, services, catalog)).await;
    match opening {
        Ok(Ok(engine)) => {
            let (tx, mut rx) = session_event_channel(32);
            let mut failed = false;
            let relay = async {
                while let Some(update) = rx.recv().await {
                    match update {
                        SessionUpdate::Streams(batch) => {
                            if !stop.is_cancelled()
                                && let Some(stream) = batch.root
                            {
                                events.set_side_chat_stream(id, stream);
                            }
                        }
                        SessionUpdate::Lifecycle(event) => {
                            let status = match &event {
                                SessionEvent::Turn(TurnEvent::Started { .. }) => {
                                    Some(SideChatStatus::Running)
                                }
                                SessionEvent::Turn(
                                    TurnEvent::Completed { .. } | TurnEvent::Cancelled { .. },
                                ) => Some(SideChatStatus::Idle),
                                SessionEvent::Turn(
                                    TurnEvent::Rejected { .. } | TurnEvent::Failed { .. },
                                ) => Some(SideChatStatus::Failed),
                                _ => None,
                            };
                            if status.is_some_and(|status| status != SideChatStatus::Running) {
                                active.lock().unwrap().take();
                            }
                            let _ = emit(
                                &events,
                                &stop,
                                SideChatEvent::Session {
                                    id,
                                    event: Box::new(event),
                                },
                            )
                            .await;
                            if let Some(status) = status {
                                let _ = emit(&events, &stop, SideChatEvent::Status { id, status })
                                    .await;
                            }
                        }
                    }
                }
            };
            let (result, ()) = tokio::join!(engine.run(commands, tx), relay);
            let lost_turn = active.lock().unwrap().take().is_some();
            if result.is_err() || lost_turn {
                failed = true;
                let error = result.err().map_or_else(
                    || "The Side Chat stopped before finishing the submitted turn.".into(),
                    |error| error.to_string(),
                );
                let _ = emit(&events, &stop, SideChatEvent::Failed { id, error }).await;
            }
            failed
        }
        result => {
            // A retry can arrive as soon as the failure is published. Release the
            // slot and close this actor's inbox before letting it be retried.
            drop(commands);
            active.lock().unwrap().take();
            let error = match result {
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => error.to_string(),
                _ => unreachable!(),
            };
            let _ = emit(&events, &stop, SideChatEvent::Failed { id, error }).await;
            true
        }
    }
}

async fn reject_submit(
    events: &SessionEventSender,
    stop: &CancellationToken,
    id: SideChatId,
    error: &str,
) {
    let _ = emit(
        events,
        stop,
        SideChatEvent::SubmitRejected {
            id,
            error: error.into(),
        },
    )
    .await;
}

fn build_engine(
    fork: SideChatFork,
    history: History,
    services: SideChatServices,
    catalog: Arc<zevria_instructions::SkillCatalog>,
) -> anyhow::Result<SessionEngine<ProviderRouter>> {
    let origin = &fork.origin;
    let directory = transcript::side_chats_dir(&services.workspace, &origin.parent_session_id);
    let id = origin.id.to_string();
    let path = directory.join(format!("{id}.jsonl"));
    let profile = services
        .profiles
        .into_iter()
        .find(|profile| profile.profile == origin.forked_profile)
        .context("the Side Chat's inherited model profile is no longer configured")?;
    let role = origin.forked_model_role;
    let context = profile.context_policy();
    // `instructions` already contains the complete application and guidance.
    let provider =
        ProviderRouter::from_routes([(role, profile, origin.forked_reasoning)], "", &id)?
            .with_prompt_cache_session_id(&origin.parent_session_id);
    #[cfg(feature = "cache-diagnostics")]
    let provider =
        provider.with_cache_diagnostics(zevria_provider::CacheDiagnosticContext::new(&path, &id));
    let tools = crate::runtime::tools::build_side_chat_tools(
        &services.workspace,
        services.command,
        services.web_search,
    )?;
    let mut policies = crate::runtime::tools::build_session_policies(
        zevria_workflow::config::PlanConfig::default(),
        false,
    );
    policies.policy_mut(origin.forked_mode).model_role = role;
    let config = SideChatReadOnlyConfig {
        quoted_excerpt: origin.quoted_excerpt.clone(),
        skills_enabled: fork.skills_enabled,
        mode: origin.forked_mode,
        instructions: Arc::from(fork.instructions),
        frozen_tools: Arc::new(fork.initial_tools),
        mcp_tools: services.mcp.as_ref().map(|_| services.mcp_tools),
        mcp: services
            .mcp
            .map(|mcp| mcp as Arc<dyn zevria_session_api::mcp::McpSessionService>),
    };
    let history = match history {
        History::Reopen(_) if path.is_file() => {
            History::Restored(Arc::new(transcript::load_report(&path)?))
        }
        History::Reopen(Some(items)) => History::Fork(items),
        history => history,
    };
    let builder = match history {
        History::Restored(report) => SessionEngine::builder_from_loaded(
            provider,
            tools,
            policies,
            TranscriptWriter::append_loaded(&report)?,
            catalog,
            Arc::unwrap_or_clone(report),
        )?,
        History::Fork(items) | History::Reopen(Some(items)) => SessionEngine::builder(
            provider,
            tools,
            policies,
            TranscriptWriter::allocate_with_id(&directory, &id)?,
            catalog,
        )?
        .with_transcript_items(Arc::unwrap_or_clone(items))?,
        History::Reopen(None) => anyhow::bail!("Side Chat transcript is missing"),
    };
    Ok(builder
        .with_read_only_side_chat(config)
        .with_compaction_policy(zevria_model::CompactionPolicy::default().with_role(role, context)?)
        .build()?)
}
