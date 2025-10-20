#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! Experimental headless driver for the Codex agent.
//!
//! The driver exposes a channel-based interface that allows non-TUI clients
//! to submit prompts and stream events from the Codex agent. The current
//! implementation wires together configuration/authentication bootstrap and
//! prepares communication channels; subsequent tickets will attach the full
//! event pipeline.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use codex_common::CliConfigOverrides;
use codex_core::AuthManager;
use codex_core::BUILT_IN_OSS_MODEL_PROVIDER_ID;
use codex_core::CodexConversation;
use codex_core::ConversationManager;
use codex_core::NewConversation;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::AgentReasoningSectionBreakEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::ExecCommandOutputDeltaEvent;
use codex_core::protocol::ExecOutputStream;
use codex_core::protocol::FileChange;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::PatchApplyEndEvent;
use codex_core::protocol::ReviewDecision;
use codex_core::protocol::SessionConfiguredEvent;
use codex_core::protocol::StreamErrorEvent;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenCountEvent;
use codex_core::protocol::TokenUsageInfo;
use codex_core::protocol::TurnAbortedEvent;
use codex_core::protocol::TurnDiffEvent;
use codex_core::protocol::WebSearchBeginEvent;
use codex_core::protocol::WebSearchEndEvent;
use codex_ollama::DEFAULT_OSS_MODEL;
use codex_protocol::ConversationId;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use crossbeam_channel::Receiver as CommandReceiver;
use crossbeam_channel::Receiver as EventReceiver;
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender as CommandSender;
use crossbeam_channel::Sender as EventSender;
use crossbeam_channel::{self};
use mcp_types::CallToolResult;
use thiserror::Error;
use tokio::runtime::Builder;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::{self};

/// Public commands that a client may send to the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCommand {
    /// Submit a user message to the active Codex session.
    SubmitUserMessage {
        /// Primary text content for the user message.
        text: String,
        /// Optional image paths to include alongside the text input.
        images: Vec<PathBuf>,
    },
    /// Request that the current Codex turn be interrupted.
    Interrupt,
    /// Shut down the running Codex session.
    Shutdown,
    /// Resume from an existing rollout, replacing the current conversation.
    Resume {
        /// Path to the rollout archive.
        rollout_path: PathBuf,
    },
    /// Respond to an approval request raised by the agent.
    RespondApproval {
        /// Submission identifier associated with the approval request.
        id: String,
        /// Identifies which subsystem is awaiting approval.
        kind: ApprovalKind,
        /// The user's decision.
        decision: ReviewDecision,
    },
}

/// Identifies the subsystem that requested approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Approval for a command execution.
    Exec,
    /// Approval for applying a patch.
    Patch,
}

/// Events emitted by the driver while a session is active.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// Session configuration payload describing the active model.
    SessionConfigured {
        /// Active model slug for the session.
        model: String,
        /// Unique identifier for the conversation.
        conversation_id: ConversationId,
        /// Path to the rollout backing this conversation.
        rollout_path: PathBuf,
    },
    /// Streaming agent response text.
    AgentMessageDelta(String),
    /// Final agent response for the current turn.
    AgentMessageFinal(String),
    /// Streaming reasoning text from the agent.
    AgentReasoningDelta(String),
    /// Final reasoning block from the agent.
    AgentReasoningFinal(String),
    /// Notification that an exec command is starting.
    ExecCommandBegin {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Command argv proposed by the agent.
        command: Vec<String>,
    },
    /// Completion notice for an exec command.
    ExecCommandEnd {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Exit code if the command completed.
        exit_code: Option<i32>,
    },
    /// Streaming chunk from an exec command's stdout/stderr.
    ExecOutputChunk {
        /// Identifier correlating output chunks with the originating command.
        call_id: String,
        /// Which stream emitted this chunk.
        stream: ExecOutputStream,
        /// The decoded text chunk.
        text: String,
    },
    /// Notification that a patch apply is beginning.
    PatchApplyBegin {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Whether the patch was auto-approved by policy.
        auto_approved: bool,
    },
    /// Completion notice for a patch apply operation.
    PatchApplyEnd {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Indicates whether the patch applied cleanly.
        success: bool,
    },
    /// Start of an MCP tool invocation.
    McpToolCallBegin {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// MCP server identifier.
        server: String,
        /// Tool name invoked on the server.
        tool: String,
    },
    /// Completion of an MCP tool invocation.
    McpToolCallEnd {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// MCP server identifier.
        server: String,
        /// Tool name invoked on the server.
        tool: String,
        /// Duration of the tool invocation.
        duration: Duration,
        /// Result payload returned by the tool (error encoded as `Err`).
        result: Result<CallToolResult, String>,
    },
    /// Approval is required before continuing with an agent action.
    ApprovalRequest {
        /// Submission identifier to echo in the approval response.
        id: String,
        /// Details about the requested approval.
        request: ApprovalRequest,
    },
    /// Reasoning stream inserted a section break separator.
    ReasoningSectionBreak,
    /// Start of a web search request.
    WebSearchBegin {
        /// Identifier correlating begin/end events.
        call_id: String,
    },
    /// Completion of a web search request.
    WebSearchEnd {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// The query executed by the agent.
        query: String,
    },
    /// Token accounting for the latest turn/session.
    TokenUsage {
        /// Total tokens consumed for the aggregated session (cached + uncached).
        blended_total: u64,
        /// Non-cached input tokens.
        input: u64,
        /// Cached input tokens.
        cached_input: u64,
        /// Output tokens.
        output: u64,
        /// Reasoning tokens.
        reasoning_output: u64,
    },
    /// Task completion notification with optional final agent message.
    TurnComplete {
        /// Final agent message, if any, reported by the backend.
        last_message: Option<String>,
    },
    /// Task aborted notification with human readable reason.
    TurnAborted {
        /// Human-readable abort reason exposed by the backend.
        reason: String,
    },
    /// Unified diff summarising the latest turn's edits.
    TurnDiff {
        /// Unified diff representing the latest edits.
        unified_diff: String,
    },
    /// Updated execution plan supplied by the agent.
    PlanUpdate(PlanUpdate),
    /// Informational background message.
    BackgroundMessage(String),
    /// Stream error notification (retried internally).
    StreamError(String),
    /// Driver detected an unrecoverable error and is terminating.
    Fatal {
        /// Human-readable error suitable for logging or user display.
        message: String,
    },
    /// Session terminated in a controlled manner.
    SessionEnded,
    /// Codex backend signalled shutdown completion.
    ShutdownComplete,
}

