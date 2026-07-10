//! 使用 ratatui 和 crossterm 提供跨平台会话选择界面。

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
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, symbols};
use unicode_width::UnicodeWidthChar;

use crate::catalog::{GroupBy, SessionCatalog, SessionQuery, group_sessions};
use crate::domain::{InteractionType, ResumeState, ScanReport, Session};
use crate::provider::ProviderRegistry;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 表示用户从 TUI 退出或选择了待恢复会话。
#[derive(Debug)]
pub enum TuiOutcome {
    /// 用户按 q 或 Esc 正常退出。
    Quit,
    /// 用户按 Enter 选择会话。
    Resume(Box<Session>),
}

/// 启动 TUI，后台扫描全部来源，并在退出前可靠恢复终端状态。
pub fn run(registry: Arc<ProviderRegistry>, initial_error: Option<String>) -> Result<TuiOutcome> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(registry.scan_all());
    });

    let mut terminal = TerminalSession::start()?;
    let mut state = AppState::new(initial_error);
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

/// 保存界面查询、选择、扫描和错误状态。
struct AppState {
    catalog: Option<SessionCatalog>,
    visible: Vec<VisibleSession>,
    group_by: GroupBy,
    selected: usize,
    search: String,
    search_mode: bool,
    loading: bool,
    warning_count: usize,
    status_error: Option<String>,
}

impl AppState {
    /// 创建处于扫描状态的界面，可选展示上一次恢复失败信息。
    fn new(initial_error: Option<String>) -> Self {
        Self {
            catalog: None,
            visible: Vec::new(),
            group_by: GroupBy::Project,
            selected: 0,
            search: String::new(),
            search_mode: false,
            loading: true,
            warning_count: 0,
            status_error: initial_error,
        }
    }

    /// 接收后台扫描报告并重建可见会话列表。
    fn load(&mut self, report: ScanReport) {
        self.loading = false;
        self.warning_count = report.warnings.len();
        self.catalog = Some(SessionCatalog::from_report(report));
        self.rebuild_visible();
    }

    /// 按当前搜索与分组条件生成表格行，默认排除非交互会话。
    fn rebuild_visible(&mut self) {
        let Some(catalog) = self.catalog.as_ref() else {
            self.visible.clear();
            self.selected = 0;
            return;
        };
        let query = SessionQuery {
            search: (!self.search.is_empty()).then(|| self.search.clone()),
            include_non_interactive: false,
            ..SessionQuery::default()
        };
        let groups = group_sessions(catalog.query(&query), self.group_by);
        self.visible = groups
            .into_iter()
            .flat_map(|group| {
                group
                    .sessions
                    .into_iter()
                    .enumerate()
                    .map(move |(index, session)| VisibleSession {
                        group: (index == 0).then(|| group.key.clone()),
                        session: session.clone(),
                    })
            })
            .collect();
        if self.visible.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.visible.len() - 1);
        }
    }

    /// 处理导航、搜索、分组、退出和选择按键，并按需返回最终动作。
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
            KeyCode::Char('q') | KeyCode::Esc => Some(TuiOutcome::Quit),
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
            KeyCode::Tab => {
                self.group_by = self.group_by.next();
                self.rebuild_visible();
                None
            }
            KeyCode::Enter => self
                .visible
                .get(self.selected)
                .map(|item| TuiOutcome::Resume(Box::new(item.session.clone()))),
            _ => None,
        }
    }

    /// 在可见会话范围内循环移动选择位置。
    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let length = self.visible.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(length) as usize;
    }

    /// 返回当前选中会话。
    fn selected_session(&self) -> Option<&Session> {
        self.visible.get(self.selected).map(|item| &item.session)
    }
}

