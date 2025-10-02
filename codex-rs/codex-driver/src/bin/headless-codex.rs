use std::io::Read;
use std::io::{self};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::ArgAction;
use clap::Parser;
use codex_driver::prelude::*;
use crossbeam_channel::Receiver as EventReceiver;
use crossbeam_channel::Sender as CommandSender;

/// Experimental headless Codex interface.
#[derive(Parser, Debug)]
#[command(name = "headless-codex")]
#[command(about = "Run a single Codex turn without the TUI", long_about = None)]
struct Cli {
    /// Prompt text to send to the driver. If omitted, stdin is read until EOF.
    #[arg(long, short, action = ArgAction::Set, value_name = "TEXT")]
    prompt: Option<String>,

    /// Model slug to use for the session.
    #[arg(long)]
    model: Option<String>,

    /// Whether to run with the built-in OSS model provider.
    #[arg(long)]
    oss: bool,

    /// Optional configuration profile name.
    #[arg(long = "config-profile")]
    config_profile: Option<String>,

    /// Apply raw `-c` style configuration overrides.
    #[arg(short = 'c', long = "config-override", action = ArgAction::Append)]
    config_override: Vec<String>,

    /// Working directory to associate with the session.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Placeholder for future interactive mode.
    #[arg(long, action = ArgAction::SetTrue)]
    interactive: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let session_config = SessionConfig {
        cwd: cli.cwd,
        model: cli.model,
        config_profile: cli.config_profile,
        raw_cli_overrides: cli.config_override,
        use_oss: cli.oss,
    };
    let startup = match SessionHandle::start(session_config.clone()) {
        Ok(startup) => startup,
        Err(err) => {
            eprintln!("failed to start session: {err}");
            return ExitCode::from(1);
        }
    };

    if cli.interactive {
        run_interactive(startup, session_config)
    } else {
        let SessionStartup {
            handle,
            command_tx,
            event_rx,
        } = startup;
        run_single_turn(handle, command_tx, event_rx, cli.prompt)
    }
}

