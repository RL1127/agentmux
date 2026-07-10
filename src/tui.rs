//! 使用 ratatui 和 crossterm 提供跨平台会话选择界面。

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, symbols};
use unicode_width::UnicodeWidthChar;

use crate::catalog::{SessionCatalog, SessionQuery};
use crate::domain::{InteractionType, ResumeState, ScanReport, Session};
use crate::provider::ProviderRegistry;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 表示用户从 TUI 退出、打开 App 会话或选择 CLI 恢复。
#[derive(Debug)]
pub enum TuiOutcome {
    /// 用户按 q 或 Esc 正常退出。
    Quit,
    /// 用户按 Enter 请求在来源桌面应用中打开会话。
    OpenInApp(Box<Session>),
    /// 用户按 c 请求通过来源官方 CLI 恢复会话。
    ResumeCli(Box<Session>),
}

/// 启动 TUI，后台扫描全部来源，并在退出前可靠恢复终端状态。
pub fn run(
    registry: Arc<ProviderRegistry>,
    initial_error: Option<String>,
    open_in_app: bool,
) -> Result<TuiOutcome> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(registry.scan_all());
    });

    let mut terminal = TerminalSession::start()?;
    let mut state = AppState::new(initial_error, open_in_app);
    run_loop(terminal.terminal_mut(), &receiver, &mut state)
}

/// 管理原始模式、备用屏幕和鼠标捕获，确保异常路径也恢复终端。
struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    /// 初始化 crossterm 后端和备用屏幕。
    fn start() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(value) => value,
            Err(error) => {
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }

    /// 返回可绘制的终端引用，生命周期受终端会话保护。
    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    /// 无论事件循环如何结束，都退出原始模式和备用屏幕。
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

/// 运行绘制和键盘事件循环，直到用户退出或选择会话。
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    receiver: &mpsc::Receiver<ScanReport>,
    state: &mut AppState,
) -> Result<TuiOutcome> {
    loop {
        if state.loading
            && let Ok(report) = receiver.try_recv()
        {
            state.load(report);
        }
        terminal.draw(|frame| render(frame, state))?;

        if !event::poll(EVENT_POLL_INTERVAL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if let Some(outcome) = state.handle_key(key) {
            return Ok(outcome);
        }
    }
}

/// 表示当前处于目录选择层或目录内会话层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserLevel {
    /// 第一层只显示会话工作目录。
    Directories,
    /// 第二层显示选中目录中的会话和详情。
    Sessions,
}

/// 保存同一工作目录下按更新时间倒序排列的会话。
struct DirectoryGroup {
    identity: String,
    directory: String,
    sessions: Vec<Session>,
}

/// 保存两级导航、查询、扫描和错误状态。
struct AppState {
    catalog: Option<SessionCatalog>,
    directories: Vec<DirectoryGroup>,
    visible_directories: Vec<usize>,
    visible_sessions: Vec<Session>,
    level: BrowserLevel,
    active_directory: Option<String>,
    selected_directory: usize,
    selected_session: usize,
    search: String,
    search_mode: bool,
    loading: bool,
    warning_count: usize,
    status_error: Option<String>,
    open_in_app: bool,
}

impl AppState {
    /// 创建处于扫描状态的目录浏览器，并设置 Enter 的默认打开方式。
    fn new(initial_error: Option<String>, open_in_app: bool) -> Self {
        Self {
            catalog: None,
            directories: Vec::new(),
            visible_directories: Vec::new(),
            visible_sessions: Vec::new(),
            level: BrowserLevel::Directories,
            active_directory: None,
            selected_directory: 0,
            selected_session: 0,
            search: String::new(),
            search_mode: false,
            loading: true,
            warning_count: 0,
            status_error: initial_error,
            open_in_app,
        }
    }

    /// 接收后台扫描报告，按完整工作目录建立第一层分组。
    fn load(&mut self, report: ScanReport) {
        self.loading = false;
        self.warning_count = report.warnings.len();
        self.catalog = Some(SessionCatalog::from_report(report));
        self.rebuild_directories();
        self.rebuild_visible();
    }

    /// 将交互式会话按规范化工作目录分组，组顺序由最新会话决定。
    fn rebuild_directories(&mut self) {
        self.directories.clear();
        let Some(catalog) = self.catalog.as_ref() else {
            return;
        };
        let sessions = catalog.query(&SessionQuery::default());
        let mut indices = HashMap::<String, usize>::new();
        for session in sessions {
            let directory = session_directory(session);
            let identity = directory_identity(&directory);
            let index = match indices.get(&identity) {
                Some(index) => *index,
                None => {
                    let index = self.directories.len();
                    self.directories.push(DirectoryGroup {
                        identity: identity.clone(),
                        directory,
                        sessions: Vec::new(),
                    });
                    indices.insert(identity, index);
                    index
                }
            };
            self.directories[index].sessions.push(session.clone());
        }
    }

