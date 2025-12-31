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
