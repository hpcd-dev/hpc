use crate::mfa::collect_mfa_answers;
use anyhow::bail;
use proto::{MfaAnswer, StreamEvent, stream_event, submit_status};
use ratatui::symbols::braille;
use std::future::Future;
use std::io::Write;
use tokio::time::Duration;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;

pub struct Spinner {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

fn braille_spinner_frames() -> [char; 6] {
    let all_mask = braille::DOTS
        .iter()
        .take(3)
        .flatten()
        .fold(0u16, |acc, mask| acc | mask);
    let order = [(0usize, 0usize), (0, 1), (1, 1), (2, 1), (2, 0), (1, 0)];
    let mut frames = [' '; 6];
    for (idx, (row, col)) in order.iter().enumerate() {
        let mask = braille::DOTS[*row][*col];
        let codepoint = u32::from(braille::BLANK | (all_mask & !mask));
        frames[idx] = char::from_u32(codepoint).unwrap_or(' ');
    }
    frames
}

impl Spinner {
    pub fn start(message: &str) -> Self {
        let message = message.to_string();
        let frames = braille_spinner_frames();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn({
            let message = message.clone();
            async move {
                let mut interval = tokio::time::interval(Duration::from_millis(120));
                let mut i = 0usize;
                tokio::pin!(stop_rx);
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        _ = interval.tick() => {
                            let frame = frames[i % frames.len()];
                            eprint!("\r{} {}", message, frame);
                            let _ = std::io::stderr().flush();
                            i += 1;
                        }
                    }
                }
                let clear = " ".repeat(message.chars().count() + 2);
                eprint!("\r{clear}\r");
                let _ = std::io::stderr().flush();
            }
        });
        Spinner {
            stop_tx: Some(stop_tx),
            handle,
        }
    }

    pub async fn stop(mut self, done_message: Option<&str>) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
        if let Some(msg) = done_message {
            eprintln!("{msg}");
        }
    }
}

pub async fn handle_stream_events<S, F, Fut>(
    mut inbound: S,
    mut send_mfa: F,
) -> anyhow::Result<Option<i32>>
where
    S: Stream<Item = Result<StreamEvent, Status>> + Unpin,
    F: FnMut(MfaAnswer) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut exit_code: Option<i32> = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    write_all(&mut std::io::stdout(), &bytes)?;
                }
                stream_event::Event::Stderr(bytes) => {
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    let answers = collect_mfa_answers(&mfa).await?;
                    if let Err(err) = send_mfa(answers).await {
                        eprintln!("server closed while sending MFA answers: {err}");
                        exit_code = Some(1);
                        break;
                    }
                }
                stream_event::Event::Error(err) => {
                    eprintln!("server error: {err}");
                    exit_code = Some(1);
                    break;
                }
                stream_event::Event::SubmitStatus(_) => {
                    // submit-specific events are handled in a different stream handler
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                eprintln!("stream error: {}", status);
                exit_code = Some(1);
                break;
            }
        }
    }

    Ok(exit_code)
}

pub enum SubmitStreamOutcome {
    Completed(Option<i32>),
    Canceled,
}

pub async fn handle_submit_stream_events<S, F, Fut>(
    mut inbound: S,
    mut send_mfa: F,
) -> anyhow::Result<SubmitStreamOutcome>
where
    S: Stream<Item = Result<StreamEvent, Status>> + Unpin,
    F: FnMut(MfaAnswer) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut exit_code: Option<i32> = None;
    let mut spinner: Option<Spinner> = None;
    let mut printed_remote_path = false;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        let item = tokio::select! {
            _ = &mut ctrl_c => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop(None).await;
                }
                return Ok(SubmitStreamOutcome::Canceled);
            }
            item = inbound.next() => item,
        };
        let Some(item) = item else {
            break;
        };
        match item {
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    write_all(&mut std::io::stdout(), &bytes)?;
                }
                stream_event::Event::Stderr(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    let answers = collect_mfa_answers(&mfa).await?;
                    if let Err(err) = send_mfa(answers).await {
                        eprintln!("server closed while sending MFA answers: {err}");
                        exit_code = Some(1);
                        break;
                    }
                }
                stream_event::Event::Error(err) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    eprintln!("server error: {err}");
                    exit_code = Some(1);
                    break;
                }
                stream_event::Event::SubmitStatus(status) => {
                    let phase = submit_status::Phase::try_from(status.phase)
                        .unwrap_or(submit_status::Phase::Unspecified);
                    match phase {
                        submit_status::Phase::Resolved => {
                            if !printed_remote_path {
                                println!("remote target: {}", status.remote_path);
                                printed_remote_path = true;
                            }
                        }
                        submit_status::Phase::TransferStart => {
                            if spinner.is_none() {
                                spinner = Some(Spinner::start("Transferring files"));
                            }
                        }
                        submit_status::Phase::TransferDone => {
                            if let Some(spinner) = spinner.take() {
                                spinner.stop(Some("Transfer complete.")).await;
                            }
                        }
                        submit_status::Phase::Unspecified => {}
                    }
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop(None).await;
                }
                eprintln!("stream error: {}", status);
                exit_code = Some(1);
                break;
            }
        }
    }

    if let Some(spinner) = spinner.take() {
        spinner.stop(None).await;
    }

    Ok(SubmitStreamOutcome::Completed(exit_code))
}

pub fn ensure_exit_code(exit_code: Option<i32>, context: &str) -> anyhow::Result<()> {
    if let Some(num) = exit_code {
        if num != 0 {
            bail!("{context} {num}");
        }
    }
    Ok(())
}

fn write_all<W: Write>(w: &mut W, buf: &[u8]) -> anyhow::Result<()> {
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}
