//! 负责命令分发，并将 CLI/TUI 与来源注册表连接起来。

use std::io::{self, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;

use crate::catalog::{SessionCatalog, SessionQuery, group_sessions};
use crate::cli::{AgentmuxCommand, Cli, ListArgs, parse_since};
use crate::output::write_list;
use crate::provider::ProviderRegistry;
use crate::provider::codex::CodexProvider;
use crate::tui::{self, TuiOutcome};

/// 从当前进程参数运行 agentmux，并返回应传递给操作系统的退出码。
pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    if cli.command.is_none() {
        return run_interactive();
    }
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_cli(cli, stdout.lock(), stderr.lock())
}

/// 启动默认交互界面，并返回退出或选择结果。
fn run_interactive() -> Result<i32> {
    let registry = Arc::new(default_registry()?);
    match tui::run(registry, None)? {
        TuiOutcome::Quit => Ok(0),
        TuiOutcome::Resume(session) => {
            bail!("已选择会话 {}，恢复执行层尚未接入", session.id)
        }
    }
}

/// 执行已解析命令；显式 writer 便于测试且确保输出编码为 UTF-8 字节。
pub fn run_cli(cli: Cli, mut stdout: impl Write, mut stderr: impl Write) -> Result<i32> {
    match cli.command {
        Some(AgentmuxCommand::List(args)) => {
            run_list(args, &mut stdout, &mut stderr)?;
            Ok(0)
        }
        Some(AgentmuxCommand::Resume(_)) => {
            bail!("resume 命令尚未接入执行层")
        }
        Some(AgentmuxCommand::Doctor(_)) => {
            bail!("doctor 命令尚未接入诊断输出")
        }
        Some(AgentmuxCommand::Sources(_)) => {
            bail!("sources 命令尚未接入来源输出")
        }
        Some(AgentmuxCommand::Completion { .. }) => {
            bail!("completion 命令尚未接入补全生成器")
        }
        None => bail!("当前构建尚未接入交互式界面，请先使用 agentmux list"),
    }
}

/// 创建默认来源注册表；新增来源只需在此注册一次实现。
pub fn default_registry() -> Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    registry.register(CodexProvider::discover().context("初始化 Codex provider 失败")?);
    Ok(registry)
}

/// 扫描来源并执行 list 过滤、分组和输出。
fn run_list(args: ListArgs, stdout: &mut impl Write, stderr: &mut impl Write) -> Result<()> {
    let registry = default_registry()?;
    let catalog = SessionCatalog::from_report(registry.scan_all());
    let since = args
        .since
        .as_deref()
        .map(|value| parse_since(value, Utc::now()))
        .transpose()?;
    let query = SessionQuery {
        source: args.source,
        project: args.project,
        provider: args.provider,
        since,
        include_non_interactive: args.include_non_interactive,
        search: args.search,
    };
    let sessions = catalog.query(&query);
    let groups = group_sessions(sessions, args.group_by);
    write_list(
        stdout,
        &groups,
        catalog.warnings(),
        args.group_by,
        args.json,
    )?;

    if !args.json {
        for warning in catalog.warnings() {
            let location = match (&warning.path, warning.line) {
                (Some(path), Some(line)) => format!("{}:{line}", path.display()),
                (Some(path), None) => path.display().to_string(),
                (None, _) => "来源".to_owned(),
            };
            writeln!(stderr, "警告 [{location}] {}", warning.message)?;
        }
    }
    Ok(())
}
