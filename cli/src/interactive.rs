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
const INPUT_COLOR: Color = Color::Magenta;

#[derive(Debug)]
pub struct ResolvedAddClusterArgs {
    pub hostname: Option<String>,
    pub ip: Option<String>,
    pub username: String,
    pub hostid: String,
    pub port: u32,
    pub identity_path: String,
    pub default_base_path: Option<String>,
}

pub fn resolve_add_cluster_args(args: AddClusterArgs) -> anyhow::Result<ResolvedAddClusterArgs> {
    let destination = normalize_option(args.destination);
    let hostname = normalize_option(args.hostname);
    let ip = normalize_option(args.ip);
    let username = normalize_option(args.username);
    let hostid = normalize_option(args.hostid);
    let identity_path = normalize_option(args.identity_path);
    let default_base_path = normalize_option(args.default_base_path);
    let env_username = default_username();

    let parsed_destination = destination
        .as_deref()
        .map(parse_destination)
        .transpose()?;
    let dest_username = parsed_destination.as_ref().and_then(|d| d.username.as_ref());
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
            || hostid.is_none()
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
                    prompt_required("Username")?
                }
            },
        },
    };

    let hostid = match hostid {
        Some(value) => value,
        None => {
            let default_hostid = default_hostid_for_host(hostname.as_deref(), ip.as_deref())?;
            if args.headless {
                default_hostid
            } else {
                prompt_with_default("Host ID", &default_hostid)?
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
                prompt_port(DEFAULT_SSH_PORT)?
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
                prompt_with_default("Identity path", DEFAULT_IDENTITY_PATH)?
            }
        }
    };

    let default_base_path = match default_base_path {
        Some(value) => Some(value),
        None => {
            if args.headless {
                None
            } else {
                prompt_optional("Default base path")?
            }
        }
    };

    Ok(ResolvedAddClusterArgs {
        hostname,
        ip,
        username,
        hostid,
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

fn prompt_destination() -> anyhow::Result<ParsedDestination> {
    loop {
        let input = prompt_line("Destination ([user@]host[:port]): ")?;
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
        (host.trim().to_string(), Some(parse_destination_port(port_str)?))
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

fn default_hostid_for_host(hostname: Option<&str>, ip: Option<&str>) -> anyhow::Result<String> {
    if let Some(host) = hostname {
        if host.parse::<IpAddr>().is_ok() {
            return Ok(generate_random_hostid());
        }
        return Ok(host.to_string());
    }
    if ip.is_some() {
        return Ok(generate_random_hostid());
    }
    bail!("hostid default requires a hostname or ip")
}

fn generate_random_hostid() -> String {
    let mut rng = rand::rng();
    let adjective = ADJECTIVES
        .choose(&mut rng)
        .copied()
        .unwrap_or("curious");
    let scientist = SCIENTISTS
        .choose(&mut rng)
        .copied()
        .unwrap_or("einstein");
    format!("{adjective}_{scientist}")
}

fn prompt_required(label: &str) -> anyhow::Result<String> {
    loop {
        let input = prompt_line(&format!("{label}: "))?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} is required.");
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

fn prompt_with_default(label: &str, default: &str) -> anyhow::Result<String> {
    let input = prompt_line_with_prefill(&format!("{label}: "), default)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_optional(label: &str) -> anyhow::Result<Option<String>> {
    let input = prompt_line_with_prefill(&format!("{label}: "), "")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn prompt_port(default: u32) -> anyhow::Result<u32> {
    loop {
        let input = prompt_line_with_prefill("Port: ", &default.to_string())?;
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

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    prompt_line_with_prefill(prompt, "")
}

fn prompt_line_with_prefill(prompt: &str, initial: &str) -> anyhow::Result<String> {
    let _guard = RawModeGuard::enter()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, cursor::MoveToColumn(0))?;
    let mut editor = LineEditor::new(prompt, 0, initial);
    editor.render(&mut stdout)?;

    loop {
        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Enter => {
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

    Ok(editor.into_string())
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
    start_col: u16,
    buffer: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    fn new(prompt: &str, start_col: u16, initial: &str) -> Self {
        let buffer: Vec<char> = initial.chars().collect();
        let cursor = buffer.len();
        Self {
            prompt: prompt.to_string(),
            prompt_len: prompt.len().min(u16::MAX as usize) as u16,
            start_col,
            buffer,
            cursor,
        }
    }

    fn insert(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
    }

    fn delete(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.buffer.remove(self.cursor);
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
            SetForegroundColor(INPUT_COLOR),
            Print(buffer_string),
            ResetColor,
        )?;
        let cursor_col = self
            .start_col
            .saturating_add(self.prompt_len)
            .saturating_add(self.cursor.min(u16::MAX as usize) as u16);
        execute!(stdout, cursor::MoveToColumn(cursor_col))?;
        stdout.flush()?;
        Ok(())
    }

    fn into_string(self) -> String {
        self.buffer.into_iter().collect()
    }
}

const ADJECTIVES: &[&str] = &[
    "bold",
    "brisk",
    "calm",
    "clever",
    "curious",
    "daring",
    "eager",
    "gentle",
    "grand",
    "happy",
    "jolly",
    "kind",
    "lively",
    "mighty",
    "noble",
    "proud",
    "quiet",
    "rapid",
    "sharp",
    "witty",
];

const SCIENTISTS: &[&str] = &[
    "bohr",
    "curie",
    "darwin",
    "einstein",
    "fermi",
    "feynman",
    "gauss",
    "galilei",
    "goodall",
    "hawking",
    "kepler",
    "lovelace",
    "mendel",
    "newton",
    "noether",
    "planck",
    "tesla",
    "turing",
    "weber",
    "wilson",
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
            hostid: None,
            port: None,
            identity_path: None,
            default_base_path: None,
            headless: true,
        };

        let resolved = resolve_add_cluster_args(args).unwrap();
        assert_eq!(resolved.port, DEFAULT_SSH_PORT);
        assert_eq!(resolved.identity_path, DEFAULT_IDENTITY_PATH);
        assert!(resolved.default_base_path.is_none());
        assert_eq!(resolved.username, "alex");
        assert_eq!(resolved.hostid, "example.com");
    }

    #[test]
    fn resolve_add_cluster_headless_requires_destination() {
        let args = AddClusterArgs {
            destination: None,
            hostname: None,
            ip: None,
            username: Some("alex".into()),
            hostid: None,
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
    fn random_hostid_uses_known_words() {
        let hostid = generate_random_hostid();
        let mut parts = hostid.split('_');
        let adjective = parts.next().unwrap_or_default();
        let scientist = parts.next().unwrap_or_default();
        assert!(ADJECTIVES.contains(&adjective));
        assert!(SCIENTISTS.contains(&scientist));
    }
}
