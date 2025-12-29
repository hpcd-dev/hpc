use anyhow::bail;
use clap::{ArgGroup, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal,
};
use proto::agent_client::AgentClient;
use proto::{
    AddClusterInit, AddClusterRequest, ListClustersRequest, ListClustersResponse,
    ListClustersUnitResponse, ListJobsRequest, ListJobsResponse, ListJobsUnitResponse, LsRequest,
    LsRequestInit, MfaAnswer, MfaPrompt, PingRequest, RetrieveJobRequest, RetrieveJobRequestInit,
    StreamEvent, SubmitPathFilterAction, SubmitPathFilterRule, SubmitRequest, add_cluster_init,
    add_cluster_request, stream_event, submit_status,
};
use ratatui::{
    Terminal,
    TerminalOptions,
    Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position},
    prelude::Frame,
    style::{Color, Modifier, Style},
    symbols::braille,
    widgets::{Clear, List, ListItem, ListState, Paragraph},
};
use serde::Deserialize;
use serde_json::json;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tokio::io;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, transport::Channel};
use tonic_types::StatusExt;

mod project;
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Ping,
    Ls(LsArgs),
    Submit(SubmitArgs),
    Init(InitProjectArgs),
    Jobs(JobsArgs),
    Clusters(ClustersArgs),
}
#[derive(clap::ValueEnum, Clone, Default, Debug, serde::Serialize, Deserialize)]
enum WLM {
    #[default]
    Slurm,
}

impl ToString for WLM {
    fn to_string(&self) -> String {
        match self {
            Self::Slurm => "slurm".to_owned(),
        }
    }
}

#[derive(Args, Debug)]
struct InitProjectArgs {
    // path to project to be initialized
    projectpath: PathBuf,
    // Workload Manager to use
    #[arg(long, default_value_t=WLM::Slurm)]
    manager: WLM,
    // force init project: ignore directory if exists, make sure that all directories within the
    // path
    #[arg(long)]
    force: bool,

    #[arg(long)]
    mem: Option<crate::project::hpcfile::Bytes>,

    #[arg(long)]
    cpus: Option<u32>,

    #[arg(long)]
    account: Option<String>,

    #[arg(long)]
    time: Option<String>,
}

#[derive(Args, Debug)]
struct JobsArgs {
    #[command(subcommand)]
    cmd: JobsCmd,
}

#[derive(Subcommand, Debug)]
enum JobsCmd {
    /// List jobs.
    List(ListJobsArgs),
    /// Show job details.
    Get(JobGetArgs),
    /// Retrieve a file or directory from a job run folder.
    Retrieve(JobRetrieveArgs),
}

#[derive(Args, Debug)]
struct JobGetArgs {
    /// Job id from the daemon.
    job_id: i64,
    #[arg(long)]
    cluster: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ListJobsArgs {
    #[arg(long)]
    cluster: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct JobRetrieveArgs {
    /// Job id from the daemon.
    job_id: i64,
    path: String,
    #[arg(long)]
    dest: Option<PathBuf>,
    #[arg(long)]
    cluster: Option<String>,
}

#[derive(Args, Debug)]
struct ListClustersArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ClustersArgs {
    #[command(subcommand)]
    cmd: ClustersCmd,
}

#[derive(Subcommand, Debug)]
enum ClustersCmd {
    /// List clusters.
    List(ListClustersArgs),
    /// Show cluster details.
    Get(ClusterGetArgs),
    /// Add a new cluster.
    Add(AddClusterArgs),
    /// Update cluster parameters.
    Set(AddClusterArgs),
}

#[derive(Args, Debug)]
struct ClusterGetArgs {
    hostid: String,
    #[arg(long)]
    json: bool,
}
#[derive(Args, Debug)]
struct LsArgs {
    hostid: String,
    path: Option<String>,
}
#[derive(Args, Debug)]
struct SubmitArgs {
    hostid: String,
    local_path: String,
    sbatchscript: Option<String>,
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    remote_path: Option<String>,
    /// Include paths matching PATTERN.
    /// Rules are checked in the order they appear across --include/--exclude;
    /// the first match wins, and unmatched paths are included.
    /// Patterns match the path relative to the submit root with '/' separators.
    /// A pattern without '/' matches the basename anywhere; a pattern with '/'
    /// but no leading '/' is treated as `**/PATTERN`.
    /// A leading '/' anchors to the root, and a trailing '/' matches directories
    /// only (and prunes their contents). Globs support `*`, `?`, `**`, `[]`, `{}`.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    include: Vec<String>,
    /// Exclude paths matching PATTERN.
    /// Rules are checked in the order they appear across --include/--exclude;
    /// the first match wins, and unmatched paths are included.
    /// Patterns match the path relative to the submit root with '/' separators.
    /// A pattern without '/' matches the basename anywhere; a pattern with '/'
    /// but no leading '/' is treated as `**/PATTERN`.
    /// A leading '/' anchors to the root, and a trailing '/' matches directories
    /// only (and prunes their contents). Globs support `*`, `?`, `**`, `[]`, `{}`.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    exclude: Vec<String>,
}

#[derive(Args, Debug)]
#[command(
    group(
        ArgGroup::new("addcluster")
            .required(true)      // at least one is required...
            .multiple(false)     // ...and they are mutually exclusive
            .args(&["hostname", "ip"])
    )
)]
struct AddClusterArgs {
    #[arg(long, value_name = "HOSTNAME")]
    hostname: Option<String>,

