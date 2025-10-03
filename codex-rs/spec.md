# Channel Driver Spec

## Overview
Build a new crate (`codex-driver`) that exposes a send-command/stream-events interface over crossbeam channels. The crate reuses `codex-core` primitives (configuration, `ConversationManager`, protocol types) but does not modify or depend on `codex-tui`. The TUI remains untouched; this is a parallel integration path intended for headless or embedded clients that need Codex agent functionality without terminal rendering or transcript management.

## Goals
- Provide a pure library crate with no terminal dependencies.
- Allow callers to start a Codex session, submit a single-turn command (text plus optional local image paths), and receive a structured event stream (tool invocations, diffs, progress, approvals, final response).
- Reuse existing async plumbing (`ConversationManager`, agent spawn loops, protocol enums) to maintain behavioral parity with the current agent flow.
- Surface approvals and diff previews as structured events so non-TUI callers can respond via paired commands.
- Keep the public API limited to crossbeam channels while internally continuing to run on Tokio.

## Non-Goals
- No refactoring of `codex-tui`, its widgets, or `App` event loop.
- No shared UI code between TUI and the new driver; the TUI does not consume the driver.
- No persistence, resume, or transcript history features—callers manage storage externally.
- No protocol redesign; approvals, diffs, and tool outputs are forwarded as-is.

## Proposed Architecture
1. **Crate layout**
   - `codex-driver/src/lib.rs` exports `SessionConfig`, `SessionHandle`, `ClientCommand`, and `ServerEvent`.
   - Internal modules wrap `codex-core` functionality and expose minimal glue; avoid depending on `codex-tui`.

2. **Session lifecycle**
   - `SessionDriver::start` (or similar) loads config/auth inputs, constructs a `ConversationManager`, and spawns the agent (mirroring `chatwidget::agent::spawn_agent`).
   - Returns `SessionHandle` containing `crossbeam_channel::Sender<ClientCommand>` and `crossbeam_channel::Receiver<ServerEvent>` along with shutdown helpers.
   - Tokio tasks translate protocol `EventMsg` values into simplified `ServerEvent` variants while handling per-turn state (pending approvals, diff previews).

3. **Command/Event contracts**
   - `ClientCommand` variants: `SubmitUserMessage`, `Approve`, `Reject`, `Interrupt`, `RequestDiffPreview`, etc.
   - `ServerEvent` variants: `AgentDelta`, `AgentFinal`, `ToolCallBegin`, `ToolCallEnd`, `DiffPreview`, `ApprovalRequired`, `TokenUsage`, `Notification`, `FatalError`, `SessionEnded`.
   - Commands received over crossbeam are forwarded to existing `Op` submissions or supporting helpers inside the driver.

4. **Runtime bridging**
   - Internally rely on Tokio `mpsc`/streams; use adapter tasks to bridge into crossbeam senders/receivers exposed publicly to avoid blocking the runtime.

5. **Error handling & shutdown**
   - Initialization failures return `Result<SessionHandle, DriverError>`.
   - When the underlying agent completes or the caller drops the sender, emit `ServerEvent::SessionEnded` and cleanly stop background tasks.

## Spike Tickets
1. **Driver API Contract Spike** – Nail down `ClientCommand`/`ServerEvent` definitions and confirm they cover required non-interactive flows (approvals, diffs, streaming text).
2. **Channel Bridging Spike** – Prototype bridging Tokio `mpsc` to crossbeam channels, validating backpressure and shutdown semantics.
3. **Approval/Diff Requirements Spike** – Document approval and diff lifecycles to ensure emitted events carry enough metadata for headless consumers.
4. **Session Logging & Metrics Spike** – Investigate how `session_log`/OTel hooks should behave in headless mode (opt-in vs disabled by default).
5. **Config & Auth Bootstrapping Spike** – Determine which config/auth helpers from `codex-core` can be reused directly and ensure they run without terminal assumptions.
6. **Danger Mode Simplification Spike** – Evaluate a v0 that runs in "always dangerous" mode (approvals/sandbox disabled); document simplifications, caveats, and reintroduction plan for approvals.

## Spike Findings

