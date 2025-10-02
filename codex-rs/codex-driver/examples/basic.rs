use codex_driver::prelude::*;
use crossbeam_channel::RecvTimeoutError;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let SessionStartup {
        handle,
        command_tx,
        event_rx,
    } = SessionHandle::start(SessionConfig::default())?;

    command_tx.send(ClientCommand::SubmitUserMessage {
        text: "Hello from example".into(),
        images: Vec::new(),
    })?;
    command_tx.send(ClientCommand::Shutdown)?;

    loop {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => {
                println!("event: {event:?}");
                if matches!(event, ServerEvent::SessionEnded) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    handle.shutdown();
    Ok(())
}