    /// 根据当前层级和搜索词刷新目录或会话列表。
    fn rebuild_visible(&mut self) {
        let matcher = SkimMatcherV2::default().ignore_case();
        match self.level {
            BrowserLevel::Directories => {
                self.visible_directories = self
                    .directories
                    .iter()
                    .enumerate()
                    .filter(|(_, group)| {
                        self.search.is_empty()
                            || matcher
                                .fuzzy_match(&group.directory, &self.search)
                                .is_some()
                    })
                    .map(|(index, _)| index)
                    .collect();
                self.selected_directory =
                    clamp_selection(self.selected_directory, self.visible_directories.len());
                self.visible_sessions.clear();
            }
            BrowserLevel::Sessions => {
                self.visible_sessions = self
                    .active_group()
                    .map(|group| {
                        group
                            .sessions
                            .iter()
                            .filter(|session| {
                                self.search.is_empty()
                                    || matcher
                                        .fuzzy_match(&session.searchable_text(), &self.search)
                                        .is_some()
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                self.selected_session =
                    clamp_selection(self.selected_session, self.visible_sessions.len());
            }
        }
    }

    /// 处理两级导航、搜索、退出和恢复按键。
    fn handle_key(&mut self, key: KeyEvent) -> Option<TuiOutcome> {
        if self.search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.search_mode = false,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.rebuild_visible();
                }
                KeyCode::Char(character) => {
                    self.search.push(character);
                    self.rebuild_visible();
                }
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Char('q') => Some(TuiOutcome::Quit),
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                if self.level == BrowserLevel::Sessions {
                    self.back_to_directories();
                    None
                } else {
                    Some(TuiOutcome::Quit)
                }
            }
            KeyCode::Char('/') => {
                self.search_mode = true;
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Char('c') if self.level == BrowserLevel::Sessions => {
                self.selected_cli_resume()
            }
            KeyCode::Enter => self.activate_selection(),
            _ => None,
        }
    }

    /// 在当前层级的可见条目中循环移动选择位置。
    fn move_selection(&mut self, delta: isize) {
        let (selected, length) = match self.level {
            BrowserLevel::Directories => {
                (&mut self.selected_directory, self.visible_directories.len())
            }
            BrowserLevel::Sessions => (&mut self.selected_session, self.visible_sessions.len()),
        };
        if length == 0 {
            return;
        }
        *selected = (*selected as isize + delta).rem_euclid(length as isize) as usize;
    }

    /// 进入选中目录，或按当前首选方式打开选中会话。
    fn activate_selection(&mut self) -> Option<TuiOutcome> {
        match self.level {
            BrowserLevel::Directories => {
                let directory_index = *self.visible_directories.get(self.selected_directory)?;
                self.active_directory = Some(self.directories[directory_index].identity.clone());
                self.level = BrowserLevel::Sessions;
                self.selected_session = 0;
                self.search.clear();
                self.rebuild_visible();
                None
            }
            BrowserLevel::Sessions => {
                let session = self.visible_sessions.get(self.selected_session)?.clone();
                Some(if self.open_in_app {
                    TuiOutcome::OpenInApp(Box::new(session))
                } else {
                    TuiOutcome::ResumeCli(Box::new(session))
                })
            }
        }
    }

    /// 返回通过来源官方 CLI 恢复当前选中会话的结果。
    fn selected_cli_resume(&self) -> Option<TuiOutcome> {
        self.visible_sessions
            .get(self.selected_session)
            .cloned()
            .map(|session| TuiOutcome::ResumeCli(Box::new(session)))
    }

    /// 从会话层返回目录层，并清除当前目录内搜索。
    fn back_to_directories(&mut self) {
        self.level = BrowserLevel::Directories;
        self.active_directory = None;
        self.selected_session = 0;
        self.search.clear();
        self.search_mode = false;
        self.rebuild_visible();
    }

    /// 返回当前活动目录分组。
    fn active_group(&self) -> Option<&DirectoryGroup> {
        let identity = self.active_directory.as_deref()?;
        self.directories
            .iter()
            .find(|group| group.identity == identity)
    }

    /// 返回会话层当前选中的会话。
    fn selected_session(&self) -> Option<&Session> {
        self.visible_sessions.get(self.selected_session)
    }
}

/// 将越界选择收敛到可见列表，空列表统一返回零。
fn clamp_selection(selected: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        selected.min(length - 1)
    }
}

/// 返回会话的完整工作目录；缺失目录使用明确占位分组。
fn session_directory(session: &Session) -> String {
    session
        .cwd
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| format!("未指定目录 ({})", session.project))
}