/// Details describing the approval being requested.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalRequest {
    /// Approval for executing a shell command.
    Exec {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Command argv proposed by the agent.
        command: Vec<String>,
        /// Working directory for the command.
        cwd: PathBuf,
        /// Optional reason supplied by the backend.
        reason: Option<String>,
    },
    /// Approval for applying a patch to the workspace.
    Patch {
        /// Identifier correlating begin/end events.
        call_id: String,
        /// Map of path -> desired change.
        changes: HashMap<PathBuf, FileChange>,
        /// Optional reason supplied by the backend.
        reason: Option<String>,
        /// Optional root that the agent is asking to grant write access to.
        grant_root: Option<PathBuf>,
    },
}

/// Structured agent plan update payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUpdate {
    /// Optional explanation supplied with the plan change.
    pub explanation: Option<String>,
    /// Ordered list of plan steps.
    pub steps: Vec<PlanStep>,
}

/// Represents a single plan step emitted by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    /// Human-readable description of the step.
    pub step: String,
    /// Current status of the step.
    pub status: PlanStepStatus,
}

/// Status values for plan steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStepStatus {
    /// Step has not been started.
    Pending,
    /// Step is actively in progress.
    InProgress,
    /// Step has been completed.
    Completed,
}

/// Configuration inputs accepted by [`SessionHandle::start`].
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Optional working directory used for sandboxing and relative path
    /// resolution.
    pub cwd: Option<PathBuf>,
    /// Explicit model slug to prefer. When omitted, the configured default is
    /// used unless [`SessionConfig::use_oss`] flips the driver into OSS mode.
    pub model: Option<String>,
    /// Optional configuration profile name matching `config.toml` profiles.
    pub config_profile: Option<String>,
    /// Raw `-c` style overrides (e.g. `sandbox.mode=read-only`).
    pub raw_cli_overrides: Vec<String>,
    /// When true, prefer the built-in OSS model provider and ensure the
    /// default OSS model is available locally.
    pub use_oss: bool,
}

/// Result of successfully starting a session: the handle plus public channel ends.
pub struct SessionStartup {
    /// Handle that owns the runtime and Codex resources.
    pub handle: SessionHandle,
    /// Sender used by callers to push commands into the driver.
    pub command_tx: CommandSender<ClientCommand>,
    /// Receiver used by callers to observe streamed events.
    pub event_rx: EventReceiver<ServerEvent>,
}

impl fmt::Debug for SessionStartup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionStartup").finish()
    }
}

