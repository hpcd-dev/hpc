// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::bail;
use proto::{MfaAnswer, MfaPrompt};
use std::io::Write;

pub async fn collect_mfa_answers(mfa: &MfaPrompt) -> anyhow::Result<MfaAnswer> {
    eprintln!();
    if !mfa.name.is_empty() {
        eprintln!("MFA: {}", mfa.name);
    }
    if !mfa.instructions.is_empty() {
        eprintln!("{}", mfa.instructions);
    }

    let mut responses = Vec::with_capacity(mfa.prompts.len());
    for p in &mfa.prompts {
        let ans = prompt_value(&p.text, p.echo).await?;
        responses.push(ans);
    }

    Ok(MfaAnswer { responses })
}

pub async fn collect_mfa_answers_transient(
    mfa: &MfaPrompt,
) -> anyhow::Result<(MfaAnswer, usize)> {
    let mut lines = 0usize;
    eprintln!();
    lines += 1;

    if !mfa.name.is_empty() {
        let title = format!("MFA: {}", mfa.name);
        eprintln!("{title}");
        lines += count_lines(&title);
    }
    if !mfa.instructions.is_empty() {
        eprintln!("{}", mfa.instructions);
        lines += count_lines(&mfa.instructions);
    }

    let mut responses = Vec::with_capacity(mfa.prompts.len());
    for p in &mfa.prompts {
        let prompt_lines = count_lines(&p.text).max(1);
        let ans = prompt_value(&p.text, p.echo).await?;
        responses.push(ans);
        lines += prompt_lines;
    }

    Ok((MfaAnswer { responses }, lines))
}

async fn prompt_value(prompt: &str, echo: bool) -> anyhow::Result<String> {
    let prompt = prompt.to_string();
    if echo {
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            print!("{}", prompt);
            std::io::stdout().flush()?;
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            // Trim common line endings
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            Ok(s)
        })
        .await?
    } else {
        bail!("Method not supported")
        /*
        tokio::task::spawn_blocking(move || -> Result<String> {
            let s = rpassword::prompt_password(prompt)?;
            Ok(s)
        })
        .await?
        */
    }
}

fn count_lines(value: &str) -> usize {
    if value.is_empty() {
        0
    } else {
        value.lines().count()
    }
}

fn clear_prompt_lines(lines: usize) -> anyhow::Result<()> {
    if lines == 0 {
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    for _ in 0..lines {
        crossterm::execute!(
            stdout,
            crossterm::cursor::MoveUp(1),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine),
        )?;
    }
    crossterm::execute!(stdout, crossterm::cursor::MoveToColumn(0))?;
    stdout.flush()?;
    Ok(())
}

pub fn clear_transient_mfa(lines: usize) -> anyhow::Result<()> {
    clear_prompt_lines(lines)
}