fn handle_event(event: &ServerEvent) -> Option<ExitCode> {
    match event {
        ServerEvent::SessionConfigured {
            model,
            conversation_id,
            rollout_path,
        } => {
            println!(
                "[session-configured] model={model} conversation_id={} rollout_path={}",
                conversation_id,
                rollout_path.display()
            );
        }
        ServerEvent::AgentMessageDelta(text) => {
            println!("[agent-delta] {text}");
        }
        ServerEvent::AgentMessageFinal(text) => {
            println!("[agent-final] {text}");
        }
        ServerEvent::AgentReasoningDelta(text) => {
            println!("[reasoning-delta] {text}");
        }
        ServerEvent::AgentReasoningFinal(text) => {
            println!("[reasoning-final] {text}");
        }
        ServerEvent::ExecCommandBegin { call_id, command } => {
            println!("[exec-begin] id={call_id} command={:?}", command);
        }
        ServerEvent::ExecCommandEnd { call_id, exit_code } => {
            println!("[exec-end] id={call_id} exit={:?}", exit_code);
        }
        ServerEvent::PatchApplyBegin {
            call_id,
            auto_approved,
        } => {
            println!("[patch-begin] id={call_id} auto_approved={auto_approved}");
        }
        ServerEvent::PatchApplyEnd { call_id, success } => {
            println!("[patch-end] id={call_id} success={success}");
        }
        ServerEvent::McpToolCallBegin {
            call_id,
            server,
            tool,
        } => {
            println!("[mcp-begin] id={call_id} server={server} tool={tool}");
        }
        ServerEvent::McpToolCallEnd {
            call_id,
            server,
            tool,
            duration,
            result,
        } => {
            let outcome = match result {
                Ok(res) => format!("ok blocks={}", res.content.len()),
                Err(err) => format!("err={err}"),
            };
            println!(
                "[mcp-end] id={call_id} server={server} tool={tool} duration={:?} result={outcome}",
                duration
            );
        }
        ServerEvent::ApprovalRequest { id, request } => match request {
            ApprovalRequest::Exec {
                call_id,
                command,
                cwd,
                reason,
            } => {
                println!(
                    "[approval-needed] id={id} kind=exec call_id={call_id} cwd={} command={:?} reason={}",
                    cwd.display(),
                    command,
                    reason.as_deref().unwrap_or("<none>")
                );
            }
            ApprovalRequest::Patch {
                call_id,
                changes,
                reason,
                grant_root,
            } => {
                println!(
                    "[approval-needed] id={id} kind=patch call_id={call_id} files={} reason={} grant_root={}",
                    changes.len(),
                    reason.as_deref().unwrap_or("<none>"),
                    grant_root
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                );
            }
        },
        ServerEvent::ExecOutputChunk {
            call_id,
            stream,
            text,
        } => {
            println!("[exec-output] id={call_id} stream={stream:?} {text}");
        }
        ServerEvent::ReasoningSectionBreak => {
            println!("[reasoning-break]");
        }
        ServerEvent::WebSearchBegin { call_id } => {
            println!("[web-search-begin] id={call_id}");
        }
        ServerEvent::WebSearchEnd { call_id, query } => {
            println!("[web-search-end] id={call_id} query={query}");
        }
        ServerEvent::TokenUsage {
            blended_total,
            input,
            cached_input,
            output,
            reasoning_output,
        } => {
            println!(
                "[token-usage] total={blended_total} input={input} cached_input={cached_input} output={output} reasoning={reasoning_output}"
            );
        }
        ServerEvent::TurnComplete { last_message } => {
            println!("[turn-complete] last_message={:?}", last_message);
        }
        ServerEvent::TurnAborted { reason } => {
            println!("[turn-aborted] reason={reason}");
        }
        ServerEvent::TurnDiff { unified_diff } => {
            println!("[turn-diff]\n{unified_diff}");
        }
        ServerEvent::PlanUpdate(update) => {
            println!("[plan-update] explanation={:?}", update.explanation);
            for step in &update.steps {
                println!("  - [{}] {}", format_plan_status(step.status), step.step);
            }
        }
        ServerEvent::BackgroundMessage(message) => {
            println!("[background] {message}");
        }
        ServerEvent::StreamError(message) => {
            eprintln!("[stream-error] {message}");
        }
        ServerEvent::ShutdownComplete => {
            println!("[shutdown-complete]");
        }
        ServerEvent::SessionEnded => {
            println!("[session-ended]");
        }
        ServerEvent::Fatal { message } => {
            eprintln!("[fatal] {message}");
            return Some(ExitCode::from(1));
        }
    }
    None
}

fn format_plan_status(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "pending",
        PlanStepStatus::InProgress => "in-progress",
        PlanStepStatus::Completed => "completed",
    }
}

fn run_single_turn(
    handle: SessionHandle,
    command_tx: CommandSender<ClientCommand>,
    event_rx: EventReceiver<ServerEvent>,
    prompt: Option<String>,
) -> ExitCode {
    let prompt = match prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            if io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("failed to read prompt from stdin");
                return ExitCode::from(1);
            }
            buf
        }
    };
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        eprintln!("prompt is empty; supply --prompt or stdin input");
        return ExitCode::from(1);
    }

    if command_tx
        .send(ClientCommand::SubmitUserMessage {
            text: prompt,
            images: Vec::new(),
        })
        .is_err()
    {
        eprintln!("failed to send prompt to Codex");
        return ExitCode::from(1);
    }
    let _ = command_tx.send(ClientCommand::Shutdown);
    drop(command_tx);

    let exit_code = pump_events(event_rx);
    handle.shutdown();
    exit_code
}