/// 保存表格所需的可选分组标题和会话副本。
struct VisibleSession {
    group: Option<String>,
    session: Session,
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

/// 绘制分组、会话数、警告数和搜索输入状态。
fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title = Line::from(vec![
        Span::styled(
            "agentmux",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  分组:{}  会话:{}  警告:{}",
            group_label(state.group_by),
            state.visible.len(),
            state.warning_count
        )),
    ]);
    let mut lines = vec![title];
    if state.search_mode || !state.search.is_empty() {
        let marker = if state.search_mode {
            "搜索> "
        } else {
            "搜索: "
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Yellow)),
            Span::raw(truncate_width(
                &state.search,
                area.width.saturating_sub(8) as usize,
            )),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);

    if state.search_mode && area.height > 1 {
        let cursor_x = 6usize
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

/// 根据宽度使用左右或上下布局绘制会话表格和详情。
fn render_content(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
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
    render_table(frame, chunks[0], state);
    render_detail(frame, chunks[1], state.selected_session());
}

/// 绘制分组、更新时间和标题表格，并保持选择高亮。
fn render_table(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if state.visible.is_empty() {
        let message = if state.search.is_empty() {
            "未找到可恢复的交互式会话。"
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

    let group_width = area.width.saturating_sub(24).clamp(8, 18);
    let rows = state.visible.iter().map(|item| {
        Row::new(vec![
            Cell::from(truncate_width(
                item.group.as_deref().unwrap_or(""),
                group_width as usize,
            )),
            Cell::from(item.session.updated_at.format("%m-%d %H:%M").to_string()),
            Cell::from(truncate_width(
                item.session.display_title(),
                area.width.saturating_sub(group_width + 18) as usize,
            )),
        ])
    });
    let header = Row::new(["分组", "更新时间", "标题"]).style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(group_width),
            Constraint::Length(11),
            Constraint::Min(8),
        ],
    )
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
    table_state.select(Some(state.selected));
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
    let shortcuts = if area.width < 72 {
        "↑↓/jk 移动  / 搜索  Tab 分组  Enter 恢复  q 退出"
    } else {
        "方向键或 j/k 移动  / 搜索  Tab 切换分组  Enter 恢复  q/Esc 退出"
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

/// 返回分组维度的中文短标签。
fn group_label(group_by: GroupBy) -> &'static str {
    match group_by {
        GroupBy::Project => "项目",
        GroupBy::Date => "日期",
        GroupBy::Source => "来源",
        GroupBy::Provider => "Provider",
    }
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
    fn test_session(id: &str, hour: u32) -> Session {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 7, 10, hour, 0, 0)
            .single()
            .expect("测试时间应有效");
        Session {
            id: id.to_owned(),
            source: SourceId::new("codex"),
            title: Some("实现跨平台会话恢复".to_owned()),
            summary: Some("在窄终端中安全显示中文路径和摘要。".to_owned()),
            cwd: Some(PathBuf::from(r"D:\项目\智能助手")),
            project: "智能助手".to_owned(),
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
        let mut state = AppState::new(Some("上次恢复失败，退出码 2".to_owned()));
        state.load(ScanReport {
            sessions: vec![test_session("newer", 14), test_session("older", 12)],
            warnings: Vec::new(),
        });
        state
    }

    /// 验证导航循环、搜索输入和分组切换状态。
    #[test]
    fn handles_navigation_search_and_grouping() {
        let mut state = loaded_state();
        state.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(state.selected, 1);
        state.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(state.selected, 0);
        state.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char('智'), KeyModifiers::NONE));
        assert!(state.search_mode);
        assert_eq!(state.visible.len(), 2);
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(state.group_by, GroupBy::Date);
    }

    /// 验证窄终端和中文内容渲染不会越界或 panic。
    #[test]
    fn renders_narrow_terminal_with_chinese_text() {
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).expect("应能创建测试终端");
        let mut state = loaded_state();

        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("窄终端渲染应成功");

        assert_eq!(terminal.size().expect("应能读取终端尺寸").width, 48);
    }

    /// 验证显示宽度截断不会切断中文字符。
    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate_width("中文路径", 5), "中文…");
        assert_eq!(display_width("中文"), 4);
    }
}