/// 规范化目录分组标识，Windows 下忽略大小写和斜杠差异。
fn directory_identity(directory: &str) -> String {
    let trimmed = directory.trim_end_matches(&['\\', '/'][..]);
    #[cfg(windows)]
    {
        trimmed.replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        trimmed.to_owned()
    }
}

/// 根据终端尺寸选择布局并绘制完整界面。
fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let area = frame.area();
    let header_height = if state.search_mode || !state.search.is_empty() {
        2
    } else {
        1
    };
    let status_height = if state.status_error.is_some() { 2 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(5),
            Constraint::Length(status_height),
        ])
        .split(area);

    render_header(frame, chunks[0], state);
    if state.loading {
        render_loading(frame, chunks[1]);
    } else {
        render_content(frame, chunks[1], state);
    }
    render_status(frame, chunks[2], state);
}

/// 绘制当前层级、条目数量、警告数和搜索输入状态。
fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut title_spans = vec![
        Span::styled(
            "agentmux",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    match state.level {
        BrowserLevel::Directories => title_spans.push(Span::raw(format!(
            "目录:{}  警告:{}",
            state.visible_directories.len(),
            state.warning_count
        ))),
        BrowserLevel::Sessions => {
            let directory = state
                .active_group()
                .map(|group| group.directory.as_str())
                .unwrap_or("未知目录");
            title_spans.push(Span::raw(format!(
                "目录:{}  会话:{}  警告:{}",
                truncate_width(directory, area.width.saturating_sub(24) as usize),
                state.visible_sessions.len(),
                state.warning_count
            )));
        }
    }
    let title = Line::from(title_spans);
    let mut lines = vec![title];
    if state.search_mode || !state.search.is_empty() {
        let marker = match (state.level, state.search_mode) {
            (BrowserLevel::Directories, true) => "搜索目录> ",
            (BrowserLevel::Sessions, true) => "搜索会话> ",
            (BrowserLevel::Directories, false) => "目录搜索: ",
            (BrowserLevel::Sessions, false) => "会话搜索: ",
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Yellow)),
            Span::raw(truncate_width(
                &state.search,
                area.width.saturating_sub(display_width(marker) as u16) as usize,
            )),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);

    if state.search_mode && area.height > 1 {
        let marker = if state.level == BrowserLevel::Directories {
            "搜索目录> "
        } else {
            "搜索会话> "
        };
        let cursor_x = display_width(marker)
            .saturating_add(display_width(&state.search))
            .min(area.width.saturating_sub(1) as usize);
        frame.set_cursor_position(Position::new(area.x + cursor_x as u16, area.y + 1));
    }
}