### Driver API Contract Spike
- Likely command set mirrors the subset of `codex_core::protocol::Op` invoked by the TUI for single turns: `UserInput`, `Interrupt`, `Shutdown`, `ListMcpTools`, `ListCustomPrompts`, `Compact`, `Review`, plus approval responses via `ExecApproval` and `PatchApproval`. `AddToHistory` is optional if v0 omits persistent history.
- Events to surface correspond to `EventMsg` variants actually consumed in `ChatWidget::dispatch_event_msg`: turn lifecycle (`SessionConfigured`, `TaskStarted`, `TaskComplete`, `TurnAborted`), text and reasoning streams (`AgentMessageDelta`, `AgentMessage`, `AgentReasoning*`), tool activity (`McpToolCall*`, `WebSearch*`, `ExecCommand*`, `PatchApply*`, `TurnDiff`), approval prompts (`ExecApprovalRequest`, `ApplyPatchApprovalRequest`), metadata (`TokenCount`, `BackgroundEvent`, `StreamError`, `PlanUpdate`), and shutdown notifications (`ShutdownComplete`).
- Final turn responses can be taken from either the accumulated `AgentMessage` content or `TaskCompleteEvent.last_agent_message`; the latter is what TUI uses for notifications once a turn ends.
- The driver can treat slash-command features (model selection, resume flows, history playback) as out-of-scope for v0 since they live purely in TUI command handling.

### Channel Bridging Spike
- Expose crossbeam endpoints (`crossbeam_channel::Sender<ClientCommand>`, `Receiver<ServerEvent>`) while retaining Tokio internally. Crossbeam’s unbounded channels offer non-blocking `send`; dropping the receiver causes `send` to return `Err`, which the driver can treat as a shutdown signal.
- Internally, prefer Tokio `mpsc::unbounded_channel` for agent-facing queues. A dedicated Tokio task can forward events from the internal receiver into the crossbeam sender; because `send` is immediate, the task stays async-friendly and simply exits when the client side closes.
- To consume client commands, spawn a blocking bridge (std thread or `tokio::task::spawn_blocking`) that performs `crossbeam_receiver.recv()` and forwards each command into the Tokio `mpsc::UnboundedSender`. This avoids blocking the async runtime while still supporting synchronous callers.
- Store join handles for the bridge tasks inside `SessionHandle` so `shutdown()` can await orderly termination and drop the underlying conversations cleanly.

### Approval/Diff Requirements Spike
- `ExecApprovalRequestEvent` carries `call_id`, command vector, cwd, and optional reason; the `Event.id` (submission id) is what `Op::ExecApproval` expects back. Driver events should surface both so clients can display user-friendly context while replying with the correct identifier.
- `ApplyPatchApprovalRequestEvent` includes `changes: HashMap<PathBuf, FileChange>`, optional `reason`, and optional `grant_root` (indicates the agent wants broader write access). As with exec approvals, the reply uses the event id, while `call_id` links to subsequent `PatchApplyBegin/End` notifications.
- `PatchApplyBegin/End` mirror exec begin/end for code modifications, and the `FileChange::Update` variant already contains unified diffs. Drivers can forward these structures verbatim; headless clients decide whether to render summarized diffs or raw blobs.
- `TurnDiffEvent` provides a single unified diff summarizing the agent’s file changes for the turn. Even if v0 omits history, the driver should emit this so callers can present post-turn summaries.
- No streaming exec output is currently surfaced (TUI ignores `ExecCommandOutputDeltaEvent`), so v0 can safely drop or buffer those deltas until support is needed.

### Session Logging & Metrics Spike
- `session_log` is opt-in via `CODEX_TUI_RECORD_SESSION`; it writes JSONL traces whenever initialized. Since it lives in `codex-tui`, the driver would need a parallel implementation if we want comparable traces. v0 can skip session logging entirely or expose a pluggable trait for clients that need it.
- Telemetry initialization happens in `codex_tui::run_main` via `codex_core::otel_init::build_provider`. The helper is reusable from the new crate: call `build_provider`, set up tracing layers, and respect whatever exporter is configured in `Config`.
- File-based tracing (terminal log) currently writes to `log_dir` using `tracing_appender::non_blocking`. The driver can reuse this pattern or rely on caller-provided tracing subscribers; nothing in `codex-core` requires the file sink.
- No other components depend on session logging or OTEL being active; they are additive features, so omitting them for v0 will not break agent functionality.

### Config & Auth Bootstrapping Spike
- `Config::load_with_cli_overrides` already bundles filesystem config (`config.toml`) with runtime overrides. The TUI constructs `ConfigOverrides` that set sandbox/approval defaults, optional OSS toggles, and tool availability (`include_plan_tool = Some(true)`). The driver can expose a higher-level builder that either accepts a ready `Config` or mirrors this helper without any TUI dependencies.
- Canonicalizing `cwd` happens before ConfigOverrides are applied (`cli.cwd.clone().map(|p| p.canonicalize().unwrap_or(p))`); reproducing this logic keeps sandbox paths consistent.
- `AuthManager::shared(config.codex_home.clone())` is the canonical way to obtain auth state. No additional UI hooks are needed: the manager lives in `codex-core` and handles token refresh/logout logic.
- If a caller wants OSS models, TUI currently calls `codex_ollama::ensure_oss_ready(&config).await`. The driver should conditionally reuse this check based on config flags to avoid surprising failures when model binaries are missing.
- Runtime consumers who want to tweak plan/apply_patch/view_image tool inclusion can do so via the same `ConfigOverrides` fields; leaving them `None` preserves whatever defaults `Config` yields.

