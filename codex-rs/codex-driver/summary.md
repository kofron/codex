**Workspace Architecture**
- `core/src/lib.rs:1` hosts `codex-core`, the engine that spins up Codex sessions, streams backend events, enforces sandbox/approval policies, coordinates tools (exec, apply_patch, plan), tracks rollouts, and exposes `ConversationManager`, `CodexConversation`, and telemetry hooks used across the workspace.
- Frontends layer on top of that core: the multitool CLI in `cli/src/main.rs:1` dispatches to the fullscreen Ratatui client (`tui/src/lib.rs:1`), the scripted/JSON emitter (`exec/src/lib.rs:1`), login workflows, cloud tooling, and debug helpers; `codex-driver/src/lib.rs:1` packages the same machinery into a headless library plus `headless-codex` binary with channel-based IO.
- `app-server/src/lib.rs:1` runs a JSON-RPC bridge that lets external processes talk to Codex via stdin/stdout using the schema defined in `app-server-protocol/src/lib.rs:1`, while `protocol/src/lib.rs:1` (and `protocol-ts/src/lib.rs:1`) define the shared event/command types for both internal Rust crates and generated TypeScript clients.

**Execution, Tooling, and Sandbox Layers**
- Shell and patch execution are mediated through dedicated crates: `apply-patch/src/lib.rs:1` parses and verifies diffs, `exec/src/event_processor.rs` (via `exec_events.rs`) renders streaming command output, and `git-tooling/src/lib.rs:1` supplies ghost-commit helpers for safe diffing.
- Platform hardening lives in `linux-sandbox` and `execpolicy` for Landlock/seccomp, with arg0 dispatch (`arg0/src/lib.rs:1`) ensuring the single binary can masquerade as `codex-linux-sandbox`, `apply_patch`, or the primary CLI while loading dotenv and configuring `PATH`.
- Fuzzy workspace search (`file-search/src/lib.rs:1`), MCP transport (`mcp-client/src/mcp_client.rs:1`, `mcp-server`), and headless exec plan updates (plan tool plumbing in `core/src/plan_tool.rs`) round out the agent toolchain.

**Authentication, Remote Services, and Cloud**
- Auth flows are centralized in `core/src/auth.rs` and re-exported through `login/src/lib.rs:1`; the CLI surfaces login/logout/device-code commands, and `chatgpt/src/chatgpt_client.rs:1` wraps direct ChatGPT backend calls using stored credentials.
- `backend-client/src/client.rs:1`, `cloud-tasks/src/lib.rs:1`, `responses-api-proxy/src/lib.rs`, and `app-server` provide HTTP/JSON-RPC clients and TUIs for Codex Cloud task browsing, apply/preflight, and response streaming.
- OSS mode and local model management rely on `ollama/src/lib.rs:1`, which checks connectivity to a local Ollama daemon and pulls the default `gpt-oss:20b` model when needed.

**Shared Utilities and Observability**
- `common/src/lib.rs:1` houses CLI config override parsing, sandbox/approval presets, fuzzy matching, and elapsed-time helpers reused across binaries.
- Telemetry and logging setup flows from `otel/src/lib.rs:1` and `core/src/otel_init.rs`, while `codex-core`’s `rollout` module records per-session transcripts for resume/backtrack features.
- Markdown rendering (`tui/src/markdown_render.rs`), onboarding/trust screens, plan visualization, and streaming widgets in the Ratatui client all consume the protocol events emitted by the core engine.

Together the workspace delivers a full Rust implementation of the Codex assistant: a hardened core agent runner, multiple user-facing shells (interactive TUI, scripted exec, headless driver), integration points for cloud services and MCP servers, and a wide set of supporting crates that manage authentication, sandboxing, diff application, fuzzy search, and telemetry.
