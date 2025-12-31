use anyhow::bail;
use clap::{CommandFactory, FromArgMatches};
use cli::args::{Cli, ClustersCmd, Cmd, JobsCmd, SubmitArgs};
use cli::client::{
    fetch_list_clusters, fetch_list_jobs, send_add_cluster, send_job_retrieve, send_ls,
    send_ping, send_submit,
};
use cli::filters::submit_filters_from_matches;
use cli::format::{
    cluster_host_string, format_cluster_details, format_cluster_details_json,
    format_clusters_json, format_clusters_table, format_job_details, format_job_details_json,
    format_jobs_json, format_jobs_table,
};
use cli::sbatch::resolve_sbatch_script;
use proto::agent_client::AgentClient;
use proto::ListJobsUnitResponse;
use std::io::Write;
use std::path::PathBuf;

const HELP_TEMPLATE: &str = r#"██╗  ██╗██████╗  ██████╗
██║  ██║██╔══██╗██╔════╝
███████║██████╔╝██║
██╔══██║██╔═══╝ ██║
██║  ██║██║     ╚██████╗
╚═╝  ╚═╝╚═╝      ╚═════╝

{before-help}{about-with-newline}{usage-heading} {usage}

{all-args}{after-help}
"#;

fn apply_help_template_recursively(cmd: &mut clap::Command) {
    let mut owned = std::mem::take(cmd);
    owned = owned.help_template(HELP_TEMPLATE);
    for sub in owned.get_subcommands_mut() {
        apply_help_template_recursively(sub);
    }
    *cmd = owned;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    apply_help_template_recursively(&mut cmd);
    let matches = cmd.get_matches();
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
        Cmd::Jobs(jobs_args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match jobs_args.cmd {
                JobsCmd::List(args) => {
                    let response = fetch_list_jobs(&mut client, args.cluster).await?;
                    if args.json {
                        let output = format_jobs_json(&response.jobs)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_jobs_table(&response.jobs));
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
                                bail!("job id {} not found in cluster '{}'", args.job_id, cluster);
                            }
                            bail!("job id {} not found", args.job_id);
                        }
                        [job] => {
                            if args.json {
                                let output = format_job_details_json(job)?;
                                println!("{output}");
                            } else {
                                print!("{}", format_job_details(job));
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
                        let output = format_clusters_json(&response.clusters)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_clusters_table(&response.clusters));
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
                        let output = format_cluster_details_json(cluster)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_cluster_details(cluster));
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
