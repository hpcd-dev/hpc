// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::args::AddClusterArgs;
use anyhow::bail;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{self, ClearType};
use rand::prelude::IndexedRandom;
use std::io::{IsTerminal, Write};
use std::net::IpAddr;

const DEFAULT_SSH_PORT: u32 = 22;
const DEFAULT_IDENTITY_PATH: &str = "~/.ssh/id_ed25519";
const DEFAULT_BASE_PATH: &str = "~";
const HINT_COLOR: Color = Color::DarkGrey;

#[derive(Debug)]
pub struct ResolvedAddClusterArgs {
    pub hostname: Option<String>,
    pub ip: Option<String>,
    pub username: String,
    pub name: String,
    pub port: u32,
    pub identity_path: String,
    pub default_base_path: Option<String>,
}

pub fn prompt_default_base_path(default_value: &str) -> anyhow::Result<String> {
    ensure_tty_for_prompt()?;
    prompt_with_default(
        "Default base path",
        default_value,
        "Remote base folder for projects.",
    )
}

pub fn resolve_add_cluster_args(args: AddClusterArgs) -> anyhow::Result<ResolvedAddClusterArgs> {
    let destination = normalize_option(args.destination);
    let hostname = normalize_option(args.hostname);
    let ip = normalize_option(args.ip);
    let username = normalize_option(args.username);
    let name = normalize_option(args.name);
    let identity_path = normalize_option(args.identity_path);
    let default_base_path = normalize_option(args.default_base_path);
    let env_username = default_username();

    let parsed_destination = destination.as_deref().map(parse_destination).transpose()?;
    let dest_username = parsed_destination
        .as_ref()
        .and_then(|d| d.username.as_ref());
    let dest_port = parsed_destination.as_ref().and_then(|d| d.port);

    if parsed_destination.is_some() && (hostname.is_some() || ip.is_some()) {
        bail!("--destination cannot be combined with --hostname or --ip");
    }

    if let (Some(_), Some(_)) = (dest_username, username.as_ref()) {
        bail!("username cannot be provided both in destination and --username");
    }

    if let (Some(_), Some(_)) = (dest_port, args.port) {
        bail!("port cannot be provided both in destination and --port");
    }

    let needs_prompt = !args.headless
        && ((parsed_destination.is_none() && hostname.is_none() && ip.is_none())
            || name.is_none()
            || (args.port.is_none() && dest_port.is_none())
            || identity_path.is_none()
            || default_base_path.is_none()
            || (username.is_none() && dest_username.is_none() && env_username.is_none()));
    if needs_prompt {
        ensure_tty_for_prompt()?;
    }

    let parsed_destination = if let Some(parsed) = parsed_destination {
        Some(parsed)
    } else if hostname.is_none() && ip.is_none() {
        if args.headless {
            bail!("destination, hostname, or ip is required in headless mode");
        }
        Some(prompt_destination()?)
    } else {
        None
    };

    let (hostname, ip) = match (parsed_destination.as_ref(), hostname, ip) {
        (Some(parsed), None, None) => {
            if parsed.host.parse::<IpAddr>().is_ok() {
                (None, Some(parsed.host.clone()))
            } else {
                (Some(parsed.host.clone()), None)
            }
        }
        (None, Some(hostname), None) => (Some(hostname), None),
        (None, None, Some(ip)) => (None, Some(ip)),
        (None, None, None) => unreachable!("destination or hostname/ip must be resolved"),
        (None, Some(_), Some(_)) => bail!("hostname and ip cannot be provided at the same time"),
        _ => unreachable!("destination parsing should cover all cases"),
    };

    let username = match parsed_destination.as_ref().and_then(|d| d.username.clone()) {
        Some(value) => value,
        None => match username {
            Some(value) => value,
            None => match env_username {
                Some(value) => value,
                None => {
                    if args.headless {
                        bail!("--username is required in headless mode");
                    }
                    prompt_required("Username", "SSH login user for the cluster")?
                }
            },
        },
    };

    let name = match name {
        Some(value) => value,
        None => {
            if args.headless {
                default_name_for_host(hostname.as_deref(), ip.as_deref())?
            } else {
                let default_name = generate_random_name();
                prompt_with_default(
                    "Name",
                    &default_name,
                    "Friendly name used in other commands (e.g. gpu01).",
                )?
            }
        }
    };

    let port = match (parsed_destination.and_then(|d| d.port), args.port) {
        (Some(value), None) => value,
        (None, Some(value)) => value,
        (None, None) => {
            if args.headless {
                DEFAULT_SSH_PORT
            } else {
                prompt_port(DEFAULT_SSH_PORT, "SSH port for the cluster")?
            }
        }
        (Some(_), Some(_)) => unreachable!("port conflicts handled earlier"),
    };

    let identity_path = match identity_path {
        Some(value) => value,
        None => {
            if args.headless {
                DEFAULT_IDENTITY_PATH.to_string()
            } else {
                prompt_with_default(
                    "Identity path",
                    DEFAULT_IDENTITY_PATH,
                    "SSH private key path used for authentication.",
                )?
            }
        }
    };

    let default_base_path = match default_base_path {
        Some(value) => Some(value),
        None => {
            if args.headless {
                Some(DEFAULT_BASE_PATH.to_string())
            } else {
                None
            }
        }
    };

    Ok(ResolvedAddClusterArgs {
        hostname,
        ip,
        username,
        name,
        port,
        identity_path,
        default_base_path,
    })
}