    /// Use a remote URL as input
    #[arg(long, value_name = "IP")]
    ip: Option<String>,

    #[arg(long)]
    username: String,

    #[arg(long)]
    hostid: String,

    #[arg(long, default_value_t = 22)]
    port: u32,

    #[arg(long, default_value = "~/.ssh/id_ed25519")]
    identity_path: String,

    #[arg(long)]
    default_base_path: Option<String>,
}

async fn send_ping(client: &mut AgentClient<Channel>) -> anyhow::Result<()> {
    let ping_request = PingRequest {
        message: "ping".into(),
    };
    let response = match timeout(Duration::from_secs(1), client.ping(ping_request)).await {
        Ok(res) => match res {
            Ok(response) => response,
            Err(status) => match status.code() {
                tonic::Code::InvalidArgument => {
                    bail!("invalid argument: {}", status.message());
                }
                tonic::Code::Cancelled => {
                    bail!("operation was canceled:{}", status.message());
                }
                _ => {
                    bail!("error occured: {}", status);
                }
            },
        },
        Err(elapsed) => bail!("Cancelled request after {elapsed} seconds"),
    };
    let message = response.get_ref().to_owned().message;
    match message.as_str() {
        "pong" => Ok(()),
        v => bail!("invalid response from server: expected 'pong', got '{v}'"),
    }
}
async fn fetch_list_clusters(
    client: &mut AgentClient<Channel>,
    filter: &str,
) -> anyhow::Result<ListClustersResponse> {
    let list_clusters_request = ListClustersRequest {
        filter: filter.to_string(),
    };
    let response = match timeout(
        Duration::from_secs(1),
        client.list_clusters(list_clusters_request),
    )
    .await
    {
        Ok(Ok(res)) => res.into_inner(),
        Ok(Err(status)) => match status.code() {
            tonic::Code::InvalidArgument => {
                bail!("invalid argument: '{}'", status.message())
            }
            tonic::Code::Internal => {
                bail!("internal error: '{}'", status.message())
            }
            _ => {
                bail!(
                    "error encountered: {} - '{}'",
                    status.code(),
                    status.message()
                )
            }
        },
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

fn cluster_host_string(item: &ListClustersUnitResponse) -> String {
    match item.host {
        Some(ref v) => match v {
            proto::list_clusters_unit_response::Host::Hostname(s) => s.to_string(),
            proto::list_clusters_unit_response::Host::Ipaddr(s) => s.to_string(),
        },
        None => "<unknown>".to_string(),
    }
}

fn cluster_to_json(item: &ListClustersUnitResponse) -> serde_json::Value {
    let status = match item.connected {
        true => "connected",
        false => "disconnected",
    };
    json!({
        "hostid": item.hostid.as_str(),
        "username": item.username.as_str(),
        "address": cluster_host_string(item),
        "port": item.port,
        "connected": item.connected,
        "status": status,
        "identity_path": item.identity_path.as_deref(),
        "accounting_available": item.accounting_available,
    })
}

fn job_to_json(item: &ListJobsUnitResponse) -> serde_json::Value {
    let status = job_status(item);
    json!({
        "job_id": item.job_id,
        "slurm_id": item.slurm_id,
        "hostid": item.hostid.as_str(),
        "status": status,
        "is_completed": item.is_completed,
        "terminal_state": item.terminal_state.as_deref(),
        "created_at": item.created_at.as_str(),
        "finished_at": item.finished_at.as_deref(),
    })
}

fn emit_json(value: serde_json::Value) -> anyhow::Result<()> {
    let output = serde_json::to_string_pretty(&value)?;
    println!("{output}");
    Ok(())
}

fn str_width(value: &str) -> usize {
    value.chars().count()
}

fn print_clusters_table(clusters: &[ListClustersUnitResponse]) {
    let headers = ["username", "hostid", "address", "port", "status", "accounting"];
    let mut rows: Vec<(String, String, String, String, String, String)> = Vec::new();

    for item in clusters.iter() {
        let host_str = cluster_host_string(item);
        let connected_str = match item.connected {
            true => "connected",
            false => "disconnected",
        };
        let accounting_str = match item.accounting_available {
            true => "enabled",
            false => "disabled",
        };
        rows.push((
            item.username.clone(),
            item.hostid.clone(),
            host_str,
            item.port.to_string(),
            connected_str.to_string(),
            accounting_str.to_string(),
        ));
    }

    let mut widths: [usize; 6] = [
        str_width(headers[0]),
        str_width(headers[1]),
        str_width(headers[2]),
        str_width(headers[3]),
        str_width(headers[4]),
        str_width(headers[5]),
    ];
    for row in rows.iter() {
        widths[0] = widths[0].max(str_width(&row.0));
        widths[1] = widths[1].max(str_width(&row.1));
        widths[2] = widths[2].max(str_width(&row.2));
        widths[3] = widths[3].max(str_width(&row.3));
        widths[4] = widths[4].max(str_width(&row.4));
        widths[5] = widths[5].max(str_width(&row.5));
    }

    println!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5]
    );

    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}",
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            row.5,
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5]
        );
    }
}