/// 绘制后台会话扫描状态。
fn render_loading(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("正在只读扫描本地 Agent 会话…")
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(symbols::border::PLAIN)
                .title(" 扫描中 "),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

/// 第一层绘制全宽目录列表，第二层绘制会话表格和详情。
fn render_content(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if state.level == BrowserLevel::Directories {
        render_directory_list(frame, area, state);
        return;
    }
    let direction = if area.width >= 96 && area.height >= 14 {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let constraints = [Constraint::Percentage(58), Constraint::Percentage(42)];
    let chunks = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    render_session_table(frame, chunks[0], state);
    render_detail(frame, chunks[1], state.selected_session());
}

/// 绘制第一层目录列表，不混入会话标题或详情。
fn render_directory_list(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if state.visible_directories.is_empty() {
        let message = if state.search.is_empty() {
            "未找到包含交互式会话的目录。"
        } else {
            "没有目录匹配当前搜索。"
        };
        frame.render_widget(
            Paragraph::new(message).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(symbols::border::PLAIN)
                    .title(" 目录 "),
            ),
            area,
        );
        return;
    }

    let rows = state.visible_directories.iter().filter_map(|index| {
        state.directories.get(*index).map(|group| {
            Row::new([Cell::from(truncate_width(
                &group.directory,
                area.width.saturating_sub(5) as usize,
            ))])
        })
    });
    let header = Row::new(["目录"]).style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(rows, [Constraint::Min(8)])
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(symbols::border::PLAIN)
                .title(" 目录 "),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut table_state = TableState::default();
    table_state.select(Some(state.selected_directory));
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// 绘制当前目录内的更新时间和会话标题。
fn render_session_table(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if state.visible_sessions.is_empty() {
        let message = if state.search.is_empty() {
            "当前目录没有可恢复的交互式会话。"
        } else {
            "没有会话匹配当前搜索。"
        };
        frame.render_widget(
            Paragraph::new(message).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(symbols::border::PLAIN)
                    .title(" 会话 "),
            ),
            area,
        );
        return;
    }

    let rows = state.visible_sessions.iter().map(|session| {
        Row::new(vec![
            Cell::from(session.updated_at.format("%m-%d %H:%M").to_string()),
            Cell::from(truncate_width(
                session.display_title(),
                area.width.saturating_sub(16) as usize,
            )),
        ])
    });
    let header = Row::new(["更新时间", "标题"]).style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(rows, [Constraint::Length(11), Constraint::Min(8)])
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(symbols::border::PLAIN)
                .title(" 会话 "),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut table_state = TableState::default();
    table_state.select(Some(state.selected_session));
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// 绘制选中会话的安全摘要、目录、模型和恢复状态。
fn render_detail(frame: &mut Frame<'_>, area: Rect, session: Option<&Session>) {
    let Some(session) = session else {
        frame.render_widget(
            Paragraph::new("选择会话后显示详情。").block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(symbols::border::PLAIN)
                    .title(" 详情 "),
            ),
            area,
        );
        return;
    };
    let cwd = session
        .cwd
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "-".to_owned());
    let model = session.model.as_deref().unwrap_or("-");
    let provider = session.model_provider.as_deref().unwrap_or("-");
    let resume_message = session.resume.message.as_deref().unwrap_or("");
    let mut lines = vec![
        detail_line("标题", session.display_title()),
        detail_line("会话 ID", &session.id),
        detail_line("来源", session.source.as_str()),
        detail_line("项目", &session.project),
        detail_line("目录", &cwd),
        detail_line("模型", model),
        detail_line("Provider", provider),
        detail_line("交互", interaction_label(&session.interaction)),
        detail_line("创建", &session.created_at.to_rfc3339()),
        detail_line("更新", &session.updated_at.to_rfc3339()),
        detail_line("恢复", resume_label(&session.resume.state)),
    ];
    if !resume_message.is_empty() {
        lines.push(detail_line("说明", resume_message));
    }
    if !session.warnings.is_empty() {
        lines.push(detail_line("解析警告", &session.warnings.len().to_string()));
    }
    if let Some(summary) = session.summary.as_deref() {
        lines.push(Line::default());
        lines.push(detail_line("摘要", summary));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(symbols::border::PLAIN)
                    .title(" 详情 "),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// 绘制快捷键提示和可选恢复失败状态。
fn render_status(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let shortcuts = match (state.level, area.width < 72) {
        (BrowserLevel::Directories, true) => "↑↓/jk 移动  / 搜索  Enter 进入  q/Esc 退出",
        (BrowserLevel::Directories, false) => {
            "方向键或 j/k 移动  / 搜索目录  Enter 进入目录  q/Esc 退出"
        }
        (BrowserLevel::Sessions, true) if state.open_in_app => {
            "↑↓/jk 移动  Enter App  c CLI  Esc/← 返回  q 退出"
        }
        (BrowserLevel::Sessions, true) => "↑↓/jk 移动  / 搜索  Enter/c CLI  Esc/← 返回  q 退出",
        (BrowserLevel::Sessions, false) if state.open_in_app => {
            "方向键或 j/k 移动  / 搜索会话  Enter 在 App 打开  c CLI 恢复  Esc/← 返回目录  q 退出"
        }
        (BrowserLevel::Sessions, false) => {
            "方向键或 j/k 移动  / 搜索会话  Enter/c CLI 恢复  Esc/← 返回目录  q 退出"
        }
    };
    let mut lines = Vec::new();
    if let Some(error) = state.status_error.as_deref() {
        lines.push(Line::from(Span::styled(
            truncate_width(error, area.width as usize),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        truncate_width(shortcuts, area.width as usize),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

/// 创建详情面板中的加粗标签行。
fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_owned()),
    ])
}

/// 返回交互类型的中文标签。
fn interaction_label(interaction: &InteractionType) -> &'static str {
    match interaction {
        InteractionType::Interactive => "交互式",
        InteractionType::NonInteractive => "非交互",
        InteractionType::Unknown => "未知",
    }
}

/// 返回恢复状态的中文标签。
fn resume_label(state: &ResumeState) -> &'static str {
    match state {
        ResumeState::Ready => "可恢复",
        ResumeState::CliMissing => "CLI 缺失",
        ResumeState::SessionMissing => "会话缺失",
        ResumeState::ProviderMissing => "Provider 缺失",
        ResumeState::Unsupported => "不支持",
        ResumeState::Blocked => "已阻止",
    }
}

/// 计算文本终端显示宽度，中文宽字符按两列处理。
fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

/// 按终端显示列截断 Unicode 文本，避免中文字符和后续列重叠。
fn truncate_width(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let target = max_width.saturating_sub(1);
    let mut width = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > target {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    use crate::domain::{InteractionType, ResumeStatus, SourceId};

    use super::*;

    /// 创建包含中文路径和摘要的 TUI 测试会话。
    fn test_session(id: &str, hour: u32, directory: &str) -> Session {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 7, 10, hour, 0, 0)
            .single()
            .expect("测试时间应有效");
        Session {
            id: id.to_owned(),
            source: SourceId::new("codex"),
            title: Some("实现跨平台会话恢复".to_owned()),
            summary: Some("在窄终端中安全显示中文路径和摘要。".to_owned()),
            cwd: Some(PathBuf::from(directory)),
            project: PathBuf::from(directory)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
            model: Some("gpt-5".to_owned()),
            model_provider: Some("custom".to_owned()),
            interaction: InteractionType::Interactive,
            raw_path: PathBuf::from(format!("{id}.jsonl")),
            warnings: Vec::new(),
            resume: ResumeStatus::ready(),
        }
    }

    /// 创建已完成扫描的界面状态。
    fn loaded_state() -> AppState {
        let mut state = AppState::new(Some("上次恢复失败，退出码 2".to_owned()), true);
        state.load(ScanReport {
            sessions: vec![
                test_session("newer", 14, r"D:\项目\智能助手"),
                test_session("older", 12, r"D:\项目\智能助手"),
                test_session("another", 13, r"D:\项目\另一个项目"),
            ],
            warnings: Vec::new(),
        });
        state
    }

    /// 验证先选择目录，再浏览会话，并可返回目录层。
    #[test]
    fn navigates_directories_before_sessions() {
        let mut state = loaded_state();
        assert_eq!(state.level, BrowserLevel::Directories);
        assert_eq!(state.visible_directories.len(), 2);

        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.level, BrowserLevel::Sessions);
        assert_eq!(state.visible_sessions.len(), 2);

        state.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(state.selected_session, 1);
        let outcome = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome, Some(TuiOutcome::OpenInApp(_))));

        let outcome = state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(outcome, Some(TuiOutcome::ResumeCli(_))));

        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state.level, BrowserLevel::Directories);
        assert!(state.active_directory.is_none());
    }

    /// 验证目录层和会话层分别使用各自的搜索范围。
    #[test]
    fn searches_directories_and_current_directory_sessions() {
        let mut state = loaded_state();
        state.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('另'), KeyModifiers::NONE));
        assert!(state.search_mode);
        assert_eq!(state.visible_directories.len(), 1);
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.level, BrowserLevel::Sessions);
        assert_eq!(state.visible_sessions.len(), 1);

        state.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('不'), KeyModifiers::NONE));
        assert!(state.visible_sessions.is_empty());
    }

    /// 验证第一层窄终端只渲染目录，不渲染会话标题。
    #[test]
    fn first_level_renders_only_directories() {
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).expect("应能创建测试终端");
        let mut state = loaded_state();

        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("窄终端渲染应成功");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = content.replace(' ', "");
        assert!(compact.contains("智能助手"));
        assert!(compact.contains("另一个项目"));
        assert!(!compact.contains("实现跨平台会话恢复"));
        assert_eq!(terminal.size().expect("应能读取终端尺寸").width, 48);
    }

    /// 验证第二层在窄终端中渲染会话标题和详情时不会越界。
    #[test]
    fn renders_session_level_in_narrow_terminal() {
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).expect("应能创建测试终端");
        let mut state = loaded_state();
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("会话层窄终端渲染应成功");

        assert_eq!(state.level, BrowserLevel::Sessions);
        assert_eq!(terminal.size().expect("应能读取终端尺寸").width, 48);
    }

    /// 验证显示宽度截断不会切断中文字符。
    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate_width("中文路径", 5), "中文…");
        assert_eq!(display_width("中文"), 4);
    }

    /// 验证关闭 App 导航后 Enter 会回退为官方 CLI 恢复。
    #[test]
    fn enter_resumes_cli_when_app_navigation_is_disabled() {
        let mut state = AppState::new(None, false);
        state.load(ScanReport {
            sessions: vec![test_session("session", 14, r"D:\项目\智能助手")],
            warnings: Vec::new(),
        });
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let outcome = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome, Some(TuiOutcome::ResumeCli(_))));
    }
}