/// Errors that can occur while constructing a [`SessionHandle`].
#[derive(Debug, Error)]
pub enum SessionStartError {
    /// Failed to initialise the Tokio runtime.
    #[error("failed to initialise tokio runtime: {0}")]
    RuntimeInit(#[source] std::io::Error),
    /// CLI overrides failed to parse.
    #[error("invalid CLI override: {0}")]
    InvalidOverride(String),
    /// Configuration load failed.
    #[error("failed to load configuration: {0}")]
    ConfigLoad(#[source] std::io::Error),
    /// Preparing the OSS model provider failed.
    #[error("failed to prepare OSS model: {0}")]
    OssSetup(String),
    /// Failed to spawn a background bridge thread.
    #[error("failed to spawn bridge thread: {0}")]
    ThreadSpawn(String),
}

type AgentBootstrapFuture = Pin<Box<dyn Future<Output = Result<AgentContext, String>> + Send>>;

struct AgentContext {
    conversation: Arc<dyn DriverConversation>,
    session_configured: SessionConfiguredEvent,
}

impl AgentContext {
    fn from_new_conversation(new_conversation: NewConversation) -> Self {
        Self {
            conversation: Arc::new(RealConversation::new(new_conversation.conversation))
                as Arc<dyn DriverConversation>,
            session_configured: new_conversation.session_configured,
        }
    }
}

fn build_bootstrap_future(
    conversation_manager: Arc<ConversationManager>,
    auth_manager: Arc<AuthManager>,
    config: Config,
    resume_path: Option<PathBuf>,
) -> AgentBootstrapFuture {
    match resume_path {
        Some(path) => Box::pin(async move {
            conversation_manager
                .resume_conversation_from_rollout(config, path, auth_manager)
                .await
                .map(AgentContext::from_new_conversation)
                .map_err(|err| err.to_string())
        }),
        None => Box::pin(async move {
            conversation_manager
                .new_conversation(config)
                .await
                .map(AgentContext::from_new_conversation)
                .map_err(|err| err.to_string())
        }),
    }
}

#[async_trait]
trait DriverConversation: Send + Sync {
    async fn submit(&self, op: Op) -> codex_core::error::Result<String>;
    async fn next_event(&self) -> codex_core::error::Result<Event>;
}

struct RealConversation {
    inner: Arc<CodexConversation>,
}

impl RealConversation {
    fn new(inner: Arc<CodexConversation>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl DriverConversation for RealConversation {
    async fn submit(&self, op: Op) -> codex_core::error::Result<String> {
        self.inner.submit(op).await
    }

    async fn next_event(&self) -> codex_core::error::Result<Event> {
        self.inner.next_event().await
    }
}

/// Opaque handle that represents a running Codex session.
pub struct SessionHandle {
    runtime: Option<Runtime>,
    config: Config,
    #[allow(dead_code)]
    auth_manager: Arc<AuthManager>,
    #[allow(dead_code)]
    conversation_manager: Arc<ConversationManager>,
    event_tx_internal: Option<UnboundedSender<ServerEvent>>,
    command_rx_guard: Option<CommandReceiver<ClientCommand>>,
    event_tx_guard: Option<EventSender<ServerEvent>>,
    bridge_threads: Vec<thread::JoinHandle<()>>,
    agent_task: Option<tokio::task::JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl SessionHandle {
    /// Start a new Codex session using the provided configuration options.
    ///
    /// The returned [`SessionStartup`] contains the `SessionHandle` alongside
    /// the public channel ends for issuing commands and receiving events.
    pub fn start(session: SessionConfig) -> Result<SessionStartup, SessionStartError> {
        Self::start_with_mode(session, None)
    }

    /// Resume a Codex session from an existing rollout.
    pub fn resume(
        session: SessionConfig,
        rollout_path: PathBuf,
    ) -> Result<SessionStartup, SessionStartError> {
        Self::start_with_mode(session, Some(rollout_path))
    }

    fn start_with_mode(
        session: SessionConfig,
        resume_path: Option<PathBuf>,
    ) -> Result<SessionStartup, SessionStartError> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(SessionStartError::RuntimeInit)?;

        let cli_overrides = parse_cli_overrides(&session.raw_cli_overrides)?;
        let overrides = build_config_overrides(&session);
        let config = Config::load_with_cli_overrides(cli_overrides, overrides)
            .map_err(SessionStartError::ConfigLoad)?;

        if session.use_oss {
            runtime
                .block_on(codex_ollama::ensure_oss_ready(&config))
                .map_err(|err| SessionStartError::OssSetup(err.to_string()))?;
        }

        let auth_manager = AuthManager::shared(config.codex_home.clone());
        let conversation_manager = Arc::new(ConversationManager::new(auth_manager.clone()));

        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let tokio_handle = runtime.handle().clone();

        let (internal_command_tx, internal_command_rx) = mpsc::unbounded_channel();
        let (internal_event_tx, internal_event_rx) = mpsc::unbounded_channel();

        let command_rx_for_bridge = command_rx.clone();
        let command_shutdown = shutdown_flag.clone();
        let command_bridge_sender = internal_command_tx.clone();
        let command_bridge = thread::Builder::new()
            .name("codex-driver-command-bridge".into())
            .spawn(move || {
                let poll_interval = Duration::from_millis(50);
                loop {
                    if command_shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match command_rx_for_bridge.recv_timeout(poll_interval) {
                        Ok(cmd) => {
                            if command_bridge_sender.send(cmd).is_err() {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|err| SessionStartError::ThreadSpawn(err.to_string()))?;

        let event_tx_for_bridge = event_tx.clone();
        let event_bridge = thread::Builder::new()
            .name("codex-driver-event-bridge".into())
            .spawn(move || {
                let mut rx = internal_event_rx;
                while let Some(event) = rx.blocking_recv() {
                    if event_tx_for_bridge.send(event).is_err() {
                        break;
                    }
                }
            })
            .map_err(|err| SessionStartError::ThreadSpawn(err.to_string()))?;

        drop(internal_command_tx);

        let agent_bootstrap_fut = build_bootstrap_future(
            conversation_manager.clone(),
            auth_manager.clone(),
            config.clone(),
            resume_path,
        );

        let agent_task = tokio_handle.spawn(run_agent_loop(
            agent_bootstrap_fut,
            internal_command_rx,
            internal_event_tx.clone(),
            shutdown_flag.clone(),
            conversation_manager.clone(),
            auth_manager.clone(),
            config.clone(),
        ));

        let handle = SessionHandle {
            runtime: Some(runtime),
            config,
            auth_manager,
            conversation_manager,
            event_tx_internal: Some(internal_event_tx.clone()),
            command_rx_guard: Some(command_rx.clone()),
            event_tx_guard: Some(event_tx.clone()),
            bridge_threads: vec![command_bridge, event_bridge],
            agent_task: Some(agent_task),
            shutdown_flag,
        };

        Ok(SessionStartup {
            handle,
            command_tx,
            event_rx,
        })
    }

    /// Access the loaded configuration for the running session.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Obtain a clone of the internal event sender used to publish
    /// [`ServerEvent`] values back to the client.
    pub fn event_sender(&self) -> UnboundedSender<ServerEvent> {
        self.event_tx_internal
            .as_ref()
            .expect("event sender available")
            .clone()
    }

    /// Shut down the running session, consuming the handle.
    pub fn shutdown(mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(sender) = self.event_tx_internal.take() {
            drop(sender);
        }
        if let Some(task) = self.agent_task.take() {
            task.abort();
        }

        if let Some(rx) = self.command_rx_guard.take() {
            drop(rx);
        }
        if let Some(tx) = self.event_tx_guard.take() {
            drop(tx);
        }
        for handle in self.bridge_threads.drain(..) {
            let _ = handle.join();
        }

        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn run_agent_loop(
    bootstrap: AgentBootstrapFuture,
    mut command_rx: mpsc::UnboundedReceiver<ClientCommand>,
    event_tx: UnboundedSender<ServerEvent>,
    shutdown_flag: Arc<AtomicBool>,
    conversation_manager: Arc<ConversationManager>,
    auth_manager: Arc<AuthManager>,
    base_config: Config,
) {
    let AgentContext {
        mut conversation,
        session_configured,
    } = match bootstrap.await {
        Ok(context) => context,
        Err(message) => {
            emit_fatal(&event_tx, message);
            let _ = event_tx.send(ServerEvent::SessionEnded);
            shutdown_flag.store(true, Ordering::Relaxed);
            return;
        }
    };

    dispatch_translated(
        &event_tx,
        EventMsg::SessionConfigured(session_configured),
        None,
    );

    let mut event_task = spawn_event_stream(
        conversation.clone(),
        event_tx.clone(),
        shutdown_flag.clone(),
    );

    while let Some(command) = command_rx.recv().await {
        match command {
            ClientCommand::SubmitUserMessage { text, images } => {
                if !forward_user_input(conversation.as_ref(), text, images, &event_tx).await {
                    shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
            }
            ClientCommand::Interrupt => {
                if let Err(err) = conversation.submit(Op::Interrupt).await {
                    emit_fatal(&event_tx, err.to_string());
                    shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
            }
            ClientCommand::Shutdown => {
                shutdown_flag.store(true, Ordering::Relaxed);
                if let Err(err) = conversation.submit(Op::Shutdown).await {
                    emit_fatal(&event_tx, err.to_string());
                }
                if let Err(join_err) = event_task.await
                    && !join_err.is_cancelled()
                {
                    emit_fatal(&event_tx, join_err.to_string());
                    let _ = event_tx.send(ServerEvent::SessionEnded);
                }
                return;
            }
            ClientCommand::Resume { rollout_path } => {
                match conversation_manager
                    .resume_conversation_from_rollout(
                        base_config.clone(),
                        rollout_path,
                        auth_manager.clone(),
                    )
                    .await
                {
                    Ok(new_conversation) => {
                        let old_task = event_task;
                        old_task.abort();
                        match old_task.await {
                            Ok(_) => {}
                            Err(join_err) => {
                                if !join_err.is_cancelled() {
                                    emit_fatal(&event_tx, join_err.to_string());
                                    shutdown_flag.store(true, Ordering::Relaxed);
                                    return;
                                }
                            }
                        }

                        let AgentContext {
                            conversation: new_conversation_arc,
                            session_configured,
                        } = AgentContext::from_new_conversation(new_conversation);

                        conversation = new_conversation_arc;
                        dispatch_translated(
                            &event_tx,
                            EventMsg::SessionConfigured(session_configured),
                            None,
                        );
                        event_task = spawn_event_stream(
                            conversation.clone(),
                            event_tx.clone(),
                            shutdown_flag.clone(),
                        );
                    }
                    Err(err) => {
                        emit_fatal(&event_tx, err.to_string());
                    }
                }
            }
            ClientCommand::RespondApproval { id, kind, decision } => {
                let op = match kind {
                    ApprovalKind::Exec => Op::ExecApproval { id, decision },
                    ApprovalKind::Patch => Op::PatchApproval { id, decision },
                };
                if let Err(err) = conversation.submit(op).await {
                    emit_fatal(&event_tx, err.to_string());
                    shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }

    shutdown_flag.store(true, Ordering::Relaxed);
    let _ = conversation.submit(Op::Shutdown).await;
    if let Err(join_err) = event_task.await
        && !join_err.is_cancelled()
    {
        emit_fatal(&event_tx, join_err.to_string());
        let _ = event_tx.send(ServerEvent::SessionEnded);
    }
}

async fn forward_user_input(
    conversation: &dyn DriverConversation,
    text: String,
    images: Vec<PathBuf>,
    event_tx: &UnboundedSender<ServerEvent>,
) -> bool {
    let mut items: Vec<InputItem> = Vec::new();
    if !text.is_empty() {
        items.push(InputItem::Text { text });
    }
    for image in images {
        items.push(InputItem::LocalImage { path: image });
    }

    if items.is_empty() {
        return true;
    }

    match conversation.submit(Op::UserInput { items }).await {
        Ok(_) => true,
        Err(err) => {
            emit_fatal(event_tx, err.to_string());
            false
        }
    }
}

async fn run_event_stream(
    conversation: Arc<dyn DriverConversation>,
    event_tx: UnboundedSender<ServerEvent>,
    shutdown_flag: Arc<AtomicBool>,
) {
    loop {
        match conversation.next_event().await {
            Ok(Event { id, msg }) => {
                dispatch_translated(&event_tx, msg, Some(&id));
            }
            Err(err) => {
                if !shutdown_flag.load(Ordering::Relaxed) {
                    emit_fatal(&event_tx, err.to_string());
                }
                break;
            }
        }
    }

    let _ = event_tx.send(ServerEvent::SessionEnded);
    shutdown_flag.store(true, Ordering::Relaxed);
}

fn emit_fatal(event_tx: &UnboundedSender<ServerEvent>, message: String) {
    let _ = event_tx.send(ServerEvent::Fatal { message });
}

fn parse_cli_overrides(raw: &[String]) -> Result<Vec<(String, toml::Value)>, SessionStartError> {
    let overrides = CliConfigOverrides {
        raw_overrides: raw.to_vec(),
    };
    overrides
        .parse_overrides()
        .map_err(SessionStartError::InvalidOverride)
}

fn build_config_overrides(session: &SessionConfig) -> ConfigOverrides {
    let (model, model_provider) = determine_model(session);
    let cwd = session
        .cwd
        .as_ref()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()));

    ConfigOverrides {
        model,
        review_model: None,
        cwd,
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode: Some(SandboxMode::DangerFullAccess),
        model_provider,
        config_profile: session.config_profile.clone(),
        codex_linux_sandbox_exe: None,
        base_instructions: None,
        include_plan_tool: Some(true),
        include_apply_patch_tool: None,
        include_view_image_tool: None,
        show_raw_agent_reasoning: session.use_oss.then_some(true),
        tools_web_search_request: None,
    }
}

fn determine_model(session: &SessionConfig) -> (Option<String>, Option<String>) {
    if let Some(model) = session.model.clone() {
        (Some(model), None)
    } else if session.use_oss {
        (
            Some(DEFAULT_OSS_MODEL.to_owned()),
            Some(BUILT_IN_OSS_MODEL_PROVIDER_ID.to_owned()),
        )
    } else {
        (None, None)
    }
}

fn dispatch_translated(tx: &UnboundedSender<ServerEvent>, msg: EventMsg, event_id: Option<&str>) {
    for event in translate_event_msg(msg, event_id) {
        let _ = tx.send(event);
    }
}

fn translate_event_msg(msg: EventMsg, event_id: Option<&str>) -> Vec<ServerEvent> {
    match msg {
        EventMsg::SessionConfigured(ev) => {
            vec![ServerEvent::SessionConfigured {
                model: ev.model,
                conversation_id: ev.session_id,
                rollout_path: ev.rollout_path,
            }]
        }
        EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
            vec![ServerEvent::AgentMessageDelta(delta)]
        }
        EventMsg::AgentMessage(AgentMessageEvent { message }) => {
            vec![ServerEvent::AgentMessageFinal(message)]
        }
        EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta })
        | EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent { delta }) => {
            vec![ServerEvent::AgentReasoningDelta(delta)]
        }
        EventMsg::AgentReasoning(AgentReasoningEvent { text })
        | EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
            vec![ServerEvent::AgentReasoningFinal(text)]
        }
        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {}) => {
            vec![ServerEvent::ReasoningSectionBreak]
        }
        EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
            call_id, command, ..
        }) => {
            vec![ServerEvent::ExecCommandBegin { call_id, command }]
        }
        EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id, exit_code, ..
        }) => {
            vec![ServerEvent::ExecCommandEnd {
                call_id,
                exit_code: Some(exit_code),
            }]
        }
        EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id,
            stream,
            chunk,
        }) => {
            let text = decode_exec_chunk(&chunk);
            vec![ServerEvent::ExecOutputChunk {
                call_id,
                stream,
                text,
            }]
        }
        EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id,
            auto_approved,
            ..
        }) => {
            vec![ServerEvent::PatchApplyBegin {
                call_id,
                auto_approved,
            }]
        }
        EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id, success, ..
        }) => {
            vec![ServerEvent::PatchApplyEnd { call_id, success }]
        }
        EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
            call_id,
            invocation,
        }) => {
            vec![ServerEvent::McpToolCallBegin {
                call_id,
                server: invocation.server,
                tool: invocation.tool,
            }]
        }
        EventMsg::McpToolCallEnd(McpToolCallEndEvent {
            call_id,
            invocation,
            duration,
            result,
        }) => {
            vec![ServerEvent::McpToolCallEnd {
                call_id,
                server: invocation.server,
                tool: invocation.tool,
                duration,
                result,
            }]
        }
        EventMsg::WebSearchBegin(WebSearchBeginEvent { call_id }) => {
            vec![ServerEvent::WebSearchBegin { call_id }]
        }
        EventMsg::WebSearchEnd(WebSearchEndEvent { call_id, query }) => {
            vec![ServerEvent::WebSearchEnd { call_id, query }]
        }
        EventMsg::TokenCount(TokenCountEvent {
            info:
                Some(TokenUsageInfo {
                    total_token_usage,
                    last_token_usage,
                    ..
                }),
            ..
        }) => {
            vec![ServerEvent::TokenUsage {
                blended_total: total_token_usage.blended_total(),
                input: last_token_usage.non_cached_input(),
                cached_input: last_token_usage.cached_input(),
                output: last_token_usage.output_tokens,
                reasoning_output: last_token_usage.reasoning_output_tokens,
            }]
        }
        EventMsg::TokenCount(TokenCountEvent { info: None, .. }) => Vec::new(),
        EventMsg::TaskComplete(TaskCompleteEvent { last_agent_message }) => {
            vec![ServerEvent::TurnComplete {
                last_message: last_agent_message,
            }]
        }
        EventMsg::TurnAborted(TurnAbortedEvent { reason }) => {
            let reason_str = match reason {
                codex_core::protocol::TurnAbortReason::Interrupted => "interrupted".to_string(),
                codex_core::protocol::TurnAbortReason::Replaced => "replaced".to_string(),
                codex_core::protocol::TurnAbortReason::ReviewEnded => "review-ended".to_string(),
            };
            vec![ServerEvent::TurnAborted { reason: reason_str }]
        }
        EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => {
            vec![ServerEvent::TurnDiff { unified_diff }]
        }
        EventMsg::PlanUpdate(update) => {
            vec![ServerEvent::PlanUpdate(convert_plan_update(update))]
        }
        EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
            vec![ServerEvent::BackgroundMessage(message)]
        }
        EventMsg::StreamError(StreamErrorEvent { message }) => {
            vec![ServerEvent::StreamError(message)]
        }
        EventMsg::ShutdownComplete => vec![ServerEvent::ShutdownComplete],
        EventMsg::Error(ev) => vec![ServerEvent::Fatal {
            message: ev.message,
        }],
        EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            command,
            cwd,
            reason,
        }) => match event_id {
            Some(id) => vec![ServerEvent::ApprovalRequest {
                id: id.to_string(),
                request: ApprovalRequest::Exec {
                    call_id,
                    command,
                    cwd,
                    reason,
                },
            }],
            None => Vec::new(),
        },
        EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            changes,
            reason,
            grant_root,
        }) => match event_id {
            Some(id) => vec![ServerEvent::ApprovalRequest {
                id: id.to_string(),
                request: ApprovalRequest::Patch {
                    call_id,
                    changes,
                    reason,
                    grant_root,
                },
            }],
            None => Vec::new(),
        },
        // Events that are not yet surfaced produce no output.
        _ => Vec::new(),
    }
}