fn print_clusters_json(clusters: &[ListClustersUnitResponse]) -> anyhow::Result<()> {
    let data: Vec<serde_json::Value> = clusters.iter().map(cluster_to_json).collect();
    emit_json(serde_json::Value::Array(data))
}

fn print_cluster_details(item: &ListClustersUnitResponse) {
    let host_str = cluster_host_string(item);
    let connected_str = match item.connected {
        true => "connected",
        false => "disconnected",
    };
    let accounting_str = match item.accounting_available {
        true => "enabled",
        false => "disabled",
    };
    println!("hostid: {}", item.hostid);
    println!("username: {}", item.username);
    println!("address: {}", host_str);
    println!("port: {}", item.port);
    println!("status: {}", connected_str);
    println!("accounting: {}", accounting_str);
    println!(
        "identity_path: {}",
        item.identity_path.as_deref().unwrap_or("-")
    );
}

fn print_cluster_details_json(item: &ListClustersUnitResponse) -> anyhow::Result<()> {
    emit_json(cluster_to_json(item))
}

async fn fetch_list_jobs(
    client: &mut AgentClient<Channel>,
    cluster: Option<String>,
) -> anyhow::Result<ListJobsResponse> {
    let list_jobs_request = ListJobsRequest { hostid: cluster };
    let response = match timeout(Duration::from_secs(5), client.list_jobs(list_jobs_request)).await
    {
        Ok(Ok(res)) => res.into_inner(),
        Ok(Err(status)) => match status.code() {
            tonic::Code::InvalidArgument => {
                bail!("invalid argument: '{}'", status.message())
            }
            tonic::Code::Internal => {
                bail!("internal error: '{}'", status.message())
            }
            _ => {
                bail!(
                    "error encountered: {} - '{}'",
                    status.code(),
                    status.message()
                )
            }
        },
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

fn print_jobs_table(jobs: &[ListJobsUnitResponse]) {
    let headers = ["job id", "slurm id", "host id", "status", "created", "finished"];
    let mut rows: Vec<(String, String, String, String, String, String)> = Vec::new();

    for item in jobs.iter() {
        let job_id = item.job_id.to_string();
        let slurm_id = item
            .slurm_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let completed_str = job_status(item);
        let finished_at = item.finished_at.clone().unwrap_or_else(|| "-".to_string());
        rows.push((
            job_id,
            slurm_id,
            item.hostid.clone(),
            completed_str.to_string(),
            item.created_at.clone(),
            finished_at,
        ));
    }

    let mut widths: [usize; 6] = [
        str_width(headers[0]),
        str_width(headers[1]),
        str_width(headers[2]),
        str_width(headers[3]),
        str_width(headers[4]),
        str_width(headers[5]),
    ];
    for row in rows.iter() {
        widths[0] = widths[0].max(str_width(&row.0));
        widths[1] = widths[1].max(str_width(&row.1));
        widths[2] = widths[2].max(str_width(&row.2));
        widths[3] = widths[3].max(str_width(&row.3));
        widths[4] = widths[4].max(str_width(&row.4));
        widths[5] = widths[5].max(str_width(&row.5));
    }

    println!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5]
    );

    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}",
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            row.5,
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5]
        );
    }
}