### Danger Mode Simplification Spike
- Setting `approval_policy = AskForApproval::Never` and `sandbox_policy = SandboxPolicy::DangerFullAccess` causes `assess_command_safety`/`assess_patch_safety` to auto-approve most operations, skipping user prompts entirely. The agent will still emit errors when a command/patch would normally require approval but is forbidden under "never" (e.g., patch writes outside allowed roots), but no approval events are generated.
- With approvals disabled, the driver can omit support for `ExecApprovalRequest`/`ApplyPatchApprovalRequest` events in v0. Downstream clients must accept that dangerous actions execute immediately, and denied operations surface as `EventMsg::Error` or failed tool calls.
- Re-enabling approvals later will require reinstating the event/command plumbing identified earlier, so the v0 architecture should leave space to plug those back in (e.g., keep enums but gate variants behind feature flags).

## Acceptance Criteria

- **Crate Layout**
  - Add a new crate (`codex-driver`) that builds as a library and exposes a companion binary `headless-codex` for manual testing.
  - Library exports a public `SessionHandle` (or equivalent) with crossbeam `Sender<ClientCommand>` / `Receiver<ServerEvent>`; the binary depends solely on this surface.

- **Session Lifecycle**
  - `SessionHandle::start(config_args)` (async) loads config/auth via `codex-core`, spawns the agent, and returns immediately with functioning channels.
  - Dropping the handle or sending a `ClientCommand::Shutdown` stops the agent, drains tasks, and closes the event stream cleanly.

- **Command Coverage (v0)**
  - Support `SubmitUserMessage { text, images? }`, `Interrupt`, and `Shutdown` client commands.
  - Enforce "danger mode" defaults internally (`approval_policy = Never`, `sandbox_policy = DangerFullAccess`) and document the safety implications.
  - Omit approval request handling in v0, but leave enums/variants ready for future support.

- **Event Stream**
  - Emit ordered `ServerEvent` variants covering: session start/configured, agent reasoning deltas/final message, tool activity (exec, apply_patch, MCP, web search), turn completion/failure, token usage, turn diff summary, and shutdown notifications.
  - No transcript persistence; consumers only receive live events.

- **Headless Binary**
  - CLI wraps the driver to run a single turn: accept prompt via stdin/flag, print streamed events to stdout (grouped by type), and exit with non-zero status on fatal errors.
  - Optional interactive mode for manual testing (e.g., REPL sending successive `SubmitUserMessage`).

- **Config/Auth Hooks**
  - Reuse codex CLI conventions (`--model`, `--oss`, `-c` overrides) and share auth via `AuthManager::shared(config.codex_home.clone())`.
  - Ensure OSS flows reuse `codex_ollama::ensure_oss_ready` when `--oss` is supplied.

- **Telemetry & Logging**
  - Library compiles without requiring session logging or OTEL. Allow callers/binary to opt into tracing via existing env vars (e.g., `RUST_LOG`).

- **Documentation**
  - Update `spec.md` and crate docs with API description, safety warning for danger mode, and usage instructions for the headless binary.
  - Provide example code snippet demonstrating embedding the driver in another Rust program.

## Open Questions / Risks
- How much of the existing approval and diff logic can be reused without pulling UI dependencies?
- What level of logging/telemetry is expected from headless integrations?
- Does running in dangerous mode for v0 meet security and product requirements for initial users?
- What is the expected lifecycle for long-running sessions if transcripts are external (e.g., responsibility for detecting inactivity or cleanup)?

## Implementation Tickets

- [x] **Create `codex-driver` crate scaffold**
  - Add the crate to the workspace with both `lib` and `bin/headless-codex` targets; ensure `cargo check` succeeds.
  - Library exposes placeholder `SessionHandle`, `ClientCommand`, and `ServerEvent` types (documented but not yet wired).
  - Binary compiles and shows a basic `--help` usage describing forthcoming functionality.

