use anyhow::bail;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute, terminal,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position},
    prelude::Frame,
    style::{Color, Modifier, Style},
    widgets::{Clear, List, ListItem, ListState, Paragraph},
};
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn collect_sbatch_scripts(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut matches = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", dir.display(), e))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() && path.extension() == Some(OsStr::new("sbatch")) {
                matches.push(path);
            }
        }
    }

    matches.sort();
    Ok(matches)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("interactive picker requires a TTY; pass --headless and specify --sbatchscript");
        }
        terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(err) = execute!(stdout, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

type RatatuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SbatchPickerPhase {
    Selecting,
    Selected(usize),
    Canceled,
}

struct SbatchPicker {
    scripts: Vec<String>,
    list_state: ListState,
    phase: SbatchPickerPhase,
    viewport_height: usize,
}

impl SbatchPicker {
    fn new(scripts: Vec<String>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let viewport_height = scripts.len().min(8).max(1);
        Self {
            scripts,
            list_state,
            phase: SbatchPickerPhase::Selecting,
            viewport_height,
        }
    }

    fn handle_event(&mut self, event: Event) {
        if self.phase != SbatchPickerPhase::Selecting {
            return;
        }
        let len = self.scripts.len();
        if len == 0 {
            self.phase = SbatchPickerPhase::Canceled;
            return;
        }
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Down => self.move_selection(1),
                KeyCode::PageUp => self.move_selection(-(self.viewport_height as isize)),
                KeyCode::PageDown => self.move_selection(self.viewport_height as isize),
                KeyCode::Home => self.select_index(0),
                KeyCode::End => self.select_index(len.saturating_sub(1)),
                KeyCode::Enter => {
                    let selected = self.list_state.selected().unwrap_or(0);
                    self.phase = SbatchPickerPhase::Selected(selected);
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.phase = SbatchPickerPhase::Canceled;
                }
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    self.phase = SbatchPickerPhase::Canceled;
                }
                _ => {}
            }
        }
    }

    fn select_index(&mut self, index: usize) {
        let index = index.min(self.scripts.len().saturating_sub(1));
        self.list_state.select(Some(index));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.scripts.len();
        if len == 0 {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, (len - 1) as isize) as usize;
        self.list_state.select(Some(next));
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);

        let items: Vec<ListItem> = self
            .scripts
            .iter()
            .map(|script| ListItem::new(script.as_str()))
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let list_area = if area.height >= 3 {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

            let header = Paragraph::new("Select sbatch script");
            frame.render_widget(header, layout[0]);

            let footer = Paragraph::new("Up/Down to move, Enter to select, Esc to cancel").style(
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            );
            frame.render_widget(footer, layout[2]);
            layout[1]
        } else if area.height == 2 {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area);
            let header = Paragraph::new("Select sbatch script");
            frame.render_widget(header, layout[0]);
            layout[1]
        } else {
            area
        };

        frame.render_stateful_widget(list, list_area, &mut self.list_state);
        self.viewport_height = list_area.height as usize;
    }

    fn inline_viewport_height(&self) -> u16 {
        let header = 1u16;
        let footer = 1u16;
        let max_list = 8u16;
        let list_height = (self.scripts.len().max(1) as u16).min(max_list);
        let desired = header + footer + list_height;
        desired.max(1)
    }

    fn clear(&mut self, terminal: &mut RatatuiTerminal, start: Position) -> anyhow::Result<()> {
        terminal.backend_mut().set_cursor_position(start)?;
        terminal
            .backend_mut()
            .clear_region(ClearType::AfterCursor)?;
        Ok(())
    }
}

fn pick_sbatch_script(scripts: Vec<String>) -> anyhow::Result<String> {
    let _guard = TerminalGuard::enter()?;
    let (cursor_x, cursor_y) = cursor::position()?;
    let (_, term_height) = terminal::size()?;
    let mut picker = SbatchPicker::new(scripts);
    let desired_height = picker.inline_viewport_height();
    let viewport_height = desired_height.min(term_height.max(1));
    let lines_after_cursor = viewport_height.saturating_sub(1);
    let available_lines = term_height.saturating_sub(cursor_y).saturating_sub(1);
    let scroll_lines = lines_after_cursor.saturating_sub(available_lines);
    let start_pos = Position::new(cursor_x, cursor_y.saturating_sub(scroll_lines));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;

    loop {
        terminal.draw(|frame| picker.render(frame))?;
        picker.handle_event(event::read()?);
        match picker.phase {
            SbatchPickerPhase::Selecting => {}
            SbatchPickerPhase::Selected(index) => {
                picker.clear(&mut terminal, start_pos)?;
                return Ok(picker.scripts[index].clone());
            }
            SbatchPickerPhase::Canceled => {
                picker.clear(&mut terminal, start_pos)?;
                bail!("sbatch selection canceled")
            }
        }
    }
}

pub fn resolve_sbatch_script(
    local_path: &Path,
    explicit: Option<&str>,
    headless: bool,
) -> anyhow::Result<String> {
    if let Some(sbatchscript) = explicit {
        return Ok(sbatchscript.to_string());
    }

    if !local_path.exists() {
        bail!("local path '{}' does not exist", local_path.display());
    }
    if !local_path.is_dir() {
        bail!(
            "local path '{}' must be a directory to auto-detect .sbatch scripts",
            local_path.display()
        );
    }

    let scripts = collect_sbatch_scripts(local_path)?;
    let relative_scripts: Vec<String> = scripts
        .iter()
        .map(|path| {
            let rel = path.strip_prefix(local_path).unwrap_or(path);
            rel.to_string_lossy().into_owned()
        })
        .collect();

    match relative_scripts.len() {
        0 => bail!(
            "no .sbatch files found under '{}'; provide the script path explicitly",
            local_path.display()
        ),
        1 => Ok(relative_scripts[0].clone()),
        _ => {
            if headless {
                let mut msg = format!(
                    "multiple .sbatch files found under '{}' while running in headless mode; specify which one to use with --sbatchscript:\n",
                    local_path.display()
                );
                for script in &relative_scripts {
                    msg.push_str(&format!("  - {}\n", script));
                }
                bail!("{}", msg.trim_end())
            }
            pick_sbatch_script(relative_scripts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cli_sbatch_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn collect_sbatch_scripts_finds_nested_files() {
        let root = temp_dir();
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let one = root.join("a.sbatch");
        let two = nested.join("b.sbatch");
        std::fs::write(&one, "echo one").unwrap();
        std::fs::write(&two, "echo two").unwrap();

        let scripts = collect_sbatch_scripts(&root).unwrap();
        let names: Vec<String> = scripts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["a.sbatch", "b.sbatch"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_sbatch_script_accepts_explicit_path() {
        let root = temp_dir();
        let script = resolve_sbatch_script(&root, Some("job.sbatch"), true).unwrap();
        assert_eq!(script, "job.sbatch");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_sbatch_script_errors_on_multiple_headless() {
        let root = temp_dir();
        std::fs::write(root.join("a.sbatch"), "echo one").unwrap();
        std::fs::write(root.join("b.sbatch"), "echo two").unwrap();

        let err = resolve_sbatch_script(&root, None, true).unwrap_err();
        assert!(err
            .to_string()
            .contains("multiple .sbatch files found"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