fn decode_exec_chunk(chunk: &[u8]) -> String {
    String::from_utf8_lossy(chunk).into_owned()
}

fn convert_plan_update(args: UpdatePlanArgs) -> PlanUpdate {
    let steps = args
        .plan
        .into_iter()
        .map(|PlanItemArg { step, status }| PlanStep {
            step,
            status: convert_plan_status(status),
        })
        .collect();

    PlanUpdate {
        explanation: args.explanation,
        steps,
    }
}

fn convert_plan_status(status: StepStatus) -> PlanStepStatus {
    match status {
        StepStatus::Pending => PlanStepStatus::Pending,
        StepStatus::InProgress => PlanStepStatus::InProgress,
        StepStatus::Completed => PlanStepStatus::Completed,
    }
}

fn spawn_event_stream(
    conversation: Arc<dyn DriverConversation>,
    event_tx: UnboundedSender<ServerEvent>,
    shutdown_flag: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_event_stream(conversation, event_tx, shutdown_flag).await;
    })
}

/// Convenience re-export for downstream crates.
pub mod prelude {
    pub use super::ApprovalKind;
    pub use super::ApprovalRequest;
    pub use super::ClientCommand;
    pub use super::PlanStep;
    pub use super::PlanStepStatus;
    pub use super::PlanUpdate;
    pub use super::ServerEvent;
    pub use super::SessionConfig;
    pub use super::SessionHandle;
    pub use super::SessionStartError;
    pub use super::SessionStartup;
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::AuthManager;
    use codex_core::CodexAuth;
    use codex_core::config::Config;
    use codex_core::config::ConfigOverrides;
    use codex_core::config::ConfigToml;
    use codex_core::error::CodexErr;
    use codex_core::protocol::McpInvocation;
    use codex_core::protocol::ModelStreamClosedEvent;
    use codex_protocol::ConversationId;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;
    use tokio::runtime::Builder;
    use tokio::sync::Mutex as AsyncMutex;

