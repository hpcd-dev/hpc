// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

mod error;
mod session;
mod sync;
mod sync_plan;
mod utils;

pub use error::AuthenticationFailure;
pub use session::{SessionManager, SshParams};
pub use sync::{SyncFilterAction, SyncFilterRule, SyncOptions};
pub use utils::receiver_to_stream;
pub(crate) use utils::sh_escape;
