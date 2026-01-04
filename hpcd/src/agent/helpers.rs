// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::ssh::{SyncFilterAction, SyncFilterRule};
use crate::state::db::{HostRecord, HostStore, JobRecord};
use crate::agent::error_codes;
use proto::{ListClustersUnitResponse, ListJobsUnitResponse};
use proto::{SubmitPathFilterAction, SubmitPathFilterRule, list_clusters_unit_response};
use tonic::Status;

pub fn build_sync_filters(
    filters: Vec<SubmitPathFilterRule>,
) -> Result<Vec<SyncFilterRule>, Status> {
    let mut out = Vec::with_capacity(filters.len());
    for rule in filters {
        let action = match SubmitPathFilterAction::try_from(rule.action) {
            Ok(SubmitPathFilterAction::Include) => SyncFilterAction::Include,
            Ok(SubmitPathFilterAction::Exclude) => SyncFilterAction::Exclude,
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };
        if rule.pattern.trim().is_empty() {
            return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
        }
        out.push(SyncFilterRule {
            action,
            pattern: rule.pattern,
        });
    }
    Ok(out)
}

pub async fn get_default_base_path(hs: &HostStore, name: &str) -> Result<String, Status> {
    let host_data = match hs.get_by_name(name).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err(Status::invalid_argument(error_codes::NOT_FOUND));
        }
        Err(e) => {
            log::debug!("could not retrieve default_base_path for {name}: {e}");
            return Err(Status::internal(error_codes::INTERNAL_ERROR));
        }
    };
    match host_data.default_base_path {
        Some(v) => Ok(v),
        None => Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT)),
    }
}

pub fn db_host_record_to_api_unit_response(hs: &HostRecord) -> ListClustersUnitResponse {
    ListClustersUnitResponse {
        username: hs.username.clone(),
        identity_path: hs.identity_path.to_owned(),
        host: match hs.address {
            crate::state::db::Address::Ip(ref ip) => {
                Some(list_clusters_unit_response::Host::Ipaddr(ip.to_string()))
            }
            crate::state::db::Address::Hostname(ref hostname) => Some(
                list_clusters_unit_response::Host::Hostname(hostname.to_owned()),
            ),
        },
        port: hs.port as i32,
        connected: false,
        name: hs.name.to_owned(),
        accounting_available: hs.accounting_available,
        default_base_path: hs.default_base_path.to_owned(),
    }
}

pub fn db_job_record_to_api_unit_response(jr: &JobRecord) -> ListJobsUnitResponse {
    ListJobsUnitResponse {
        name: jr.name.clone(),
        job_id: jr.id,
        scheduler_id: jr.scheduler_id,
        created_at: jr.created_at.clone(),
        finished_at: jr.finished_at.clone(),
        is_completed: jr.is_completed,
        terminal_state: jr.terminal_state.clone(),
    }
}
