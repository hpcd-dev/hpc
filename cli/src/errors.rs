// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use tonic::{Code, Status};

const AUTHENTICATION_FAILURE: &str = "authentication_failure";
const CONNECTION_FAILURE: &str = "connection_failure";
const NETWORK_ERROR: &str = "network_error";
const INVALID_ARGUMENT: &str = "invalid_argument";
const NOT_FOUND: &str = "not_found";
const CONFLICT: &str = "conflict";
const INTERNAL_ERROR: &str = "internal_error";
const CANCELED: &str = "canceled";
const REMOTE_ERROR: &str = "remote_error";
const LOCAL_ERROR: &str = "local_error";
const PERMISSION_DENIED: &str = "permission_denied";

pub fn format_server_error(raw: &str) -> String {
    if let Some(message) = describe_error_code(raw) {
        return message.to_string();
    }
    raw.to_string()
}

pub fn format_status_error(status: &Status) -> String {
    let message = status.message();
    if let Some(message) = describe_error_code(message) {
        return message.to_string();
    }
    if !message.is_empty() {
        return message.to_string();
    }
    match status.code() {
        Code::Cancelled => describe_error_code(CANCELED).unwrap_or("Canceled.").to_string(),
        Code::Unauthenticated => {
            describe_error_code(AUTHENTICATION_FAILURE)
                .unwrap_or("Authentication failed.")
                .to_string()
        }
        Code::PermissionDenied => {
            describe_error_code(PERMISSION_DENIED)
                .unwrap_or("Permission denied.")
                .to_string()
        }
        Code::Unavailable => describe_error_code(NETWORK_ERROR)
            .unwrap_or("Network error.")
            .to_string(),
        _ => "Server error.".to_string(),
    }
}

fn describe_error_code(code: &str) -> Option<&'static str> {
    match code {
        AUTHENTICATION_FAILURE => Some(
            "Authentication failed. Check your username, SSH key, and MFA responses.",
        ),
        CONNECTION_FAILURE => Some("Could not establish an SSH connection to the cluster."),
        NETWORK_ERROR => Some("Network error while resolving or reaching the cluster."),
        INVALID_ARGUMENT => Some("Invalid input; check the command arguments."),
        NOT_FOUND => Some("Requested item not found."),
        CONFLICT => Some("The requested resource already exists."),
        INTERNAL_ERROR => Some("Internal server error."),
        CANCELED => Some("Operation canceled."),
        REMOTE_ERROR => Some("Remote operation failed."),
        LOCAL_ERROR => Some("Local operation failed."),
        PERMISSION_DENIED => Some("Permission denied."),
        _ => None,
    }
}
