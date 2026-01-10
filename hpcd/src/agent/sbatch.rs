// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::util::remote_path::resolve_relative;
use std::path::Path;

pub const DEFAULT_STDOUT_TEMPLATE: &str = "slurm-%j.out";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbatchLogTemplates {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub job_name: Option<String>,
}

pub fn parse_sbatch_log_templates(script: &str) -> SbatchLogTemplates {
    let mut stdout: Option<String> = None;
    let mut stderr: Option<String> = None;
    let mut job_name: Option<String> = None;

    for line in script.lines() {
        let line = line.trim();
        if !line.starts_with("#SBATCH") {
            continue;
        }
        let rest = line.trim_start_matches("#SBATCH").trim_start();
        if rest.is_empty() {
            continue;
        }
        let tokens = split_sbatch_args(rest);
        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i].as_str();
            if let Some(value) = parse_flag_value(tok, "-o", "--output") {
                stdout = normalize_value(value);
                i += 1;
                continue;
            }
            if let Some(value) = parse_flag_value(tok, "-e", "--error") {
                stderr = normalize_value(value);
                i += 1;
                continue;
            }
            if let Some(value) = parse_flag_value(tok, "-J", "--job-name") {
                job_name = normalize_value(value);
                i += 1;
                continue;
            }
            if tok == "-o" || tok == "--output" {
                if let Some(next) = tokens.get(i + 1) {
                    stdout = normalize_value(next.clone());
                    i += 2;
                    continue;
                }
            }
            if tok == "-e" || tok == "--error" {
                if let Some(next) = tokens.get(i + 1) {
                    stderr = normalize_value(next.clone());
                    i += 2;
                    continue;
                }
            }
            if tok == "-J" || tok == "--job-name" {
                if let Some(next) = tokens.get(i + 1) {
                    job_name = normalize_value(next.clone());
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
    }

    SbatchLogTemplates {
        stdout,
        stderr,
        job_name,
    }
}

pub fn resolve_log_path(
    template: &str,
    remote_root: &str,
    scheduler_id: i64,
    job_name: Option<&str>,
    user_name: Option<&str>,
) -> String {
    let expanded = expand_log_tokens(template, scheduler_id, job_name, user_name);
    if Path::new(&expanded).is_absolute() {
        expanded
    } else {
        resolve_relative(remote_root, expanded)
            .to_string_lossy()
            .into_owned()
    }
}

fn parse_flag_value(token: &str, short: &str, long: &str) -> Option<String> {
    if let Some(value) = token.strip_prefix(short).and_then(|rest| rest.strip_prefix('=')) {
        return Some(value.to_string());
    }
    if let Some(value) = token.strip_prefix(long).and_then(|rest| rest.strip_prefix('=')) {
        return Some(value.to_string());
    }
    None
}

fn normalize_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn expand_log_tokens(
    value: &str,
    scheduler_id: i64,
    job_name: Option<&str>,
    user_name: Option<&str>,
) -> String {
    let id = scheduler_id.to_string();
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('j') | Some('J') | Some('A') => out.push_str(&id),
            Some('x') => match job_name {
                Some(name) => out.push_str(name),
                None => out.push_str("%x"),
            },
            Some('u') => match user_name {
                Some(name) => out.push_str(name),
                None => out.push_str("%u"),
            },
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn split_sbatch_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for ch in input.chars() {
        if escape {
            buf.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                buf.push(ch);
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            } else {
                buf.push(ch);
            }
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            ch if ch.is_whitespace() => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            _ => buf.push(ch),
        }
    }

    if escape {
        buf.push('\\');
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sbatch_log_templates_picks_last_values() {
        let script = r#"
#!/usr/bin/env bash
#SBATCH -o first.out
#SBATCH --output=final.out
#SBATCH -e first.err
#SBATCH --error final.err
#SBATCH -J first-name
#SBATCH --job-name=final-name
echo "hi"
"#;
        let parsed = parse_sbatch_log_templates(script);
        assert_eq!(parsed.stdout.as_deref(), Some("final.out"));
        assert_eq!(parsed.stderr.as_deref(), Some("final.err"));
        assert_eq!(parsed.job_name.as_deref(), Some("final-name"));
    }

    #[test]
    fn parse_sbatch_log_templates_handles_quotes() {
        let script = r#"#SBATCH --output="logs/run %j.out"
#SBATCH --job-name="job name"
"#;
        let parsed = parse_sbatch_log_templates(script);
        assert_eq!(parsed.stdout.as_deref(), Some("logs/run %j.out"));
        assert_eq!(parsed.job_name.as_deref(), Some("job name"));
    }

    #[test]
    fn resolve_log_path_replaces_job_id_and_joins_relative() {
        let path = resolve_log_path("logs/%j.out", "/remote/run", 42, None, None);
        assert_eq!(path, "/remote/run/logs/42.out");
    }

    #[test]
    fn resolve_log_path_keeps_absolute() {
        let path = resolve_log_path("/var/log/slurm-%j.out", "/remote/run", 7, None, None);
        assert_eq!(path, "/var/log/slurm-7.out");
    }

    #[test]
    fn resolve_log_path_expands_job_name_and_user() {
        let path = resolve_log_path(
            "logs/%x-%u-%j.out",
            "/remote/run",
            26,
            Some("run-name"),
            Some("ubuntu"),
        );
        assert_eq!(path, "/remote/run/logs/run-name-ubuntu-26.out");
    }

    #[test]
    fn resolve_log_path_keeps_escaped_percent() {
        let path = resolve_log_path("logs/%%-%j.out", "/remote/run", 5, None, None);
        assert_eq!(path, "/remote/run/logs/%-5.out");
    }
}
