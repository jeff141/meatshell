//! Serial-port session worker (issue #14 / #17).
//!
//! Mirrors the public surface of [`crate::ssh::spawn_session`] so the rest of
//! the UI pipeline (terminal output, key input, tab lifecycle) is reused
//! unchanged: it returns a [`SessionHandle`] plus an
//! [`UnboundedReceiver<SessionEvent>`].
//!
//! Unlike SSH there is no remote PTY, no SFTP and no resource monitor — a
//! serial line is just a raw byte pipe to a switch / router / MCU console.
//! The `serialport` crate is blocking, so the read side runs on a dedicated OS
//! thread and writes happen via `spawn_blocking`.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::Session;
use crate::i18n::t;
use crate::ssh::{SessionCommand, SessionEvent, SessionHandle};

/// Spawn a serial-port session. See module docs for why the signature mirrors
/// `spawn_session` (minus the PTY size, which a serial line has no notion of).
pub fn spawn_serial_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_serial(session, cmd_rx, evt_for_task.clone()).await {
            let _ = evt_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle {
            tab_id,
            commands: cmd_tx,
            join,
        },
        evt_rx,
    )
}

fn parse_data_bits(n: u8) -> DataBits {
    match n {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    }
}

fn parse_stop_bits(n: u8) -> StopBits {
    match n {
        2 => StopBits::Two,
        _ => StopBits::One,
    }
}

fn parse_parity(s: &str) -> Parity {
    match s {
        "odd" => Parity::Odd,
        "even" => Parity::Even,
        _ => Parity::None,
    }
}

fn parse_flow(s: &str) -> FlowControl {
    match s {
        "hardware" => FlowControl::Hardware,
        "software" => FlowControl::Software,
        _ => FlowControl::None,
    }
}

async fn run_serial(
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let port_name = session.serial_port.trim().to_string();
    if port_name.is_empty() {
        return Err(anyhow::anyhow!(t("串口号为空", "serial port is empty")));
    }

    let _ = events.send(SessionEvent::Status(format!(
        "{} {} @ {}",
        t("打开串口", "Opening serial"),
        port_name,
        session.baud_rate
    )));

    // Open on a blocking thread — serialport::open() can stall on a busy device.
    let open_name = port_name.clone();
    let baud = session.baud_rate;
    let data_bits = parse_data_bits(session.data_bits);
    let stop_bits = parse_stop_bits(session.stop_bits);
    let parity = parse_parity(&session.parity);
    let flow = parse_flow(&session.flow_control);
    let port = tokio::task::spawn_blocking(move || {
        serialport::new(&open_name, baud)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            .flow_control(flow)
            // Short read timeout so the reader thread can poll the stop flag.
            .timeout(Duration::from_millis(50))
            .open()
    })
    .await
    .context("serial open task panicked")?
    .with_context(|| format!("{} {}", t("打开串口失败", "failed to open serial port"), port_name))?;

    // A second handle for writing so the reader thread can own the read side.
    let writer = port
        .try_clone()
        .context("failed to clone serial handle for writing")?;
    let writer = Arc::new(Mutex::new(writer));

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {} @ {} {}{}{}",
        t("已连接", "Connected"),
        port_name,
        session.baud_rate,
        session.data_bits,
        parity_letter(&session.parity),
        session.stop_bits,
    )));

    // --- Reader thread ------------------------------------------------------
    let running = Arc::new(AtomicBool::new(true));
    let reader_running = running.clone();
    let reader_events = events.clone();
    let reader_handle = std::thread::spawn(move || {
        let mut port = port;
        let mut buf = [0u8; 4096];
        while reader_running.load(Ordering::Relaxed) {
            match port.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if reader_events.send(SessionEvent::Output(text)).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = reader_events.send(SessionEvent::Closed(format!(
                        "{}: {e}",
                        t("串口读取错误", "serial read error")
                    )));
                    break;
                }
            }
        }
    });

    // --- Command pump -------------------------------------------------------
    while let Some(cmd) = commands.recv().await {
        match cmd {
            SessionCommand::RawInput(bytes) => {
                // Never log keystroke bytes — they can be passwords (#15).
                tracing::debug!("serial write len={} bytes", bytes.len());
                let w = writer.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let mut guard = w.lock().unwrap();
                    guard.write_all(&bytes).and_then(|_| guard.flush())
                })
                .await;
                if let Ok(Err(e)) = res {
                    let _ = events.send(SessionEvent::Closed(format!(
                        "{}: {e}",
                        t("串口写入失败", "serial write failed")
                    )));
                    break;
                }
            }
            // A serial line has no window size; nothing to propagate.
            SessionCommand::Resize(_, _) => {}
            SessionCommand::Close => break,
        }
    }

    // Stop the reader thread and wait for it to drain.
    running.store(false, Ordering::Relaxed);
    let _ = reader_handle.join();
    let _ = events.send(SessionEvent::Closed(
        t("串口已关闭", "serial port closed").into(),
    ));
    Ok(())
}

/// Single-letter parity tag for the status line (8N1 style).
fn parity_letter(parity: &str) -> &'static str {
    match parity {
        "odd" => "O",
        "even" => "E",
        _ => "N",
    }
}