fn run_interactive(startup: SessionStartup, _session_config: SessionConfig) -> ExitCode {
    println!("Entering interactive mode. Submit prompts and press Ctrl-D to exit.");
    let SessionStartup {
        handle,
        command_tx,
        event_rx,
    } = startup;

    let mut exit_code = ExitCode::SUCCESS;
    let mut session_active = true;
    let mut buffer = String::new();

    loop {
        buffer.clear();
        print!("codex> ");
        let _ = io::Write::flush(&mut io::stdout());
        if io::stdin().read_line(&mut buffer).expect("stdin") == 0 {
            break;
        }
        let prompt = buffer.trim();
        if prompt.is_empty() {
            continue;
        }

        if let Some(rest) = prompt.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(path) = rest.strip_prefix("resume") {
                let rollout = path.trim();
                if rollout.is_empty() {
                    eprintln!("usage: :resume <rollout-path>");
                    continue;
                }
                if command_tx
                    .send(ClientCommand::Resume {
                        rollout_path: PathBuf::from(rollout),
                    })
                    .is_err()
                {
                    eprintln!("failed to send resume command");
                    exit_code = ExitCode::from(1);
                    session_active = false;
                    break;
                }
                let outcome = wait_for_session_config(&event_rx);
                if let Some(code) = outcome.exit_code {
                    exit_code = code;
                }
                if outcome.session_ended {
                    session_active = false;
                    break;
                }
                continue;
            }
            if rest.trim() == "quit" {
                break;
            }
            eprintln!("unknown command: {prompt}");
            continue;
        }

        if command_tx
            .send(ClientCommand::SubmitUserMessage {
                text: prompt.to_string(),
                images: Vec::new(),
            })
            .is_err()
        {
            eprintln!("failed to send prompt");
            exit_code = ExitCode::from(1);
            session_active = false;
            break;
        }

        let outcome = run_turn(&event_rx);
        if let Some(code) = outcome.exit_code {
            exit_code = code;
        }
        if outcome.session_ended {
            session_active = false;
            break;
        }
    }

    if session_active {
        let _ = command_tx.send(ClientCommand::Shutdown);
        let code = pump_events(event_rx);
        if code != ExitCode::SUCCESS {
            exit_code = code;
        }
    } else {
        drop(event_rx);
    }

    handle.shutdown();
    exit_code
}

fn pump_events(receiver: EventReceiver<ServerEvent>) -> ExitCode {
    let mut exit_code = ExitCode::SUCCESS;
    for event in receiver.iter() {
        if let Some(code) = handle_event(&event) {
            exit_code = code;
        }
        if matches!(event, ServerEvent::SessionEnded) {
            break;
        }
    }
    exit_code
}

struct PumpOutcome {
    exit_code: Option<ExitCode>,
    session_ended: bool,
}

fn run_turn(receiver: &EventReceiver<ServerEvent>) -> PumpOutcome {
    let mut exit_code = None;
    loop {
        match receiver.recv() {
            Ok(event) => {
                if let Some(code) = handle_event(&event) {
                    exit_code = Some(code);
                }
                match event {
                    ServerEvent::TurnComplete { .. } => {
                        return PumpOutcome {
                            exit_code,
                            session_ended: false,
                        };
                    }
                    ServerEvent::SessionEnded => {
                        return PumpOutcome {
                            exit_code: exit_code.or(Some(ExitCode::SUCCESS)),
                            session_ended: true,
                        };
                    }
                    _ => {}
                }
            }
            Err(_) => {
                return PumpOutcome {
                    exit_code: Some(ExitCode::from(1)),
                    session_ended: true,
                };
            }
        }
    }
}

fn wait_for_session_config(receiver: &EventReceiver<ServerEvent>) -> PumpOutcome {
    let mut exit_code = None;
    loop {
        match receiver.recv() {
            Ok(event) => {
                if let Some(code) = handle_event(&event) {
                    exit_code = Some(code);
                }
                match event {
                    ServerEvent::SessionConfigured { .. } => {
                        return PumpOutcome {
                            exit_code,
                            session_ended: false,
                        };
                    }
                    ServerEvent::SessionEnded => {
                        return PumpOutcome {
                            exit_code: exit_code.or(Some(ExitCode::SUCCESS)),
                            session_ended: true,
                        };
                    }
                    _ => {}
                }
            }
            Err(_) => {
                return PumpOutcome {
                    exit_code: Some(ExitCode::from(1)),
                    session_ended: true,
                };
            }
        }
    }
}