    #[test]
    fn agent_loop_streams_events_from_mock_conversation() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let (conversation, harness) = MockConversation::new();
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let conversation_manager = Arc::new(ConversationManager::new(auth_manager.clone()));
        let codex_home = std::env::temp_dir().join(format!(
            "codex-driver-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        fs::create_dir_all(&codex_home).expect("create temp codex home");
        let config = Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            codex_home,
        )
        .expect("load config");
        let session_configured = SessionConfiguredEvent {
            session_id: ConversationId::new(),
            model: "mock-model".to_string(),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::new(),
        };

        let bootstrap: AgentBootstrapFuture = Box::pin(async move {
            Ok(AgentContext {
                conversation,
                session_configured,
            })
        });

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let handle = runtime.spawn(run_agent_loop(
            bootstrap,
            command_rx,
            event_tx,
            shutdown_flag.clone(),
            conversation_manager,
            auth_manager,
            config,
        ));

        // SessionConfigured should be forwarded immediately.
        let configured = runtime
            .block_on(async { event_rx.recv().await })
            .expect("session configured event");
        assert!(matches!(
            configured,
            ServerEvent::SessionConfigured {
                ref model,
                ..
            } if model == "mock-model"
        ));

        command_tx
            .send(ClientCommand::SubmitUserMessage {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send submit");

        harness.emit(EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
            delta: "hello".to_string(),
        }));
        harness.emit(EventMsg::AgentMessage(AgentMessageEvent {
            message: "hello".to_string(),
        }));

        let delta_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("delta event");
        assert!(matches!(delta_evt, ServerEvent::AgentMessageDelta(ref msg) if msg == "hello"));

        let final_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("final event");
        assert!(matches!(final_evt, ServerEvent::AgentMessageFinal(ref msg) if msg == "hello"));

        harness.emit(EventMsg::ExecCommandOutputDelta(
            ExecCommandOutputDeltaEvent {
                call_id: "exec-call".to_string(),
                stream: ExecOutputStream::Stdout,
                chunk: b"chunk".to_vec(),
            },
        ));
        let chunk_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("exec chunk event");
        assert!(matches!(
            chunk_evt,
            ServerEvent::ExecOutputChunk {
                ref call_id,
                stream: ExecOutputStream::Stdout,
                ref text,
            } if call_id == "exec-call" && text == "chunk"
        ));

        harness.emit(EventMsg::AgentReasoningSectionBreak(
            AgentReasoningSectionBreakEvent {},
        ));
        let break_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("reasoning break");
        assert!(matches!(break_evt, ServerEvent::ReasoningSectionBreak));

        harness.emit(EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: Some("focus".to_string()),
            plan: vec![PlanItemArg {
                step: "step one".to_string(),
                status: StepStatus::InProgress,
            }],
        }));
        let plan_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("plan update");
        match plan_evt {
            ServerEvent::PlanUpdate(update) => {
                assert_eq!(update.explanation.as_deref(), Some("focus"));
                assert_eq!(update.steps.len(), 1);
                assert_eq!(update.steps[0].step, "step one");
                assert!(matches!(update.steps[0].status, PlanStepStatus::InProgress));
            }
            other => panic!("unexpected plan event {other:?}"),
        }

        harness.emit(EventMsg::ModelStreamClosed(ModelStreamClosedEvent {
            had_completion: true,
        }));
        let stream_closed_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("stream closed event");
        assert!(matches!(
            stream_closed_evt,
            ServerEvent::StreamClosed {
                had_completion: true
            }
        ));

        let invocation = McpInvocation {
            server: "server".to_string(),
            tool: "tool".to_string(),
            arguments: None,
        };
        harness.emit(EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
            call_id: "mcp-1".to_string(),
            invocation: invocation.clone(),
        }));
        let mcp_begin = runtime
            .block_on(async { event_rx.recv().await })
            .expect("mcp begin");
        assert!(matches!(
            mcp_begin,
            ServerEvent::McpToolCallBegin {
                ref call_id,
                ref server,
                ref tool,
            } if call_id == "mcp-1" && server == "server" && tool == "tool"
        ));

        harness.emit(EventMsg::McpToolCallEnd(McpToolCallEndEvent {
            call_id: "mcp-1".to_string(),
            invocation,
            duration: Duration::from_millis(5),
            result: Ok(CallToolResult {
                content: Vec::new(),
                is_error: None,
                structured_content: None,
            }),
        }));
        let mcp_end = runtime
            .block_on(async { event_rx.recv().await })
            .expect("mcp end");
        assert!(matches!(
            mcp_end,
            ServerEvent::McpToolCallEnd {
                ref call_id,
                ref server,
                ref tool,
                duration,
                result: Ok(_),
            } if call_id == "mcp-1" && server == "server" && tool == "tool" && duration == Duration::from_millis(5)
        ));

        harness.emit_with_id(
            "exec-1",
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "exec-call".to_string(),
                command: vec!["rm".to_string(), "-rf".to_string(), ".".to_string()],
                cwd: PathBuf::from("/tmp"),
                reason: Some("danger".to_string()),
            }),
        );

        let approval_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("approval event");
        match approval_evt {
            ServerEvent::ApprovalRequest { id, request } => {
                assert_eq!(id, "exec-1");
                match request {
                    ApprovalRequest::Exec {
                        call_id,
                        command,
                        cwd,
                        reason,
                    } => {
                        assert_eq!(call_id, "exec-call");
                        assert_eq!(command, vec!["rm", "-rf", "."]);
                        assert_eq!(cwd, PathBuf::from("/tmp"));
                        assert_eq!(reason.as_deref(), Some("danger"));
                    }
                    _ => panic!("expected exec approval"),
                }
            }
            other => panic!("unexpected event {other:?}"),
        }

        command_tx
            .send(ClientCommand::RespondApproval {
                id: "exec-1".to_string(),
                kind: ApprovalKind::Exec,
                decision: ReviewDecision::Approved,
            })
            .expect("respond to exec approval");

        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello".to_string(),
            },
        );
        harness.emit_with_id(
            "patch-1",
            EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id: "patch-call".to_string(),
                changes: changes.clone(),
                reason: None,
                grant_root: Some(PathBuf::from("/workspace")),
            }),
        );

        let patch_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("patch approval event");
        match patch_evt {
            ServerEvent::ApprovalRequest { id, request } => {
                assert_eq!(id, "patch-1");
                match request {
                    ApprovalRequest::Patch {
                        call_id,
                        changes: ref change_map,
                        reason,
                        grant_root,
                    } => {
                        assert_eq!(call_id, "patch-call");
                        assert_eq!(change_map, &changes);
                        assert!(reason.is_none());
                        assert_eq!(grant_root, Some(PathBuf::from("/workspace")));
                    }
                    _ => panic!("expected patch approval"),
                }
            }
            other => panic!("unexpected event {other:?}"),
        }

        command_tx
            .send(ClientCommand::RespondApproval {
                id: "patch-1".to_string(),
                kind: ApprovalKind::Patch,
                decision: ReviewDecision::Denied,
            })
            .expect("respond to patch approval");

        command_tx
            .send(ClientCommand::Shutdown)
            .expect("send shutdown");
        harness.emit(EventMsg::ShutdownComplete);
        harness.close();

        let shutdown_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("shutdown complete");
        assert!(matches!(shutdown_evt, ServerEvent::ShutdownComplete));

        let ended_evt = runtime
            .block_on(async { event_rx.recv().await })
            .expect("session ended");
        assert!(matches!(ended_evt, ServerEvent::SessionEnded));

        runtime.block_on(async { handle.await.expect("agent loop completes") });

        let recorded_ops = runtime.block_on(async { harness.take_submitted().await });
        assert!(matches!(
            recorded_ops.as_slice(),
            [
                Op::UserInput { .. },
                Op::ExecApproval { .. },
                Op::PatchApproval { .. },
                Op::Shutdown
            ]
        ));
    }

    struct MockConversation {
        submissions: Arc<AsyncMutex<Vec<Op>>>,
        events: AsyncMutex<mpsc::UnboundedReceiver<Event>>,
    }

    struct MockConversationHandle {
        submissions: Arc<AsyncMutex<Vec<Op>>>,
        event_tx: StdMutex<Option<mpsc::UnboundedSender<Event>>>,
    }

    impl MockConversationHandle {
        fn emit(&self, msg: EventMsg) {
            self.emit_with_id("mock", msg);
        }

        fn emit_with_id(&self, id: &str, msg: EventMsg) {
            if let Some(tx) = self.event_tx.lock().expect("lock event_tx").as_ref() {
                let _ = tx.send(Event {
                    id: id.to_string(),
                    msg,
                });
            }
        }

        async fn take_submitted(&self) -> Vec<Op> {
            let mut guard = self.submissions.lock().await;
            std::mem::take(&mut *guard)
        }

        fn close(&self) {
            let _ = self.event_tx.lock().expect("lock event_tx").take();
        }
    }

    impl MockConversation {
        fn new() -> (Arc<Self>, MockConversationHandle) {
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            let submissions = Arc::new(AsyncMutex::new(Vec::new()));
            let conversation = Arc::new(Self {
                submissions: submissions.clone(),
                events: AsyncMutex::new(event_rx),
            });
            let handle = MockConversationHandle {
                submissions,
                event_tx: StdMutex::new(Some(event_tx)),
            };
            (conversation, handle)
        }
    }

    #[async_trait]
    impl DriverConversation for MockConversation {
        async fn submit(&self, op: Op) -> codex_core::error::Result<String> {
            let mut submissions = self.submissions.lock().await;
            submissions.push(op);
            Ok("mock-submission".to_string())
        }

        async fn next_event(&self) -> codex_core::error::Result<Event> {
            let mut events = self.events.lock().await;
            match events.recv().await {
                Some(event) => Ok(event),
                None => Err(CodexErr::InternalAgentDied),
            }
        }
    }
}
