// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::bail;
use clap::{CommandFactory, FromArgMatches};
use cli::args::{Cli, ClusterCmd, Cmd, JobCmd, SubmitArgs};
use cli::client::{
    fetch_list_clusters, fetch_list_jobs, send_add_cluster, send_delete_cluster, send_job_retrieve,
    send_ls, send_ping, send_submit,
};
use cli::filters::submit_filters_from_matches;
use cli::format::{
    cluster_host_string, format_cluster_details, format_cluster_details_json, format_clusters_json,
    format_clusters_table, format_job_details, format_job_details_json, format_jobs_json,
    format_jobs_table,
};
use cli::interactive::{confirm_action, resolve_add_cluster_args};
use cli::sbatch::resolve_sbatch_script;
use proto::ListJobsUnitResponse;
use proto::agent_client::AgentClient;
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
            send_ls(&mut client, &args.name, &args.path).await?
        }
        Cmd::Submit(SubmitArgs {
            name,
            local_path,
            remote_path,
            sbatchscript,
            headless,
            ..
        }) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            let local_path_buf = PathBuf::from(&local_path);
            println!("Submitting job...");
            println!("name: {name}");
            let _ = std::io::stdout().flush();
            let sbatchscript =
                resolve_sbatch_script(&local_path_buf, sbatchscript.as_deref(), headless)?;
            println!("selected sbatch script: {sbatchscript}");
            send_submit(
                &mut client,
                &name,
                &local_path,
                &remote_path,
                &sbatchscript,
                &submit_filters,
            )
            .await?
        }
        Cmd::Job(job_args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match job_args.cmd {
                JobCmd::List(args) => {
                    let response = fetch_list_jobs(&mut client, args.cluster).await?;
                    if args.json {
                        let output = format_jobs_json(&response.jobs)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_jobs_table(&response.jobs));
                    }
                }
                JobCmd::Get(args) => {
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
                JobCmd::Retrieve(args) => {
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
        Cmd::Cluster(cluster_args) => {
            let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
            match cluster_args.cmd {
                ClusterCmd::List(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if args.json {
                        let output = format_clusters_json(&response.clusters)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_clusters_table(&response.clusters));
                    }
                }
                ClusterCmd::Get(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    let Some(cluster) = response
                        .clusters
                        .iter()
                        .find(|cluster| cluster.name == args.name)
                    else {
                        bail!("cluster '{}' not found", args.name);
                    };
                    if args.json {
                        let output = format_cluster_details_json(cluster)?;
                        println!("{output}");
                    } else {
                        print!("{}", format_cluster_details(cluster));
                    }
                }
                ClusterCmd::Add(args) => {
                    println!("Adding new cluster...");
                    let resolved = resolve_add_cluster_args(args)?;
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if response
                        .clusters
                        .iter()
                        .any(|cluster| cluster.name == resolved.name)
                    {
                        bail!(
                            "cluster '{}' already exists; use 'cluster set' to update it",
                            resolved.name
                        );
                    }
                    if let Some(host) = resolved.hostname.as_deref().or(resolved.ip.as_deref()) {
                        if let Some(existing) = response.clusters.iter().find(|cluster| {
                            cluster.username == resolved.username
                                && cluster_host_string(cluster) == host
                                && cluster.port == resolved.port as i32
                        }) {
                            bail!(
                                "another cluster with name '{}' is already using {}:{}:{}; use 'cluster set' to update it.",
                                existing.name,
                                resolved.username,
                                host,
                                resolved.port
                            );
                        }
                    }
                    send_add_cluster(
                        &mut client,
                        &resolved.name,
                        &resolved.username,
                        &resolved.hostname,
                        &resolved.ip,
                        Some(resolved.identity_path.as_str()),
                        resolved.port,
                        &resolved.default_base_path,
                    )
                    .await?;
                    println!("Cluster {} added successfully!", resolved.name);
                }
                ClusterCmd::Set(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    let Some(cluster) = response
                        .clusters
                        .iter()
                        .find(|cluster| cluster.name == args.name)
                    else {
                        bail!(
                            "cluster '{}' not found; use 'cluster add' to create it",
                            args.name
                        );
                    };
                    let (hostname, ip) = match args.ip.as_ref() {
                        Some(ip) => (None, Some(ip.clone())),
                        None => match &cluster.host {
                            Some(proto::list_clusters_unit_response::Host::Hostname(host)) => {
                                (Some(host.clone()), None)
                            }
                            Some(proto::list_clusters_unit_response::Host::Ipaddr(host)) => {
                                (None, Some(host.clone()))
                            }
                            None => {
                                bail!(
                                    "cluster '{}' has no address; pass --ip to update it",
                                    args.name
                                );
                            }
                        },
                    };
                    let port = match args.port {
                        Some(port) => port,
                        None => match u32::try_from(cluster.port) {
                            Ok(port) => port,
                            Err(_) => bail!(
                                "cluster '{}' has invalid port '{}'",
                                args.name,
                                cluster.port
                            ),
                        },
                    };
                    let identity_path = args
                        .identity_path
                        .clone()
                        .or_else(|| cluster.identity_path.clone());
                    let default_base_path = args
                        .default_base_path
                        .clone()
                        .or_else(|| cluster.default_base_path.clone());
                    send_add_cluster(
                        &mut client,
                        &cluster.name,
                        &cluster.username,
                        &hostname,
                        &ip,
                        identity_path.as_deref(),
                        port,
                        &default_base_path,
                    )
                    .await?
                }
                ClusterCmd::Delete(args) => {
                    let response = fetch_list_clusters(&mut client, "").await?;
                    if !response
                        .clusters
                        .iter()
                        .any(|cluster| cluster.name == args.name)
                    {
                        bail!("cluster name '{}' is not known", args.name);
                    }
                    println!(
                        "WARNING:\nThis will delete cluster '{}' and all its job records from the local database.",
                        args.name
                    );
                    println!("Any active SSH sessions for this cluster will be closed.");
                    println!("This action cannot be undone.");
                    if !args.yes {
                        let confirmed = confirm_action(
                            "Continue with delete? (yes/no): ",
                            "Type yes to confirm, no to cancel.",
                        )?;
                        if !confirmed {
                            println!("Delete canceled.");
                            return Ok(());
                        }
                    }
                    let response = send_delete_cluster(&mut client, &args.name).await?;
                    if !response.deleted {
                        bail!("cluster name '{}' is not known", args.name);
                    }
                    println!("Cluster '{}' deleted.", args.name);
                }
            }
        }
    }
    Ok(())
}
