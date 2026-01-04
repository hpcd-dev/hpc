// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::agent::error_codes;
use crate::util::remote_path::resolve_relative;
use std::path::PathBuf;
use tonic::Status;

pub fn resolve_submit_remote_path(
    remote_path: Option<&str>,
    default_base_path: &str,
    random_suffix: &str,
) -> Result<String, Status> {
    match remote_path {
        Some(v) => {
            if v.is_empty() {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
            if PathBuf::from(v).is_absolute() {
                Ok(v.to_string())
            } else {
                Ok(resolve_relative(default_base_path, v)
                    .to_string_lossy()
                    .into_owned())
            }
        }
        None => Ok(PathBuf::from(default_base_path)
            .join(random_suffix)
            .to_string_lossy()
            .into_owned()),
    }
}

pub fn resolve_remote_sbatch_path(remote_root: &str, sbatchscript: &str) -> String {
    resolve_relative(remote_root, sbatchscript)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_submit_remote_path_handles_variants() {
        let err = resolve_submit_remote_path(Some(""), "/base", "run").unwrap_err();
        assert_eq!(err.message(), error_codes::INVALID_ARGUMENT);

        let absolute = resolve_submit_remote_path(Some("/abs/path"), "/base", "run").unwrap();
        assert_eq!(absolute, "/abs/path");

        let relative = resolve_submit_remote_path(Some("jobs/run"), "/base", "run").unwrap();
        assert_eq!(relative, "/base/jobs/run");

        let generated = resolve_submit_remote_path(None, "/base", "run").unwrap();
        assert_eq!(generated, "/base/run");
    }

    #[test]
    fn resolve_remote_sbatch_path_resolves_relative() {
        let path = resolve_remote_sbatch_path("/base/run", "scripts/job.sbatch");
        assert_eq!(path, "/base/run/scripts/job.sbatch");
    }

    // No submit success formatter tests; the final submit result is encoded in RPC events.
}