- [x] **Session bootstrap implementation**
  - Implement `SessionHandle::start` to load configuration (supporting `--model`, `--oss`, `-c` overrides), initialize `AuthManager`, optionally run `ensure_oss_ready`, and construct `ConversationManager`.
  - Return crossbeam command/event channels plus a shutdown path; dropping the handle or calling `shutdown` cancels background tasks and closes channels.
  - Add tests (unit or integration) confirming that a session can start and shut down without deadlocks or leaked tasks.

- [x] **Tokio ↔ crossbeam channel bridges**
  - Create internal Tokio `mpsc` channels and bridge tasks that forward events into crossbeam receivers and commands back into Tokio senders.
  - Document and test graceful shutdown when either side drops (e.g., close event receiver → bridge task exits).
  - Ensure bridges never block the async runtime (use `spawn_blocking` or dedicated threads for crossbeam `recv`).

- [x] **Command handling (danger-mode v0)**
  - Implement `ClientCommand::SubmitUserMessage`, `Interrupt`, and `Shutdown`, mapping to `Op::UserInput`, `Op::Interrupt`, and `Op::Shutdown` respectively.
  - Force danger-mode overrides (`approval_policy = Never`, `sandbox_policy = DangerFullAccess`) when starting sessions; clearly warn in docs.
  - Provide an automated test or example that submits a prompt and observes at least one streamed response event.

- [x] **Event translation pipeline**
  - Map `EventMsg` variants to public `ServerEvent` enums covering session start, reasoning deltas/finals, exec/apply_patch begin/end, MCP/web search, token usage, turn completion/failure, unified diffs, and shutdown.
  - Preserve ordering by forwarding events sequentially; include unit tests for representative mappings.
  - Emit `ServerEvent::SessionEnded` (or equivalent) when the agent stops or a fatal error occurs.

- [x] **Headless binary UX**
  - Implement CLI parsing for prompt input (flag or stdin), optional interactive mode, and config overrides.
  - Stream events to stdout with readable tags; exit with non-zero code on fatal errors.
  - Add a smoke test (using the existing test backend or mocks) that exercises the binary end-to-end.

- [x] **Docs & examples**
  - Update crate-level docs, README section, and `spec.md` status to reflect implemented features and safety warnings.
  - Provide a Rust example illustrating embedding the driver in another app.
  - Ensure `cargo doc --no-deps -p codex-driver` succeeds.

## Upcoming Spike Tickets

- **Agent Event Wiring Spike** – Investigate how to hook `ConversationManager::new_conversation` and the existing agent event stream into the driver. Understand what data we need from `SessionConfiguredEvent`, how streaming events arrive, and how to coordinate the async tasks without blocking.
- **Approval/Diff Re-introduction Spike** – Determine the minimal subset of approval workflows to re-enable (Exec/Patch), including how to expose approval prompts on the event stream and consume responses over `ClientCommand`. Decide whether to defer UI/CLI surface or provide textual prompts.
- **Tool Output Streaming Spike** – Review how exec output, patch previews, MCP tool results, and other long-running streams are surfaced today. Document the event ordering expectations so the driver can translate them faithfully and avoid echoing fake data.
- **Session State & Resume Spike** – Explore requirements for multi-turn sessions: what state must be retained (history/log paths), how resume should behave, and what new commands/events we need before exposing the feature in the driver/CLI.

### Agent Event Wiring Spike – findings
- Reference implementation lives in `tui/src/chatwidget/agent.rs`: it calls `ConversationManager::new_conversation`, forwards the initial `SessionConfiguredEvent`, spawns a task that drains `conversation.next_event()`, and concurrently forwards `Op`s via `conversation.submit(op)`.
- In `codex-driver` we already own a Tokio runtime; the same pattern can be reproduced by spawning an async task that awaits `new_conversation`, keeps the returned `CodexConversation`, and routes events through our existing `translate_event_msg` helper.
- Remove the synthetic “echo” events once real events flow from the agent. Errors from `new_conversation`/`next_event` should be surfaced as `ServerEvent::Fatal` followed by `SessionEnded` to prevent silent failures.
- Keep the command bridge but redirect it to `conversation.submit`; retain the internal op channel only for tests/debugging.

### Approval/Diff Re-introduction Spike – findings
- Exec approvals emit `EventMsg::ExecApprovalRequest`; apply-patch approvals emit `EventMsg::ApplyPatchApprovalRequest`. Current UI responds with `Op::ExecApproval { id, decision }` or `Op::PatchApproval { id, decision }` where `decision` is a `ReviewDecision` (Approved, Denied, Abort, etc.).
- To support approvals we need a new `ClientCommand` (e.g., `RespondApproval { id, decision }`) and a corresponding `ServerEvent::ApprovalRequest` that carries command/diff metadata plus any reason/grant_root fields.
- Diff previews are already emitted via `PatchApplyBegin/End` and `TurnDiff`; we should continue forwarding them but improve CLI rendering (e.g., print diff text with minimal formatting).
- CLI UX decision: either prompt users interactively or leave approval handling to embedding clients. For v1 we can print a message and expect an explicit `ClientCommand` from the caller.

