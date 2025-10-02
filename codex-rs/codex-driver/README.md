# codex-driver

Experimental headless driver for the Codex agent. This crate exposes a library
API and a companion binary (`headless-codex`) that can be used to send prompts
and stream Codex events without the interactive TUI.

## Features

- Channel-based interface (`ClientCommand`/`ServerEvent`) suitable for
  embedding in other applications.
- Danger-mode defaults (`approval_policy = never`, `sandbox = danger-full-access`).
- Minimal streaming output via the `headless-codex` CLI, including an
  experimental interactive mode.

## Usage

Add the crate to your workspace (it already lives in this repository) and use
`SessionHandle::start` to spin up a session:

```rust,no_run
use codex_driver::prelude::*;
use crossbeam_channel::RecvTimeoutError;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let SessionStartup {
        mut handle,
        command_tx,
        event_rx,
    } = SessionHandle::start(SessionConfig::default())?;

    command_tx.send(ClientCommand::SubmitUserMessage {
        text: "Hello, Codex!".into(),
        images: Vec::new(),
    })?;
    command_tx.send(ClientCommand::Shutdown)?;

    for event in event_rx.iter() {
        println!("event: {event:?}");
        if matches!(event, ServerEvent::SessionEnded) {
            break;
        }
    }

    handle.shutdown();
    Ok(())
}
```

See [`examples/basic.rs`](examples/basic.rs) for a complete example.

## CLI

The `headless-codex` binary wraps the library API. Run a single prompt:

```bash
cargo run -p codex-driver --bin headless-codex -- --prompt "Hello Codex!"
```

Enter interactive mode:

```bash
cargo run -p codex-driver --bin headless-codex -- --interactive
```

## Safety Note

This driver currently forces Codex into "danger" mode (no sandbox/approvals).
Use only in controlled environments.