struct ParsedDestination {
    username: Option<String>,
    host: String,
    port: Option<u32>,
}

fn normalize_option(value: Option<String>) -> Option<String> {
    value.and_then(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn default_username() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn ensure_tty_for_prompt() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive prompts require a TTY; pass --headless and specify all options");
    }
    Ok(())
}

pub fn confirm_action(prompt: &str, hint: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("confirmation requires a TTY; pass --yes to skip the prompt");
    }
    loop {
        let input = prompt_line_with_default(prompt, hint, Some("no"))?;
        let normalized = input.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" | "" => return Ok(false),
            _ => eprintln!("Please answer 'yes' or 'no'."),
        }
    }
}

fn prompt_destination() -> anyhow::Result<ParsedDestination> {
    loop {
        let input = prompt_line(
            "Destination: ",
            "SSH destination in [user@]host[:port] form.",
        )?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("Destination is required.");
            continue;
        }
        match parse_destination(trimmed) {
            Ok(parsed) => return Ok(parsed),
            Err(err) => eprintln!("{err}"),
        }
    }
}

fn parse_destination(input: &str) -> anyhow::Result<ParsedDestination> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("destination is required");
    }

    let (username_part, host_part) = match trimmed.split_once('@') {
        Some((user, host)) => {
            if user.trim().is_empty() {
                bail!("destination username is empty");
            }
            (Some(user.trim().to_string()), host.trim())
        }
        None => (None, trimmed),
    };

    let host_part = host_part.trim();
    if host_part.is_empty() {
        bail!("destination host is required");
    }

    let (host, port) = if let Some(rest) = host_part.strip_prefix('[') {
        let Some((host, remainder)) = rest.split_once(']') else {
            bail!("destination IPv6 host must end with ']'");
        };
        let remainder = remainder.trim();
        if remainder.is_empty() {
            (host.to_string(), None)
        } else if let Some(port_str) = remainder.strip_prefix(':') {
            (host.to_string(), Some(parse_destination_port(port_str)?))
        } else {
            bail!("destination has unexpected trailing characters after ']'");
        }
    } else if host_part.matches(':').count() > 1 {
        if host_part.parse::<IpAddr>().is_err() {
            bail!("destination host must be IPv6 when using multiple ':' characters");
        }
        (host_part.to_string(), None)
    } else if let Some((host, port_str)) = host_part.rsplit_once(':') {
        if host.trim().is_empty() {
            bail!("destination host is required");
        }
        (
            host.trim().to_string(),
            Some(parse_destination_port(port_str)?),
        )
    } else {
        (host_part.to_string(), None)
    };

    Ok(ParsedDestination {
        username: username_part,
        host,
        port,
    })
}

fn parse_destination_port(value: &str) -> anyhow::Result<u32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("destination port is required after ':'");
    }
    match trimmed.parse::<u32>() {
        Ok(port) => Ok(port),
        Err(_) => bail!("destination port must be a number"),
    }
}