fn print_jobs_json(jobs: &[ListJobsUnitResponse]) -> anyhow::Result<()> {
    let data: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();
    emit_json(serde_json::Value::Array(data))
}

fn print_job_details(item: &ListJobsUnitResponse) {
    let slurm_id = item
        .slurm_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let completed_str = job_status(item);
    println!("job_id: {}", item.job_id);
    println!("slurm_id: {}", slurm_id);
    println!("hostid: {}", item.hostid);
    println!("status: {}", completed_str);
    println!(
        "terminal_state: {}",
        item.terminal_state.as_deref().unwrap_or("-")
    );
    println!("created: {}", item.created_at);
    println!(
        "finished: {}",
        item.finished_at.as_deref().unwrap_or("-")
    );
}

fn print_job_details_json(item: &ListJobsUnitResponse) -> anyhow::Result<()> {
    emit_json(job_to_json(item))
}

fn job_status(item: &ListJobsUnitResponse) -> &'static str {
    if !item.is_completed {
        return "running";
    }
    match item.terminal_state.as_deref() {
        Some("COMPLETED") => "completed",
        Some(_) => "failed",
        None => "completed",
    }
}
async fn collect_mfa_answers(mfa: &MfaPrompt) -> anyhow::Result<MfaAnswer> {
    eprintln!();
    if !mfa.name.is_empty() {
        eprintln!("MFA: {}", mfa.name);
    }
    if !mfa.instructions.is_empty() {
        eprintln!("{}", mfa.instructions);
    }

    let mut responses = Vec::with_capacity(mfa.prompts.len());
    for p in &mfa.prompts {
        let ans = prompt_value(&p.text, p.echo).await?;
        responses.push(ans);
    }

    Ok(MfaAnswer { responses })
}

