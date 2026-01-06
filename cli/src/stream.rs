// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::errors::{format_server_error, format_status_error};
use crate::mfa::collect_mfa_answers;
use anyhow::bail;
use crossterm::{
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use proto::{
    MfaAnswer, StreamEvent, SubmitStreamEvent, stream_event, submit_result, submit_status,
    submit_stream_event,
};
use ratatui::symbols::braille;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::time::sleep;
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

pub struct MinDurationSpinner {
    spinner: Spinner,
    started_at: Instant,
    min_duration: Duration,
}

impl MinDurationSpinner {
    pub fn start(message: &str, min_duration: Duration) -> Self {
        Self {
            spinner: Spinner::start(message),
            started_at: Instant::now(),
            min_duration,
        }
    }

    pub async fn stop(self, done_message: Option<&str>) {
        let elapsed = self.started_at.elapsed();
        if elapsed < self.min_duration {
            sleep(self.min_duration - elapsed).await;
        }
        self.spinner.stop(done_message).await;
    }

    pub async fn cancel(self) {
        self.spinner.stop(None).await;
    }
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

struct ProgressSpinner {
    message: String,
    spinner: Option<Spinner>,
    started_at: Instant,
    min_duration: Duration,
}

impl ProgressSpinner {
    fn start(message: &str, min_duration: Duration) -> Self {
        Self {
            message: message.to_string(),
            spinner: Some(Spinner::start(message)),
            started_at: Instant::now(),
            min_duration,
        }
    }

    async fn pause(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop(None).await;
        }
    }

    async fn resume(&mut self) {
        if self.spinner.is_none() {
            self.spinner = Some(Spinner::start(&self.message));
        }
    }

    async fn print_output(&mut self, bytes: &[u8], stream: OutputStream) -> anyhow::Result<()> {
        self.pause().await;
        match stream {
            OutputStream::Stdout => write_all(&mut std::io::stdout(), bytes)?,
            OutputStream::Stderr => write_stderr_with_green_ticks(bytes)?,
        }
        self.resume().await;
        Ok(())
    }

    async fn stop(mut self, done_message: Option<&str>) {
        let elapsed = self.started_at.elapsed();
        if elapsed < self.min_duration {
            sleep(self.min_duration - elapsed).await;
        }
        if let Some(spinner) = self.spinner.take() {
            spinner.stop(done_message).await;
        } else if let Some(msg) = done_message {
            eprintln!("{msg}");
        }
    }

    async fn cancel(mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop(None).await;
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
                    write_stderr_with_green_ticks(&bytes)?;
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
                    eprintln!("{}", format_server_error(&err));
                    exit_code = Some(1);
                    break;
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                eprintln!("{}", format_status_error(&status));
                exit_code = Some(1);
                break;
            }
        }
    }

    Ok(exit_code)
}

