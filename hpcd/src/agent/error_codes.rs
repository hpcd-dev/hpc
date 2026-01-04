// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::Error;

use crate::ssh::AuthenticationFailure;

pub const AUTHENTICATION_FAILURE: &str = "authentication_failure";
pub const CONNECTION_FAILURE: &str = "connection_failure";
pub const NETWORK_ERROR: &str = "network_error";
pub const INVALID_ARGUMENT: &str = "invalid_argument";
pub const NOT_FOUND: &str = "not_found";
pub const CONFLICT: &str = "conflict";
pub const INTERNAL_ERROR: &str = "internal_error";
pub const CANCELED: &str = "canceled";
pub const REMOTE_ERROR: &str = "remote_error";
pub const LOCAL_ERROR: &str = "local_error";

pub fn code_for_ssh_error(err: &Error) -> &'static str {
    if is_auth_failure(err) {
        AUTHENTICATION_FAILURE
    } else {
        CONNECTION_FAILURE
    }
}

fn is_auth_failure(err: &Error) -> bool {
    err.chain().any(|cause| cause.is::<AuthenticationFailure>())
}