### Tool Output Streaming Spike – findings
- Pending event types: `ExecCommandOutputDeltaEvent`, `AgentReasoningSectionBreak`, `PlanUpdate`, `ConversationHistory`, `UpdatePlanArgs`. We currently discard these; decide whether to surface raw deltas (possibly base64-decoded) or defer until unified exec/tool work is available.
- MCP tool results arrive in `McpToolCallEndEvent::result`; ensure the driver forwards the result payload (currently omitted).
- Streaming output may generate a large volume; consider chunked `ServerEvent` variants (`ExecStdoutChunk`, `ToolOutputChunk`) to avoid buffering entire output in memory.

### Session State & Resume Spike – findings
- Multi-turn sessions simply require leaving the session running: skip auto-shutdown after the first prompt, and expose a `ClientCommand::Shutdown` to callers. CLI interactive mode should call shutdown only on exit.
- Resume from rollout uses `ConversationManager::resume_conversation_from_rollout`; the initial `SessionConfiguredEvent` includes the `rollout_path`. Expose this path (and conversation id) via a `ServerEvent::SessionConfigured` payload so clients can persist it.
- For historical logs, the driver may need to surface `ConversationPath` events; today we drop them. Reintroducing resume will require translating that event and possibly providing helper APIs to list available rollouts.

## Upcoming Implementation Tickets

- [x] **Agent event loop integration**
  - Replace the synthetic command task with a real loop that invokes `ConversationManager::new_conversation`, retains the returned `CodexConversation`, and forwards ops via `conversation.submit(op)`.
  - Spawn a Tokio task that continuously awaits `conversation.next_event()`, translating each `EventMsg` through `translate_event_msg` and pushing results to the public event channel.
  - Surface startup failures (`new_conversation` errors) and stream errors as `ServerEvent::Fatal` followed by `SessionEnded`; ensure the session shuts down cleanly on error.
  - Update unit tests to assert that submitting a prompt yields actual streamed events (without the old echo), using a mocked/synthetic conversation if necessary.

- [x] **Remove synthetic response generation**
  - Delete the current echo logic that emits `AgentMessageDelta/Final` and `TokenUsage` inside the command task.
  - Ensure `SessionConfigured` events originate from the agent stream rather than `build_session_configured_event`; keep a fallback only if the agent never delivers one.
  - Adjust CLI tests/expectations to reflect real agent behaviour (when running against the synthetic test backend, ensure events are deterministic or stubbed appropriately).

- [x] **Approval command plumbing**
  - Add `ClientCommand::RespondApproval { id, kind, decision }` and a matching `ServerEvent::ApprovalRequest { id, request }` capturing exec/patch metadata.
  - Translate `EventMsg::ExecApprovalRequest` and `EventMsg::ApplyPatchApprovalRequest` into the new `ServerEvent` variant, including command text, diff summary, reason, and grant_root when present.
  - Forward approval responses to Codex via `Op::ExecApproval` / `Op::PatchApproval` in the agent loop; unit test the happy path by simulating an approval event followed by a response.

- [x] **Tool output streaming & MCP enrichment**
  - Introduce new `ServerEvent` variants for streaming exec output (`ExecStdoutChunk` / `ExecStderrChunk`), reasoning section breaks, plan updates, and MCP tool results (including data from `McpToolCallEndEvent::result`).
  - Update `translate_event_msg` to map the new `EventMsg` variants, decoding payloads if necessary (e.g., base64 command output to UTF-8 best-effort).
  - Extend the CLI printer to render these new events sensibly (timestamps optional, multi-line diff handling, etc.).

- [x] **Session continuity & resume support**
  - Stop auto-sending `ClientCommand::Shutdown` after a single prompt; keep the session alive until the caller issues an explicit shutdown.
  - Surface conversation metadata via `ServerEvent::SessionConfigured { model, conversation_id, rollout_path }` so clients can persist IDs for resume.
  - Add `ClientCommand::Resume { rollout_path: PathBuf }` and `SessionHandle::resume` helper that delegates to `ConversationManager::resume_conversation_from_rollout`.
  - Extend CLI interactive mode with commands to resume existing sessions (e.g., `:resume <path>`), handling graceful shutdown on exit.
