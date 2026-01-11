// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use proto::{ListClustersUnitResponse, ListJobsUnitResponse};
use serde_json::json;

pub fn cluster_host_string(item: &ListClustersUnitResponse) -> String {
    match item.host {
        Some(ref v) => match v {
            proto::list_clusters_unit_response::Host::Hostname(s) => s.to_string(),
            proto::list_clusters_unit_response::Host::Ipaddr(s) => s.to_string(),
        },
        None => "<unknown>".to_string(),
    }
}

pub fn cluster_ssh_string(item: &ListClustersUnitResponse) -> String {
    let host = cluster_host_string(item);
    let formatted_host = if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]")
    } else {
        host
    };
    format!("{}@{}:{}", item.username, formatted_host, item.port)
}

pub fn cluster_to_json(item: &ListClustersUnitResponse) -> serde_json::Value {
    let status = match item.connected {
        true => "connected",
        false => "disconnected",
    };
    json!({
        "name": item.name.as_str(),
        "username": item.username.as_str(),
        "address": cluster_host_string(item),
        "port": item.port,
        "connected": item.connected,
        "reachable": item.reachable,
        "status": status,
        "identity_path": item.identity_path.as_deref(),
        "accounting_available": item.accounting_available,
        "default_base_path": item.default_base_path.as_deref(),
    })
}

pub fn job_to_json(item: &ListJobsUnitResponse) -> serde_json::Value {
    let status = job_status(item);
    json!({
        "job_id": item.job_id,
        "local_path": item.local_path.as_str(),
        "remote_path": item.remote_path.as_str(),
        "name": item.name.as_str(),
        "status": status,
        "is_completed": item.is_completed,
        "terminal_state": item.terminal_state.as_deref(),
        "created_at": item.created_at.as_str(),
        "finished_at": item.finished_at.as_deref(),
        "scheduler_id": item.scheduler_id,
    })
}

pub fn format_json(value: serde_json::Value) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&value)?)
}

fn str_width(value: &str) -> usize {
    value.chars().count()
}

pub fn format_clusters_table(clusters: &[ListClustersUnitResponse]) -> String {
    let headers = ["name", "destination", "status", "reachable", "accounting"];
    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();

    for item in clusters.iter() {
        let ssh_str = cluster_ssh_string(item);
        let connected_str = match item.connected {
            true => "connected",
            false => "disconnected",
        };
        let reachable_str = match item.reachable {
            true => "yes",
            false => "no",
        };
        let accounting_str = match item.accounting_available {
            true => "enabled",
            false => "disabled",
        };
        rows.push((
            item.name.clone(),
            ssh_str,
            connected_str.to_string(),
            reachable_str.to_string(),
            accounting_str.to_string(),
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

    let mut output = String::new();
    output.push_str(&format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}\n",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
    ));

    for row in rows {
        output.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}\n",
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
        ));
    }

    output
}

pub fn format_clusters_json(clusters: &[ListClustersUnitResponse]) -> anyhow::Result<String> {
    let data: Vec<serde_json::Value> = clusters.iter().map(cluster_to_json).collect();
    format_json(serde_json::Value::Array(data))
}

pub fn format_cluster_details(item: &ListClustersUnitResponse) -> String {
    let host_str = cluster_host_string(item);
    let connected_str = match item.connected {
        true => "connected",
        false => "disconnected",
    };
    let accounting_str = match item.accounting_available {
        true => "enabled",
        false => "disabled",
    };
    format!(
        "name: {}\nusername: {}\naddress: {}\nport: {}\nstatus: {}\naccounting: {}\nidentity_path: {}\ndefault_base_path: {}\n",
        item.name,
        item.username,
        host_str,
        item.port,
        connected_str,
        accounting_str,
        item.identity_path.as_deref().unwrap_or("-"),
        item.default_base_path.as_deref().unwrap_or("-")
    )
}

pub fn format_cluster_details_json(item: &ListClustersUnitResponse) -> anyhow::Result<String> {
    format_json(cluster_to_json(item))
}

pub fn format_jobs_table(jobs: &[ListJobsUnitResponse]) -> String {
    let headers = [
        "job id",
        "cluster id",
        "status",
        "created",
        "finished",
        "scheduler id",
    ];
    let mut rows: Vec<(String, String, String, String, String, String)> = Vec::new();

    for item in jobs.iter() {
        let job_id = item.job_id.to_string();
        let scheduler_id = item
            .scheduler_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let completed_str = job_status(item);
        let finished_at = item.finished_at.clone().unwrap_or_else(|| "-".to_string());
        rows.push((
            job_id,
            item.name.clone(),
            completed_str.to_string(),
            item.created_at.clone(),
            finished_at,
            scheduler_id,
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

    let mut output = String::new();
    output.push_str(&format!(
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}\n",
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
        w5 = widths[5],
    ));

    for row in rows {
        output.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}\n",
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
            w5 = widths[5],
        ));
    }

    output
}

