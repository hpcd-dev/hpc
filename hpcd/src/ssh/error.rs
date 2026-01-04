// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
#[error("authentication_failure")]
pub struct AuthenticationFailure;