struct Spinner {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

fn braille_spinner_frames() -> [char; 6] {
    let all_mask = braille::DOTS
        .iter()
        .take(3)
        .flatten()
        .fold(0u16, |acc, mask| acc | mask);
    let order = [
        (0usize, 0usize),
        (0, 1),
        (1, 1),
        (2, 1),
        (2, 0),
        (1, 0),
    ];
    let mut frames = [' '; 6];
    for (idx, (row, col)) in order.iter().enumerate() {
        let mask = braille::DOTS[*row][*col];
        let codepoint = u32::from(braille::BLANK | (all_mask & !mask));
        frames[idx] = char::from_u32(codepoint).unwrap_or(' ');
    }
    frames
}

impl Spinner {
    fn start(message: &str) -> Self {
        let message = message.to_string();
        let frames = braille_spinner_frames();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn({
            let message = message.clone();
            let frames = frames;
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

    async fn stop(mut self, done_message: Option<&str>) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
        if let Some(msg) = done_message {
            eprintln!("{msg}");
        }
    }
}

async fn handle_stream_events<S, F, Fut>(
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

enum SubmitStreamOutcome {
    Completed(Option<i32>),
    Canceled,
}

async fn handle_submit_stream_events<S, F, Fut>(
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
    let mut ctrl_c = tokio::signal::ctrl_c();
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
                    let phase = submit_status::Phase::from_i32(status.phase)
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

fn ensure_exit_code(exit_code: Option<i32>, context: &str) -> anyhow::Result<()> {
    if let Some(num) = exit_code {
        if num != 0 {
            bail!("{context} {num}");
        }
    }
    Ok(())
}

async fn send_ls(
    client: &mut AgentClient<Channel>,
    hostid: &str,
    path: &Option<String>,
) -> anyhow::Result<()> {
    let (tx_ans, rx_ans) = mpsc::channel::<LsRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(LsRequest {
            msg: Some(proto::ls_request::Msg::Init(LsRequestInit {
                hostid: hostid.to_owned(),
                path: path.to_owned(),
            })),
        })
        .await?;

    let response = client.ls(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(LsRequest {
                    msg: Some(proto::ls_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    ensure_exit_code(exit_code, "received exit code")
}

async fn send_job_retrieve(
    client: &mut AgentClient<Channel>,
    job_id: i64,
    path: &str,
    dest: &Option<PathBuf>,
    cluster: &Option<String>,
) -> anyhow::Result<()> {
    let mut local_base = match dest {
        Some(v) => v.clone(),
        None => std::env::current_dir()?,
    };
    if !local_base.is_absolute() {
        local_base = std::env::current_dir()?.join(local_base);
    }
    let local_path = local_base.to_string_lossy().into_owned();

    let (tx_ans, rx_ans) = mpsc::channel::<RetrieveJobRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(RetrieveJobRequest {
            msg: Some(proto::retrieve_job_request::Msg::Init(
                RetrieveJobRequestInit {
                    job_id,
                    hostid: cluster.to_owned(),
                    path: path.to_owned(),
                    local_path: Some(local_path),
                },
            )),
        })
        .await?;

    let response = client.retrieve_job(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(RetrieveJobRequest {
                    msg: Some(proto::retrieve_job_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    ensure_exit_code(exit_code, "received exit code")
}

fn submit_filters_from_matches(matches: &clap::ArgMatches) -> Vec<SubmitPathFilterRule> {
    let Some(("submit", sub_matches)) = matches.subcommand() else {
        return Vec::new();
    };

    let mut ordered: Vec<(usize, SubmitPathFilterAction, String)> = Vec::new();
    let mut push_rules = |arg: &str, action: SubmitPathFilterAction| {
        let values: Vec<String> = sub_matches
            .get_many::<String>(arg)
            .map(|vals| vals.map(|v| v.to_string()).collect())
            .unwrap_or_default();
        let indices: Vec<usize> = sub_matches
            .indices_of(arg)
            .map(|vals| vals.collect())
            .unwrap_or_default();
        for (idx, pattern) in indices.into_iter().zip(values.into_iter()) {
            ordered.push((idx, action, pattern));
        }
    };

    push_rules("include", SubmitPathFilterAction::Include);
    push_rules("exclude", SubmitPathFilterAction::Exclude);

    ordered.sort_by_key(|(idx, _, _)| *idx);
    ordered
        .into_iter()
        .map(|(_, action, pattern)| SubmitPathFilterRule {
            action: action as i32,
            pattern,
        })
        .collect()
}

async fn send_submit(
    client: &mut AgentClient<Channel>,
    hostid: &str,
    local_path: &str,
    remote_path: &Option<String>,
    sbatchscript: &str,
    filters: &[SubmitPathFilterRule],
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<SubmitRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(SubmitRequest {
            msg: Some(proto::submit_request::Msg::Init(proto::SubmitRequestInit {
                local_path: local_path.to_owned(),
                remote_path: remote_path.to_owned(),
                hostid: hostid.to_owned(),
                sbatchscript: sbatchscript.to_owned(),
                filters: filters.to_vec(),
            })),
        })
        .await?;
    // Start Submit RPC
    let response = client.submit(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let outcome = handle_submit_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(SubmitRequest {
                    msg: Some(proto::submit_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    match outcome {
        SubmitStreamOutcome::Completed(exit_code) => {
            ensure_exit_code(exit_code, "on client side: received exit code")?;
            println!("Submission complete.");
            Ok(())
        }
        SubmitStreamOutcome::Canceled => bail!("submission canceled"),
    }
}

async fn send_add_cluster(
    client: &mut AgentClient<Channel>,
    host_id: &str,
    username: &str,
    hostname: &Option<String>,
    ip: &Option<String>,
    identity_path: &str,
    port: u32,
    default_base_path: &Option<String>,
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<AddClusterRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    let host: add_cluster_init::Host = match hostname {
        Some(v) => add_cluster_init::Host::Hostname(v.into()),
        None => match ip {
            Some(v) => add_cluster_init::Host::Ipaddr(v.into()),
            None => anyhow::bail!("both hostname and ip address can't be none"),
        },
    };
    let identity_path_expanded = shellexpand::full(identity_path)?;
    let init = AddClusterInit {
        hostid: host_id.to_owned(),
        username: username.to_owned(),
        host: Some(host),
        identity_path: Some(identity_path_expanded.to_string()),
        port: port,
        default_base_path: default_base_path.to_owned(),
    };
    let acr = AddClusterRequest {
        msg: Some(add_cluster_request::Msg::Init(init)),
    };
    tx_ans.send(acr).await?;
    // Start AddCluster RPC
    let response = client.add_cluster(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(AddClusterRequest {
                    msg: Some(proto::add_cluster_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    ensure_exit_code(exit_code, "on client side: received exit code")
}

async fn run_init_project(args: &InitProjectArgs) -> anyhow::Result<()> {
    let project_path = &args.projectpath;
    let force = args.force;
    if project_path.exists() {
        if !project_path.is_dir() {
            bail!("{} is not a directory", project_path.to_string_lossy());
        }
        if !force {
            bail!(
                "{} exists - use --force to overwrite that instead",
                project_path.to_string_lossy()
            )
        }
    }

    if !project_path.exists() {
        if force {
            tokio::fs::create_dir_all(&project_path).await?;
        } else {
            tokio::fs::create_dir(&project_path).await?;
        }
    }

    let hpcfile_path = project_path.join("Hpcfile");
    if hpcfile_path.exists() & !force {
        bail!(
            "{} exists, can't init it - use --force if you want overwrite it",
            hpcfile_path.to_string_lossy()
        );
    }
    // TODO: add more customization options for this; templates also live at this level.
    let mut hpcfile_config = project::hpcfile::Hpcfile::default();

    let slurm_config = project::hpcfile::SlurmConfig::new(
        args.time.clone(),
        args.account.clone(),
        args.cpus.clone(),
        args.mem.clone(),
        "python3 main.py".into(),
    );

    let batch_script_path = project_path.join("batch_script.sh");
    tokio::fs::write(&batch_script_path, &slurm_config.to_sbatch_script()).await?;

    hpcfile_config.batch_script = Some(batch_script_path);
    let hpcfile_content = toml::to_string(&hpcfile_config)?;

    tokio::fs::write(&hpcfile_path, &hpcfile_content.into_bytes()).await?;

    Ok(())
}

fn write_all<W: Write>(w: &mut W, buf: &[u8]) -> anyhow::Result<()> {
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}

async fn prompt_value(prompt: &str, echo: bool) -> anyhow::Result<String> {
    let prompt = prompt.to_string();
    if echo {
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            print!("{}", prompt);
            std::io::stdout().flush()?;
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            // Trim common line endings
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            Ok(s)
        })
        .await?
    } else {
        bail!("Method not supported")
        /*
        tokio::task::spawn_blocking(move || -> Result<String> {
            let s = rpassword::prompt_password(prompt)?;
            Ok(s)
        })
        .await?
        */
    }
}

fn collect_sbatch_scripts(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut matches = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", dir.display(), e))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() && path.extension() == Some(OsStr::new("sbatch")) {
                matches.push(path);
            }
        }
    }

    matches.sort();
    Ok(matches)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("interactive picker requires a TTY; pass --headless and specify --sbatchscript");
        }
        terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(err) = execute!(stdout, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

type RatatuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SbatchPickerPhase {
    Selecting,
    Selected(usize),
    Canceled,
}

struct SbatchPicker {
    scripts: Vec<String>,
    list_state: ListState,
    phase: SbatchPickerPhase,
    viewport_height: usize,
}

impl SbatchPicker {
    fn new(scripts: Vec<String>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let viewport_height = scripts.len().min(8).max(1);
        Self {
            scripts,
            list_state,
            phase: SbatchPickerPhase::Selecting,
            viewport_height,
        }
    }

    fn handle_event(&mut self, event: Event) {
        if self.phase != SbatchPickerPhase::Selecting {
            return;
        }
        let len = self.scripts.len();
        if len == 0 {
            self.phase = SbatchPickerPhase::Canceled;
            return;
        }
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Down => self.move_selection(1),
                KeyCode::PageUp => self.move_selection(-(self.viewport_height as isize)),
                KeyCode::PageDown => self.move_selection(self.viewport_height as isize),
                KeyCode::Home => self.select_index(0),
                KeyCode::End => self.select_index(len.saturating_sub(1)),
                KeyCode::Enter => {
                    let selected = self.list_state.selected().unwrap_or(0);
                    self.phase = SbatchPickerPhase::Selected(selected);
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.phase = SbatchPickerPhase::Canceled;
                }
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    self.phase = SbatchPickerPhase::Canceled;
                }
                _ => {}
            }
        }
    }

    fn select_index(&mut self, index: usize) {
        let index = index.min(self.scripts.len().saturating_sub(1));
        self.list_state.select(Some(index));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.scripts.len();
        if len == 0 {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, (len - 1) as isize) as usize;
        self.list_state.select(Some(next));
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.size();
        frame.render_widget(Clear, area);

        let items: Vec<ListItem> = self
            .scripts
            .iter()
            .map(|script| ListItem::new(script.as_str()))
            .collect();
        let list = List::new(items)
            .highlight_style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");

        let list_area = if area.height >= 3 {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

            let header = Paragraph::new("Select sbatch script");
            frame.render_widget(header, layout[0]);

            let footer = Paragraph::new("Up/Down to move, Enter to select, Esc to cancel")
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM));
            frame.render_widget(footer, layout[2]);
            layout[1]
        } else if area.height == 2 {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area);
            let header = Paragraph::new("Select sbatch script");
            frame.render_widget(header, layout[0]);
            layout[1]
        } else {
            area
        };

        frame.render_stateful_widget(list, list_area, &mut self.list_state);
        self.viewport_height = list_area.height as usize;
    }

    fn inline_viewport_height(&self) -> u16 {
        let header = 1u16;
        let footer = 1u16;
        let max_list = 8u16;
        let list_height = (self.scripts.len().max(1) as u16).min(max_list);
        let desired = header + footer + list_height;
        desired.max(1)
    }

    fn clear(&mut self, terminal: &mut RatatuiTerminal, start: Position) -> anyhow::Result<()> {
        terminal.backend_mut().set_cursor_position(start)?;
        terminal.backend_mut().clear_region(ClearType::AfterCursor)?;
        Ok(())
    }

}

fn pick_sbatch_script(scripts: Vec<String>) -> anyhow::Result<String> {
    let _guard = TerminalGuard::enter()?;
    let (cursor_x, cursor_y) = cursor::position()?;
    let (_, term_height) = terminal::size()?;
    let mut picker = SbatchPicker::new(scripts);
    let desired_height = picker.inline_viewport_height();
    let viewport_height = desired_height.min(term_height.max(1));
    let lines_after_cursor = viewport_height.saturating_sub(1);
    let available_lines = term_height.saturating_sub(cursor_y).saturating_sub(1);
    let scroll_lines = lines_after_cursor.saturating_sub(available_lines);
    let start_pos = Position::new(cursor_x, cursor_y.saturating_sub(scroll_lines));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;

    loop {
        terminal.draw(|frame| picker.render(frame))?;
        picker.handle_event(event::read()?);
        match picker.phase {
            SbatchPickerPhase::Selecting => {}
            SbatchPickerPhase::Selected(index) => {
                picker.clear(&mut terminal, start_pos)?;
                return Ok(picker.scripts[index].clone());
            }
            SbatchPickerPhase::Canceled => {
                picker.clear(&mut terminal, start_pos)?;
                bail!("sbatch selection canceled")
            }
        }
    }
}

fn resolve_sbatch_script(
    local_path: &Path,
    explicit: Option<&str>,
    headless: bool,
) -> anyhow::Result<String> {
    if let Some(sbatchscript) = explicit {
        return Ok(sbatchscript.to_string());
    }

    if !local_path.exists() {
        bail!("local path '{}' does not exist", local_path.display());
    }
    if !local_path.is_dir() {
        bail!(
            "local path '{}' must be a directory to auto-detect .sbatch scripts",
            local_path.display()
        );
    }

    let scripts = collect_sbatch_scripts(local_path)?;
    let relative_scripts: Vec<String> = scripts
        .iter()
        .map(|path| {
            let rel = path.strip_prefix(local_path).unwrap_or(path);
            rel.to_string_lossy().into_owned()
        })
        .collect();

    match relative_scripts.len() {
        0 => bail!(
            "no .sbatch files found under '{}'; provide the script path explicitly",
            local_path.display()
        ),
        1 => {
            Ok(relative_scripts[0].clone())
        }
        _ => {
            if headless {
                let mut msg = format!(
                    "multiple .sbatch files found under '{}' while running in headless mode; specify which one to use with --sbatchscript:\n",
                    local_path.display()
                );
                for script in &relative_scripts {
                    msg.push_str(&format!("  - {}\n", script));
                }
                bail!("{}", msg.trim_end())
            }
            pick_sbatch_script(relative_scripts)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    let submit_filters = submit_filters_from_matches(&matches);
    match cli.cmd {
        Cmd::Ping => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match send_ping(&mut client).await {
                Ok(()) => println!("pong"),
                Err(e) => bail!(e),
            }
        }
        Cmd::Ls(args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            send_ls(&mut client, &args.hostid, &args.path).await?
        }
        Cmd::Submit(SubmitArgs {
            hostid,
            local_path,
            remote_path,
            sbatchscript,
            headless,
            ..
        }) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            let local_path_buf = PathBuf::from(&local_path);
            println!("Submitting job...");
            println!("hostid: {hostid}");
            let _ = std::io::stdout().flush();
            let sbatchscript =
                resolve_sbatch_script(&local_path_buf, sbatchscript.as_deref(), headless)?;
            println!("selected sbatch script: {sbatchscript}");
            send_submit(
                &mut client,
                &hostid,
                &local_path,
                &remote_path,
                &sbatchscript,
                &submit_filters,
            )
            .await?
        }
        Cmd::Init(ref args) => run_init_project(args).await?,
        Cmd::Jobs(jobs_args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match jobs_args.cmd {
                JobsCmd::List(args) => {
                    let response = fetch_list_jobs(&mut client, args.cluster).await?;
                    if args.json {
                        print_jobs_json(&response.jobs)?;
                    } else {
                        print_jobs_table(&response.jobs);
                    }
                }
                JobsCmd::Get(args) => {
                    let response = fetch_list_jobs(&mut client, args.cluster.clone()).await?;
                    let matches: Vec<&ListJobsUnitResponse> = response
                        .jobs
                        .iter()
                        .filter(|job| job.job_id == args.job_id)
                        .collect();
                    match matches.as_slice() {
                        [] => {
                            if let Some(cluster) = args.cluster.as_deref() {
                                bail!(
                                    "job id {} not found in cluster '{}'",
                                    args.job_id,
                                    cluster
                                );
                            }
                            bail!("job id {} not found", args.job_id);
                        }
                        [job] => {
                            if args.json {
                                print_job_details_json(job)?;
                            } else {
                                print_job_details(job);
                            }
                        }
                        _ => {
                            if args.cluster.is_some() {
                                bail!(
                                    "multiple jobs matched job id {}",
                                    args.job_id
                                );
                            }
                            bail!(
                                "job id {} matched multiple clusters; use --cluster",
                                args.job_id
                            );
                        }
                    }
                }
                JobsCmd::Retrieve(args) => {
                    send_job_retrieve(
                        &mut client,
                        args.job_id,
                        &args.path,
                        &args.dest,
                        &args.cluster,
                    )
                    .await?
                }
            }
        }
        Cmd::Clusters(clusters_args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match clusters_args.cmd {
                ClustersCmd::List(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if args.json {
                        print_clusters_json(&response.clusters)?;
                    } else {
                        print_clusters_table(&response.clusters);
                    }
                }
                ClustersCmd::Get(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    let Some(cluster) = response
                        .clusters
                        .iter()
                        .find(|cluster| cluster.hostid == args.hostid)
                    else {
                        bail!("cluster '{}' not found", args.hostid);
                    };
                    if args.json {
                        print_cluster_details_json(cluster)?;
                    } else {
                        print_cluster_details(cluster);
                    }
                }
                ClustersCmd::Add(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if response
                        .clusters
                        .iter()
                        .any(|cluster| cluster.hostid == args.hostid)
                    {
                        bail!(
                            "cluster '{}' already exists; use 'clusters set' to update it",
                            args.hostid
                        );
                    }
                    if let Some(host) = args.hostname.as_deref().or(args.ip.as_deref()) {
                        if response.clusters.iter().any(|cluster| {
                            cluster.username == args.username
                                && cluster_host_string(cluster) == host
                        }) {
                            bail!(
                                "cluster '{}' with address '{}' already exists; use 'clusters set' to update it",
                                args.username,
                                host
                            );
                        }
                    }
                    send_add_cluster(
                        &mut client,
                        &args.hostid,
                        &args.username,
                        &args.hostname,
                        &args.ip,
                        &args.identity_path,
                        args.port,
                        &args.default_base_path,
                    )
                    .await?
                }
                ClustersCmd::Set(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if !response
                        .clusters
                        .iter()
                        .any(|cluster| cluster.hostid == args.hostid)
                    {
                        bail!(
                            "cluster '{}' not found; use 'clusters add' to create it",
                            args.hostid
                        );
                    }
                    send_add_cluster(
                        &mut client,
                        &args.hostid,
                        &args.username,
                        &args.hostname,
                        &args.ip,
                        &args.identity_path,
                        args.port,
                        &args.default_base_path,
                    )
                    .await?
                }
            }
        }
    }
    Ok(())
}

fn num_digits(mut n: i64) -> u32 {
    if n == 0 {
        return 1;
    }

    n = n.abs(); // handle negatives
    n.ilog10() + 1
}