pub async fn handle_stream_events_with_progress<S, F, Fut>(
    mut inbound: S,
    mut send_mfa: F,
    message: &str,
    min_duration: Duration,
) -> anyhow::Result<Option<i32>>
where
    S: Stream<Item = Result<StreamEvent, Status>> + Unpin,
    F: FnMut(MfaAnswer) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut exit_code: Option<i32> = None;
    let mut cancel_spinner = false;
    let mut spinner = ProgressSpinner::start(message, min_duration);
    while let Some(item) = inbound.next().await {
        match item {
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    if let Err(err) = spinner.print_output(&bytes, OutputStream::Stdout).await {
                        spinner.cancel().await;
                        return Err(err);
                    }
                }
                stream_event::Event::Stderr(bytes) => {
                    if let Err(err) = spinner.print_output(&bytes, OutputStream::Stderr).await {
                        spinner.cancel().await;
                        return Err(err);
                    }
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    spinner.pause().await;
                    let answers = match collect_mfa_answers(&mfa).await {
                        Ok(v) => v,
                        Err(err) => {
                            spinner.cancel().await;
                            return Err(err);
                        }
                    };
                    if let Err(err) = send_mfa(answers).await {
                        eprintln!("server closed while sending MFA answers: {err}");
                        exit_code = Some(1);
                        cancel_spinner = true;
                        break;
                    }
                    spinner.resume().await;
                }
                stream_event::Event::Error(err) => {
                    spinner.pause().await;
                    eprintln!("{}", format_server_error(&err));
                    exit_code = Some(1);
                    cancel_spinner = true;
                    break;
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                spinner.pause().await;
                eprintln!("{}", format_status_error(&status));
                exit_code = Some(1);
                cancel_spinner = true;
                break;
            }
        }
    }

    if cancel_spinner {
        spinner.cancel().await;
    } else {
        spinner.stop(None).await;
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
    S: Stream<Item = Result<SubmitStreamEvent, Status>> + Unpin,
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
            Ok(SubmitStreamEvent { event: Some(ev) }) => match ev {
                submit_stream_event::Event::Stdout(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    write_all(&mut std::io::stdout(), &bytes)?;
                }
                submit_stream_event::Event::Stderr(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                submit_stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                submit_stream_event::Event::Mfa(mfa) => {
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
                submit_stream_event::Event::Error(err) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    eprintln!("{}", format_server_error(&err));
                    exit_code = Some(1);
                    break;
                }
                submit_stream_event::Event::SubmitStatus(status) => {
                    let phase = submit_status::Phase::try_from(status.phase)
                        .unwrap_or(submit_status::Phase::Unspecified);
                    match phase {
                        submit_status::Phase::Resolved => {
                            if !printed_remote_path {
                                print_with_green_check_stdout(&format!(
                                    "Remote path: {}",
                                    status.remote_path
                                ))?;
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
                                spinner.stop(None).await;
                            }
                            print_with_green_check_stderr("Data transfer complete.")?;
                        }
                        submit_status::Phase::Unspecified => {}
                    }
                }
                submit_stream_event::Event::SubmitResult(result) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    let status = submit_result::Status::try_from(result.status)
                        .unwrap_or(submit_result::Status::Unspecified);
                    match status {
                        submit_result::Status::Submitted => {
                            if let Some(job_id) = result.job_id {
                                print_with_green_check_stdout(&format!("Job {job_id} submitted!"))?;
                                println!("Check job status with 'hpc job get {job_id}'");
                            } else {
                                println!("Successfully submitted sbatch script.");
                            }
                            exit_code = Some(0);
                        }
                        submit_result::Status::Failed => {
                            let detail = result.detail.trim();
                            if detail.is_empty() {
                                eprintln!("failed");
                            } else {
                                eprintln!("failed: {}", format_server_error(detail));
                            }
                            exit_code = Some(1);
                        }
                        submit_result::Status::Unspecified => {
                            eprintln!("failed: submit result missing status");
                            exit_code = Some(1);
                        }
                    }
                    break;
                }
            },
            Ok(SubmitStreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop(None).await;
                }
                eprintln!("{}", format_status_error(&status));
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

fn write_stderr_with_green_ticks(bytes: &[u8]) -> anyhow::Result<()> {
    let mut stderr = std::io::stderr();
    if !stderr.is_terminal() {
        return write_all(&mut stderr, bytes);
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(v) => v,
        Err(_) => return write_all(&mut stderr, bytes),
    };
    if !text.contains('✓') {
        return write_all(&mut stderr, bytes);
    }
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix('✓') {
            execute!(
                stderr,
                SetForegroundColor(Color::Green),
                Print("✓"),
                ResetColor,
                Print(rest)
            )?;
        } else {
            write_all(&mut stderr, line.as_bytes())?;
        }
    }
    Ok(())
}

fn write_stdout_with_green_ticks(bytes: &[u8]) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return write_all(&mut stdout, bytes);
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(v) => v,
        Err(_) => return write_all(&mut stdout, bytes),
    };
    if !text.contains('✓') {
        return write_all(&mut stdout, bytes);
    }
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix('✓') {
            execute!(
                stdout,
                SetForegroundColor(Color::Green),
                Print("✓"),
                ResetColor,
                Print(rest)
            )?;
        } else {
            write_all(&mut stdout, line.as_bytes())?;
        }
    }
    Ok(())
}

pub fn print_with_green_check_stdout(message: &str) -> anyhow::Result<()> {
    let mut line = String::from("✓ ");
    line.push_str(message);
    line.push('\n');
    write_stdout_with_green_ticks(line.as_bytes())
}

pub fn print_with_green_check_stderr(message: &str) -> anyhow::Result<()> {
    let mut line = String::from("✓ ");
    line.push_str(message);
    line.push('\n');
    write_stderr_with_green_ticks(line.as_bytes())
}

fn write_all<W: Write>(w: &mut W, buf: &[u8]) -> anyhow::Result<()> {
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}