fn default_name_for_host(hostname: Option<&str>, ip: Option<&str>) -> anyhow::Result<String> {
    if let Some(host) = hostname {
        if host.parse::<IpAddr>().is_ok() {
            return Ok(generate_random_name());
        }
        return Ok(host.to_string());
    }
    if ip.is_some() {
        return Ok(generate_random_name());
    }
    bail!("name default requires a hostname or ip")
}

fn generate_random_name() -> String {
    let mut rng = rand::rng();
    let adjective = ADJECTIVES.choose(&mut rng).copied().unwrap_or("curious");
    let scientist = SCIENTISTS.choose(&mut rng).copied().unwrap_or("einstein");
    format!("{adjective}_{scientist}")
}

fn prompt_required(label: &str, hint: &str) -> anyhow::Result<String> {
    loop {
        let input = prompt_line(&format!("{label}: "), hint)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} is required.");
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

fn prompt_with_default(label: &str, default: &str, hint: &str) -> anyhow::Result<String> {
    let hint = format_default_hint(default, hint);
    let input = prompt_line_with_default(&format!("{label}: "), &hint, Some(default))?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_port(default: u32, hint: &str) -> anyhow::Result<u32> {
    let default_str = default.to_string();
    let hint = format_default_hint(default_str.as_str(), hint);
    loop {
        let input = prompt_line_with_default("Port: ", &hint, Some(default_str.as_str()))?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        match trimmed.parse::<u32>() {
            Ok(value) => return Ok(value),
            Err(_) => eprintln!("Invalid port; expected a number."),
        }
    }
}

fn prompt_line(prompt: &str, hint: &str) -> anyhow::Result<String> {
    prompt_line_with_default(prompt, hint, None)
}

fn format_default_hint(default: &str, hint: &str) -> String {
    let trimmed = hint.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        format!("{default} - {trimmed}")
    }
}

fn prompt_line_with_default(
    prompt: &str,
    hint: &str,
    default: Option<&str>,
) -> anyhow::Result<String> {
    Ok(prompt_line_with_default_result(prompt, hint, default)?.input)
}

struct PromptLineResult {
    input: String,
    used_default: bool,
}

fn prompt_line_with_default_result(
    prompt: &str,
    hint: &str,
    default: Option<&str>,
) -> anyhow::Result<PromptLineResult> {
    let _guard = RawModeGuard::enter()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, cursor::MoveToColumn(0))?;
    let mut editor = LineEditor::new(prompt, 0, hint, default);
    editor.render(&mut stdout)?;
    let mut used_default = false;

    loop {
        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Enter => {
                    if editor.apply_default_if_empty() {
                        used_default = true;
                        editor.render(&mut stdout)?;
                    }
                    execute!(stdout, ResetColor, Print("\r\n"))?;
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    execute!(stdout, ResetColor, Print("\r\n"))?;
                    bail!("prompt canceled");
                }
                KeyCode::Esc => {
                    execute!(stdout, ResetColor, Print("\r\n"))?;
                    bail!("prompt canceled");
                }
                KeyCode::Backspace => {
                    editor.backspace();
                }
                KeyCode::Delete => {
                    editor.delete();
                }
                KeyCode::Left => {
                    editor.move_left();
                }
                KeyCode::Right => {
                    editor.move_right();
                }
                KeyCode::Home => {
                    editor.move_home();
                }
                KeyCode::End => {
                    editor.move_end();
                }
                KeyCode::Char(c) => {
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                        editor.insert(c);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        editor.render(&mut stdout)?;
    }

    Ok(PromptLineResult {
        input: editor.into_string(),
        used_default,
    })
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

struct LineEditor {
    prompt: String,
    prompt_len: u16,
    hint: String,
    default_value: Option<String>,
    start_col: u16,
    buffer: Vec<char>,
    cursor: usize,
    has_typed: bool,
}

impl LineEditor {
    fn new(prompt: &str, start_col: u16, hint: &str, default: Option<&str>) -> Self {
        Self {
            prompt: prompt.to_string(),
            prompt_len: prompt.len().min(u16::MAX as usize) as u16,
            hint: hint.to_string(),
            default_value: default.map(|value| value.to_string()),
            start_col,
            buffer: Vec::new(),
            cursor: 0,
            has_typed: false,
        }
    }

    fn insert(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += 1;
        self.has_typed = true;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
        self.has_typed = true;
    }

    fn delete(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.buffer.remove(self.cursor);
        self.has_typed = true;
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor += 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    fn render(&self, stdout: &mut std::io::Stdout) -> anyhow::Result<()> {
        let buffer_string: String = self.buffer.iter().collect();
        execute!(
            stdout,
            cursor::MoveToColumn(self.start_col),
            terminal::Clear(ClearType::UntilNewLine),
            Print(&self.prompt),
        )?;
        if buffer_string.is_empty() {
            if !self.hint.is_empty() {
                execute!(
                    stdout,
                    SetForegroundColor(HINT_COLOR),
                    Print(&self.hint),
                    ResetColor,
                )?;
            }
        } else {
            execute!(stdout, Print(&buffer_string))?;
        }
        let cursor_col = self
            .start_col
            .saturating_add(self.prompt_len)
            .saturating_add(self.cursor.min(u16::MAX as usize) as u16);
        execute!(stdout, cursor::MoveToColumn(cursor_col))?;
        stdout.flush()?;
        Ok(())
    }

    fn apply_default_if_empty(&mut self) -> bool {
        if !self.buffer.is_empty() {
            return false;
        }
        let Some(default_value) = self.default_value.as_ref() else {
            return false;
        };
        self.buffer.extend(default_value.chars());
        self.cursor = self.buffer.len();
        self.has_typed = true;
        true
    }

    fn into_string(self) -> String {
        self.buffer.into_iter().collect()
    }
}

const ADJECTIVES: &[&str] = &[
    "bold", "brisk", "calm", "clever", "curious", "daring", "eager", "gentle", "grand", "happy",
    "jolly", "kind", "lively", "mighty", "noble", "proud", "quiet", "rapid", "sharp", "witty",
];

const SCIENTISTS: &[&str] = &[
    "bohr", "curie", "darwin", "einstein", "fermi", "feynman", "gauss", "galilei", "goodall",
    "hawking", "kepler", "lovelace", "mendel", "newton", "noether", "planck", "tesla", "turing",
    "weber", "wilson",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_add_cluster_headless_defaults() {
        let args = AddClusterArgs {
            destination: Some("alex@example.com".into()),
            hostname: None,
            ip: None,
            username: None,
            name: None,
            port: None,
            identity_path: None,
            default_base_path: None,
            headless: true,
        };

        let resolved = resolve_add_cluster_args(args).unwrap();
        assert_eq!(resolved.port, DEFAULT_SSH_PORT);
        assert_eq!(resolved.identity_path, DEFAULT_IDENTITY_PATH);
        assert_eq!(resolved.default_base_path.as_deref(), Some(DEFAULT_BASE_PATH));
        assert_eq!(resolved.username, "alex");
        assert_eq!(resolved.name, "example.com");
    }

    #[test]
    fn resolve_add_cluster_headless_requires_destination() {
        let args = AddClusterArgs {
            destination: None,
            hostname: None,
            ip: None,
            username: Some("alex".into()),
            name: None,
            port: Some(2222),
            identity_path: Some("~/.ssh/id_rsa".into()),
            default_base_path: None,
            headless: true,
        };

        let err = resolve_add_cluster_args(args).unwrap_err();
        assert!(err.to_string().contains("destination"));
    }

    #[test]
    fn parse_destination_with_user_port() {
        let parsed = parse_destination("user@example.com:2222").unwrap();
        assert_eq!(parsed.username.as_deref(), Some("user"));
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, Some(2222));
    }

    #[test]
    fn random_name_uses_known_words() {
        let name = generate_random_name();
        let mut parts = name.split('_');
        let adjective = parts.next().unwrap_or_default();
        let scientist = parts.next().unwrap_or_default();
        assert!(ADJECTIVES.contains(&adjective));
        assert!(SCIENTISTS.contains(&scientist));
    }
}
