use anyhow::bail;
use clap::{ArgGroup, Args, Parser, Subcommand};
use proto::agent_client::AgentClient;
use proto::{
    AddClusterInit, AddClusterRequest, ListClustersRequest, ListClustersResponse,
    ListClustersUnitResponse, ListJobsRequest, ListJobsResponse, ListJobsUnitResponse, LsRequest,
    LsRequestInit, MfaAnswer, MfaPrompt, PingRequest, StreamEvent, SubmitRequest, SubmitRequestInit,
    add_cluster_init, add_cluster_request, stream_event,
};
use serde::Deserialize;
use serde_json::json;
use std::future::Future;
use std::io::Write;
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
}

#[derive(Args, Debug)]
struct JobGetArgs {
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
    /// Add or update a cluster.
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
    remote_path: Option<String>,
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
    let status = match item.is_completed {
        true => "completed",
        false => "running",
    };
    json!({
        "internal_job_id": item.internal_job_id,
        "job_id": item.job_id,
        "hostid": item.hostid.as_str(),
        "status": status,
        "is_completed": item.is_completed,
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
    let headers = ["job id", "host id", "status", "created", "finished"];
    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();

    for item in jobs.iter() {
        let job_id = item
            .job_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let completed_str = match item.is_completed {
            true => "completed",
            false => "running",
        };
        let finished_at = item.finished_at.clone().unwrap_or_else(|| "-".to_string());
        rows.push((
            job_id,
            item.hostid.clone(),
            completed_str.to_string(),
            item.created_at.clone(),
            finished_at,
        ));
    }

    let mut widths: [usize; 5] = [
        str_width(headers[0]),
        str_width(headers[1]),
        str_width(headers[2]),
        str_width(headers[3]),
        str_width(headers[4]),
    ];
    for row in rows.iter() {
        widths[0] = widths[0].max(str_width(&row.0));
        widths[1] = widths[1].max(str_width(&row.1));
        widths[2] = widths[2].max(str_width(&row.2));
        widths[3] = widths[3].max(str_width(&row.3));
        widths[4] = widths[4].max(str_width(&row.4));
    }

    println!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4]
    );

    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}",
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4]
        );
    }
}

fn print_jobs_json(jobs: &[ListJobsUnitResponse]) -> anyhow::Result<()> {
    let data: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();
    emit_json(serde_json::Value::Array(data))
}

fn print_job_details(item: &ListJobsUnitResponse) {
    let job_id = item
        .job_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let completed_str = match item.is_completed {
        true => "completed",
        false => "running",
    };
    println!("internal_id: {}", item.internal_job_id);
    println!("job_id: {}", job_id);
    println!("hostid: {}", item.hostid);
    println!("status: {}", completed_str);
    println!("created: {}", item.created_at);
    println!(
        "finished: {}",
        item.finished_at.as_deref().unwrap_or("-")
    );
}

fn print_job_details_json(item: &ListJobsUnitResponse) -> anyhow::Result<()> {
    emit_json(job_to_json(item))
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

async fn send_submit(
    client: &mut AgentClient<Channel>,
    hostid: &str,
    local_path: &str,
    remote_path: &Option<String>,
    sbatchscript: &str,
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
            })),
        })
        .await?;
    // Start Submit RPC
    let response = client.submit(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
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
    ensure_exit_code(exit_code, "on client side: received exit code")
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

fn resolve_sbatch_script(local_path: &Path, explicit: Option<&str>) -> anyhow::Result<String> {
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
    match scripts.len() {
        0 => bail!(
            "no .sbatch files found under '{}'; provide the script path explicitly",
            local_path.display()
        ),
        1 => {
            let rel = scripts[0].strip_prefix(local_path).unwrap_or(&scripts[0]);
            Ok(rel.to_string_lossy().into_owned())
        }
        _ => {
            let mut msg = format!(
                "multiple .sbatch files found under '{}'; specify which one to use:\n",
                local_path.display()
            );
            for script in scripts {
                msg.push_str(&format!("  - {}\n", script.display()));
            }
            bail!("{}", msg.trim_end())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
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
        }) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            let local_path_buf = PathBuf::from(&local_path);
            let sbatchscript = resolve_sbatch_script(&local_path_buf, sbatchscript.as_deref())?;
            send_submit(
                &mut client,
                &hostid,
                &local_path,
                &remote_path,
                &sbatchscript,
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
                        .filter(|job| job.job_id == Some(args.job_id))
                        .collect();
                    match matches.as_slice() {
                        [] => {
                            if let Some(cluster) = args.cluster.as_deref() {
                                bail!("job {} not found in cluster '{}'", args.job_id, cluster);
                            }
                            bail!("job {} not found", args.job_id);
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
                                bail!("multiple jobs matched job id {}", args.job_id);
                            }
                            bail!(
                                "job id {} matched multiple clusters; use --cluster",
                                args.job_id
                            );
                        }
                    }
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
                ClustersCmd::Set(args) => {
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
