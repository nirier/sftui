use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use lscolors::{Color as LsColor, Indicator, LsColors, Style as LsStyle};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use crate::app::{ActionMode, App, Bookmark, Pane, SyncDirection, TransferItem};
use crate::sftp::FileInfo;
use crate::ssh_config::SshHost;

pub struct Ui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Ui {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Ui { terminal })
    }

    pub fn draw(&mut self, app: &App) -> Result<()> {
        let current_host = app.current_host.clone();
        let active_pane = app.active_pane;
        let action_mode = app.action_mode;
        let local_path = app.local_path.clone();
        let remote_path = app.remote_path.clone();
        let local_cursor = app.local_cursor;
        let remote_cursor = app.remote_cursor;
        let local_selected = app.local_selected.clone();
        let remote_selected = app.remote_selected.clone();
        let show_connection_dialog = app.show_connection_dialog;
        let show_transfer_dialog = app.show_transfer_dialog;
        let available_hosts = app.available_hosts.clone();
        let connection_cursor = app.connection_cursor;
        let transfer_queue = app.transfer_queue.clone();
        let show_bookmark_dialog = app.show_bookmark_dialog;
        let bookmarks = app.bookmarks.clone();
        let bookmark_cursor = app.bookmark_cursor;
        let bookmark_editing = app.bookmark_editing;
        let bookmark_name = app.bookmark_name.clone();
        let show_sync_dialog = app.show_sync_dialog;
        let sync_direction = app.sync_direction;
        let sync_dry_run = app.sync_dry_run;
        let sync_completed = app.sync_completed;
        let sync_output = app.sync_output.clone();
        let lscolors = LsColors::from_env().unwrap_or_default();

        self.terminal.draw(move |f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Length(3),
                        Constraint::Min(0),
                        Constraint::Length(3),
                    ]
                    .as_ref(),
                )
                .split(f.area());

            Ui::draw_header(f, chunks[0], &current_host);
            Ui::draw_panes(
                f,
                chunks[1],
                &active_pane,
                &local_path,
                &remote_path,
                app.get_current_local_files(),
                app.get_current_remote_files(),
                local_cursor,
                remote_cursor,
                &local_selected,
                &remote_selected,
                &lscolors,
            );
            Ui::draw_footer(
                f,
                chunks[2],
                app.search_mode,
                &app.search_query,
                action_mode,
            );

            if show_connection_dialog {
                Ui::draw_connection_dialog(f, &available_hosts, connection_cursor);
            }

            if show_transfer_dialog {
                Ui::draw_transfer_dialog(f, &transfer_queue);
            }

            if show_bookmark_dialog {
                Ui::draw_bookmark_dialog(
                    f,
                    &bookmarks,
                    bookmark_cursor,
                    bookmark_editing,
                    &bookmark_name,
                );
            }

            if show_sync_dialog {
                Ui::draw_sync_dialog(
                    f,
                    sync_direction,
                    sync_dry_run,
                    sync_completed,
                    &sync_output,
                );
            }
        })?;

        Ok(())
    }

    fn draw_header(f: &mut Frame, area: Rect, current_host: &Option<String>) {
        let title = format!(
            "SFTP TUI - Connected to: {}",
            current_host
                .as_ref()
                .unwrap_or(&"Not Connected".to_string())
        );
        let header = Paragraph::new(title)
            .block(Block::default().borders(Borders::ALL))
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(header, area);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_panes(
        f: &mut Frame,
        area: Rect,
        active_pane: &Pane,
        local_path: &Path,
        remote_path: &Path,
        local_files: &[FileInfo],
        remote_files: &[FileInfo],
        local_cursor: usize,
        remote_cursor: usize,
        local_selected: &HashSet<usize>,
        remote_selected: &HashSet<usize>,
        lscolors: &LsColors,
    ) {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
            .split(area);

        Ui::draw_local_pane(
            f,
            panes[0],
            active_pane,
            local_path,
            local_files,
            local_cursor,
            local_selected,
            lscolors,
        );
        Ui::draw_remote_pane(
            f,
            panes[1],
            active_pane,
            remote_path,
            remote_files,
            remote_cursor,
            remote_selected,
            lscolors,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_local_pane(
        f: &mut Frame,
        area: Rect,
        active_pane: &Pane,
        local_path: &Path,
        local_files: &[FileInfo],
        local_cursor: usize,
        local_selected: &HashSet<usize>,
        lscolors: &LsColors,
    ) {
        let title = format!("Local: {} ({})", local_path.display(), local_files.len());
        let style = if *active_pane == Pane::Local {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        };

        let items: Vec<ListItem> = local_files
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let name = format!(" {}", display_name(file));
                let mut item_style = style_for_file(lscolors, file, true);

                if local_selected.contains(&i) {
                    item_style = item_style.bg(Color::Blue);
                }

                ListItem::new(Line::from(Span::styled(name, item_style)))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(local_cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_remote_pane(
        f: &mut Frame,
        area: Rect,
        active_pane: &Pane,
        remote_path: &Path,
        remote_files: &[FileInfo],
        remote_cursor: usize,
        remote_selected: &HashSet<usize>,
        lscolors: &LsColors,
    ) {
        let title = format!("Remote: {} ({})", remote_path.display(), remote_files.len());
        let style = if *active_pane == Pane::Remote {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        };

        let items: Vec<ListItem> = remote_files
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let name = format!(" {}", display_name(file));
                let mut item_style = style_for_file(lscolors, file, false);

                if remote_selected.contains(&i) {
                    item_style = item_style.bg(Color::Blue);
                }

                ListItem::new(Line::from(Span::styled(name, item_style)))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(remote_cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_footer(
        f: &mut Frame,
        area: Rect,
        search_mode: bool,
        search_query: &str,
        action_mode: ActionMode,
    ) {
        let footer_text = if search_mode {
            format!("Search: {search_query} | Esc: Cancel | Enter: Exit search")
        } else {
            let mode = match action_mode {
                ActionMode::Single => "Single",
                ActionMode::Dual => "Dual",
            };
            [
                "Tab: Switch panes".to_string(),
                format!("A: Action mode ({mode})"),
                "j/k or ↑/↓: Move".to_string(),
                "h: Parent | l/Enter: Open".to_string(),
                "g/G: Top/bottom".to_string(),
                "Space: Select".to_string(),
                "T: Transfer files".to_string(),
                "C: Change connection".to_string(),
                "M: Save bookmark | B: Bookmarks".to_string(),
                "S: Sync".to_string(),
                "/: Search".to_string(),
                "Q: Quit".to_string(),
            ]
            .join(" | ")
        };

        let footer = Paragraph::new(footer_text)
            .block(Block::default().borders(Borders::ALL))
            .style(if search_mode {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Cyan)
            });
        f.render_widget(footer, area);
    }

    fn draw_connection_dialog(
        f: &mut Frame,
        available_hosts: &[SshHost],
        connection_cursor: usize,
    ) {
        let area = Ui::centered_rect(60, 20, f.area());

        f.render_widget(Clear, area);

        let hosts: Vec<ListItem> = available_hosts
            .iter()
            .map(|host| {
                let display = format!(
                    "{} ({})",
                    host.host,
                    host.hostname.as_ref().unwrap_or(&host.host)
                );
                ListItem::new(display)
            })
            .collect();

        let list = List::new(hosts)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Select Host (j/k: move, g/G: top/bottom)"),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(connection_cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_transfer_dialog(f: &mut Frame, transfer_queue: &[TransferItem]) {
        let area = Ui::centered_rect(80, 30, f.area());

        f.render_widget(Clear, area);

        let items: Vec<ListItem> = transfer_queue
            .iter()
            .map(|item| {
                let direction = match item.direction {
                    crate::app::TransferDirection::Upload => "",
                    crate::app::TransferDirection::Download => "",
                };
                let text = format!(
                    "{} {} -> {}",
                    direction,
                    item.source.display(),
                    item.destination.display()
                );
                ListItem::new(text)
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Transfer Queue (Enter to confirm, Esc to cancel)"),
            )
            .style(Style::default().fg(Color::Yellow));

        f.render_widget(list, area);
    }

    fn draw_bookmark_dialog(
        f: &mut Frame,
        bookmarks: &[Bookmark],
        bookmark_cursor: usize,
        editing: bool,
        bookmark_name: &str,
    ) {
        let area = Ui::centered_rect(85, 55, f.area());
        f.render_widget(Clear, area);

        if editing {
            let input = Paragraph::new(format!(
                "Bookmark name: {}\n\nEnter: Save/update | Esc: Cancel",
                bookmark_name
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Save Bookmark"),
            )
            .style(Style::default().fg(Color::Yellow));
            f.render_widget(input, area);
            return;
        }

        let items: Vec<ListItem> = if bookmarks.is_empty() {
            vec![ListItem::new("No bookmarks. Press a to add one.")]
        } else {
            bookmarks
                .iter()
                .map(|bookmark| {
                    let host = bookmark.host.as_deref().unwrap_or("not connected");
                    ListItem::new(format!(
                        "{}  |  local: {}  |  remote: {}  |  host: {}",
                        bookmark.name,
                        bookmark.local_path.display(),
                        bookmark.remote_path.display(),
                        host
                    ))
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Bookmarks (Enter: restore, a/m: add, d: delete, Esc: close)"),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if !bookmarks.is_empty() {
            state.select(Some(bookmark_cursor));
        }
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_sync_dialog(
        f: &mut Frame,
        direction: SyncDirection,
        dry_run: bool,
        completed: bool,
        output: &str,
    ) {
        let area = Ui::centered_rect(85, 65, f.area());
        f.render_widget(Clear, area);

        let direction = match direction {
            SyncDirection::LocalToRemote => "Local -> Remote",
            SyncDirection::RemoteToLocal => "Remote -> Local",
        };
        let body = if completed {
            format!("rsync result:\n\n{output}\n\nEnter/Esc: Close")
        } else {
            format!(
                "Direction: {direction}\nDry-run: {}\n\nLeft/Right or Tab: Change direction\nd: Toggle dry-run\nEnter: Run rsync\nEsc: Cancel\n\nThe destination will be made an exact mirror with --delete.",
                if dry_run {
                    "ON (safe preview)"
                } else {
                    "OFF (writes/deletes files)"
                }
            )
        };

        let paragraph = Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Directory Sync"),
            )
            .style(Style::default().fg(if completed {
                Color::Green
            } else {
                Color::Yellow
            }))
            .wrap(ratatui::widgets::Wrap { trim: false });
        f.render_widget(paragraph, area);
    }

    fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(r);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(popup_layout[1])[1]
    }

    pub fn handle_events(&self) -> Result<Option<Event>> {
        if crossterm::event::poll(std::time::Duration::from_millis(100))? {
            let event = crossterm::event::read()?;
            if let Event::Key(key) = &event
                && key.kind == KeyEventKind::Press
            {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
    }
}

fn display_name(file: &FileInfo) -> String {
    let suffix = if file.is_symlink {
        "@"
    } else if file.is_dir {
        "/"
    } else if file.permissions & 0o111 != 0 {
        "*"
    } else {
        ""
    };
    format!("{}{}", file.name, suffix)
}

fn style_for_file(lscolors: &LsColors, file: &FileInfo, local: bool) -> Style {
    let ls_style = if local {
        fs::symlink_metadata(&file.path)
            .ok()
            .and_then(|metadata| lscolors.style_for_path_with_metadata(&file.path, Some(&metadata)))
    } else {
        let indicator = if file.is_symlink {
            Indicator::SymbolicLink
        } else if file.is_dir {
            Indicator::Directory
        } else if file.permissions & 0o111 != 0 {
            Indicator::ExecutableFile
        } else {
            Indicator::RegularFile
        };
        if indicator == Indicator::RegularFile {
            lscolors
                .style_for_str(&file.name)
                .or_else(|| lscolors.style_for_indicator(indicator))
        } else {
            lscolors.style_for_indicator(indicator)
        }
    };

    ls_style.map(to_ratatui_style).unwrap_or_default()
}

fn to_ratatui_style(style: &LsStyle) -> Style {
    let mut ratatui_style = Style::default();
    if let Some(color) = style.foreground {
        ratatui_style = ratatui_style.fg(to_ratatui_color(color));
    }
    if let Some(color) = style.background {
        ratatui_style = ratatui_style.bg(to_ratatui_color(color));
    }
    if let Some(color) = style.underline {
        ratatui_style = ratatui_style.underline_color(to_ratatui_color(color));
    }

    let font = style.font_style;
    let mut modifiers = Modifier::empty();
    if font.bold {
        modifiers |= Modifier::BOLD;
    }
    if font.dimmed {
        modifiers |= Modifier::DIM;
    }
    if font.italic {
        modifiers |= Modifier::ITALIC;
    }
    if font.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if font.reverse {
        modifiers |= Modifier::REVERSED;
    }
    if font.hidden {
        modifiers |= Modifier::HIDDEN;
    }
    if font.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    ratatui_style.add_modifier(modifiers)
}

fn to_ratatui_color(color: LsColor) -> Color {
    match color {
        LsColor::Black => Color::Black,
        LsColor::Red => Color::Red,
        LsColor::Green => Color::Green,
        LsColor::Yellow => Color::Yellow,
        LsColor::Blue => Color::Blue,
        LsColor::Magenta => Color::Magenta,
        LsColor::Cyan => Color::Cyan,
        LsColor::White => Color::Gray,
        LsColor::BrightBlack => Color::DarkGray,
        LsColor::BrightRed => Color::Red,
        LsColor::BrightGreen => Color::Green,
        LsColor::BrightYellow => Color::Yellow,
        LsColor::BrightBlue => Color::Blue,
        LsColor::BrightMagenta => Color::Magenta,
        LsColor::BrightCyan => Color::Cyan,
        LsColor::BrightWhite => Color::White,
        LsColor::Fixed(value) => Color::Indexed(value),
        LsColor::RGB(red, green, blue) => Color::Rgb(red, green, blue),
    }
}
