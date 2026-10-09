use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::health::{AuthStatus, HealthReport, ModelSettings};

pub struct Tui {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    should_quit: bool,
    health: Option<HealthReport>,
    log_lines: Vec<String>,
    show_settings: bool,
    settings_draft: ModelSettings,
}

impl Tui {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            should_quit: false,
            health: None,
            log_lines: Vec::new(),
            show_settings: false,
            settings_draft: ModelSettings::default(),
        })
    }

    pub fn set_health(&mut self, health: HealthReport) {
        self.health = Some(health);
    }

    pub fn log(&mut self, line: impl Into<String>) {
        self.log_lines.push(line.into());
        if self.log_lines.len() > 100 {
            self.log_lines.remove(0);
        }
    }

    pub fn run(&mut self) -> Result<()> {
        while !self.should_quit {
            let health = self.health.clone();
            let log_lines = self.log_lines.clone();
            let show_settings = self.show_settings;
            let settings_draft = self.settings_draft.clone();
            self.terminal.draw(|f| {
                draw_frame(
                    f,
                    health.as_ref(),
                    &log_lines,
                    show_settings,
                    &settings_draft,
                );
            })?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code);
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('s') => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    self.settings_draft = self
                        .health
                        .as_ref()
                        .map(|h| h.model_settings.clone())
                        .unwrap_or_default();
                }
            }
            KeyCode::Char('t') if self.show_settings => {
                self.settings_draft.deep_thinking = !self.settings_draft.deep_thinking;
            }
            KeyCode::Char('e') if self.show_settings => {
                self.settings_draft.search_enabled = !self.settings_draft.search_enabled;
            }
            _ => {}
        }
    }
}

fn draw_frame(
    frame: &mut Frame,
    health: Option<&HealthReport>,
    log_lines: &[String],
    show_settings: bool,
    settings_draft: &ModelSettings,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(12),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0]);
    draw_status(frame, chunks[1], health);
    draw_log(frame, chunks[2], log_lines);
    draw_footer(frame, chunks[3]);

    if show_settings {
        draw_settings(frame, settings_draft);
    }
}

fn draw_header(frame: &mut Frame, area: Rect) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "FreeChatCode",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " — DeepSeek Chat bridge for Codewhale",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Bridge "));
    frame.render_widget(title, area);
}

fn draw_status(frame: &mut Frame, area: Rect, health: Option<&HealthReport>) {
    let codewhale_status = health
        .as_ref()
        .and_then(|h| h.codewhale_binary.as_ref())
        .map(|b| format!("codewhale: {}", b))
        .unwrap_or_else(|| "codewhale: not found".to_string());

    let version = health
        .as_ref()
        .and_then(|h| h.codewhale_version.as_ref())
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let relay_status = health
        .as_ref()
        .map(|h| {
            if h.relay_reachable {
                "relay: connected"
            } else {
                "relay: waiting..."
            }
        })
        .unwrap_or("relay: not started");

    let auth_status = health
        .as_ref()
        .map(|h| match h.auth_status {
            AuthStatus::SignedIn => "auth: signed in",
            AuthStatus::SignedOut => "auth: sign in required",
            AuthStatus::Expired => "auth: session expired",
            AuthStatus::Unknown => "auth: checking...",
        })
        .unwrap_or("auth: unknown");

    let browser_status = health
        .as_ref()
        .map(|h| {
            if h.browser_ready {
                "browser: ready"
            } else {
                "browser: launching..."
            }
        })
        .unwrap_or("browser: not started");

    let text = format!(
        "{}\n{}\n{}\n{}\n{}",
        codewhale_status, version, relay_status, auth_status, browser_status
    );
    let widget = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Status "))
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

fn draw_log(frame: &mut Frame, area: Rect, log_lines: &[String]) {
    let lines: Vec<Line> = log_lines
        .iter()
        .rev()
        .take(area.height as usize)
        .rev()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Color::Gray))))
        .collect();
    let widget = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Log "));
    frame.render_widget(widget, area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::raw(" settings  "),
        Span::styled("t", Style::default().fg(Color::Yellow)),
        Span::raw(" toggle deepthinking  "),
        Span::styled("e", Style::default().fg(Color::Yellow)),
        Span::raw(" toggle search"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(footer, area);
}

fn draw_settings(frame: &mut Frame, settings_draft: &ModelSettings) {
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Model Settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    let search_label = if settings_draft.search_enabled {
        "Search [ON]"
    } else {
        "Search [OFF]"
    };
    let thinking_label = if settings_draft.deep_thinking {
        "Deep Thinking [ON]"
    } else {
        "Deep Thinking [OFF]"
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            search_label,
            Style::default().fg(if settings_draft.search_enabled {
                Color::Green
            } else {
                Color::Red
            }),
        ))),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            thinking_label,
            Style::default().fg(if settings_draft.deep_thinking {
                Color::Green
            } else {
                Color::Red
            }),
        ))),
        chunks[1],
    );

    let temp = settings_draft
        .temperature
        .map(|t| format!("{t:.2}"))
        .unwrap_or_else(|| "default".to_string());
    let max = settings_draft
        .max_tokens
        .map(|t| t.to_string())
        .unwrap_or_else(|| "default".to_string());
    frame.render_widget(Paragraph::new(format!("Temperature: {temp}")), chunks[2]);
    frame.render_widget(Paragraph::new(format!("Max tokens: {max}")), chunks[3]);

    frame.render_widget(
        Paragraph::new("Press 't' to toggle deep thinking, 'e' to toggle search, 's' to close")
            .alignment(Alignment::Center),
        chunks[4],
    );
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

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