pub fn format_jobs_json(jobs: &[ListJobsUnitResponse]) -> anyhow::Result<String> {
    let data: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();
    format_json(serde_json::Value::Array(data))
}

pub fn format_job_details(item: &ListJobsUnitResponse) -> String {
    let scheduler_id = item
        .scheduler_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let completed_str = job_status(item);
    format!(
        "job_id: {}\nlocal_path: {}\nremote_path: {}\nname: {}\nstatus: {}\nterminal_state: {}\ncreated: {}\nfinished: {}\nscheduler_id: {}\n",
        item.job_id,
        item.local_path.as_str(),
        item.remote_path.as_str(),
        item.name,
        completed_str,
        item.terminal_state.as_deref().unwrap_or("-"),
        item.created_at,
        item.finished_at.as_deref().unwrap_or("-"),
        scheduler_id
    )
}

pub fn format_job_details_json(item: &ListJobsUnitResponse) -> anyhow::Result<String> {
    format_json(job_to_json(item))
}

pub fn job_status(item: &ListJobsUnitResponse) -> &'static str {
    if !item.is_completed {
        return "running";
    }
    match item.terminal_state.as_deref() {
        Some("COMPLETED") => "completed",
        Some(_) => "failed",
        None => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cluster(
        host: Option<proto::list_clusters_unit_response::Host>,
    ) -> ListClustersUnitResponse {
        ListClustersUnitResponse {
            username: "alice".to_string(),
            identity_path: Some("~/.ssh/id_ed25519".to_string()),
            port: 22,
            host,
            connected: true,
            reachable: true,
            name: "cluster-a".to_string(),
            accounting_available: false,
            default_base_path: None,
        }
    }

    fn sample_job(completed: bool, terminal_state: Option<&str>) -> ListJobsUnitResponse {
        ListJobsUnitResponse {
            name: "cluster-a".to_string(),
            job_id: 42,
            scheduler_id: Some(99),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: Some("2024-01-01T01:00:00Z".to_string()),
            is_completed: completed,
            terminal_state: terminal_state.map(|s| s.to_string()),
            local_path: "/tmp/project".to_string(),
            remote_path: "/remote/project".to_string(),
        }
    }

    #[test]
    fn cluster_host_string_handles_hostname_ip_none() {
        let hostname = sample_cluster(Some(proto::list_clusters_unit_response::Host::Hostname(
            "node".to_string(),
        )));
        let ip = sample_cluster(Some(proto::list_clusters_unit_response::Host::Ipaddr(
            "10.0.0.1".to_string(),
        )));
        let none = sample_cluster(None);

        assert_eq!(cluster_host_string(&hostname), "node");
        assert_eq!(cluster_host_string(&ip), "10.0.0.1");
        assert_eq!(cluster_host_string(&none), "<unknown>");
    }

    #[test]
    fn cluster_ssh_string_includes_username_host_port() {
        let cluster = sample_cluster(Some(proto::list_clusters_unit_response::Host::Hostname(
            "node".to_string(),
        )));
        assert_eq!(cluster_ssh_string(&cluster), "alice@node:22");
    }

    #[test]
    fn job_status_reports_running_and_terminal_states() {
        let running = sample_job(false, None);
        let completed = sample_job(true, Some("COMPLETED"));
        let failed = sample_job(true, Some("FAILED"));
        let completed_unknown = sample_job(true, None);

        assert_eq!(job_status(&running), "running");
        assert_eq!(job_status(&completed), "completed");
        assert_eq!(job_status(&failed), "failed");
        assert_eq!(job_status(&completed_unknown), "completed");
    }

    #[test]
    fn format_clusters_table_includes_headers_and_rows() {
        let cluster = sample_cluster(Some(proto::list_clusters_unit_response::Host::Hostname(
            "node".to_string(),
        )));
        let output = format_clusters_table(&[cluster]);
        assert!(output.contains("name"));
        assert!(output.contains("destination"));
        assert!(output.contains("reachable"));
        assert!(output.contains("cluster-a"));
        assert!(output.contains("alice@node:22"));
        assert!(output.contains("yes"));
    }

    #[test]
    fn format_jobs_table_includes_headers_and_rows() {
        let job = sample_job(true, Some("COMPLETED"));
        let output = format_jobs_table(&[job]);
        assert!(output.contains("job id"));
        assert!(output.contains("cluster-a"));
        assert!(output.contains("completed"));
    }

    #[test]
    fn cluster_to_json_includes_status_fields() {
        let cluster = sample_cluster(Some(proto::list_clusters_unit_response::Host::Hostname(
            "node".to_string(),
        )));
        let json = cluster_to_json(&cluster);
        assert_eq!(json["status"], "connected");
        assert_eq!(json["name"], "cluster-a");
        assert_eq!(json["address"], "node");
    }
}
