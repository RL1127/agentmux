//! 负责命令分发，并将 CLI/TUI 与来源注册表连接起来。

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::catalog::{SessionCatalog, SessionQuery, group_sessions};
use crate::cli::{AgentmuxCommand, Cli, ListArgs, ResumeArgs, parse_since};
use crate::domain::{CommandSpec, DiagnosticSeverity, RepairOptions, Session};
use crate::output::{write_diagnostics, write_list, write_sources};
use crate::provider::ProviderRegistry;
use crate::provider::codex::CodexProvider;
use crate::resume;
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
    let mut last_error = None;
    loop {
        match tui::run(Arc::clone(&registry), last_error.take())? {
            TuiOutcome::Quit => return Ok(0),
            TuiOutcome::Resume(session) => match execute_session(&registry, &session, false) {
                Ok(execution) if execution.exit_code == 0 => return Ok(0),
                Ok(execution) => {
                    let message =
                        format!("恢复失败: {} 退出码 {}", session.id, execution.exit_code);
                    if wait_after_resume_failure(&message)? {
                        return Ok(execution.exit_code);
                    }
                    last_error = Some(message);
                }
                Err(error) => {
                    let message = format!("恢复失败: {error:#}");
                    if wait_after_resume_failure(&message)? {
                        return Ok(1);
                    }
                    last_error = Some(message);
                }
            },
        }
    }
}

/// 在 Codex 恢复失败后保留普通终端输出，等待用户决定返回列表或退出。
fn wait_after_resume_failure(message: &str) -> Result<bool> {
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "\n{message}")?;
    writeln!(stderr, "Codex 的原始错误输出保留在上方。")?;
    write!(stderr, "按 Enter 返回 agentmux，输入 q 后按 Enter 退出: ")?;
    stderr.flush()?;

    let mut answer = String::new();
    let bytes_read = io::stdin().read_line(&mut answer)?;
    Ok(bytes_read == 0 || resume_failure_requests_quit(&answer))
}

/// 判断恢复失败提示中的用户输入是否表示退出应用。
fn resume_failure_requests_quit(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "q" | "quit")
}

/// 执行已解析命令；显式 writer 便于测试且确保输出编码为 UTF-8 字节。
pub fn run_cli(cli: Cli, mut stdout: impl Write, mut stderr: impl Write) -> Result<i32> {
    match cli.command {
        Some(AgentmuxCommand::List(args)) => {
            run_list(args, &mut stdout, &mut stderr)?;
            Ok(0)
        }
        Some(AgentmuxCommand::Resume(args)) => run_resume(args, &mut stdout, &mut stderr),
        Some(AgentmuxCommand::Doctor(args)) => run_doctor(args.json, &mut stdout),
        Some(AgentmuxCommand::Sources(args)) => run_sources(args.json, &mut stdout),
        Some(AgentmuxCommand::Completion { shell }) => {
            run_completion(shell, &mut stdout);
            Ok(0)
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

/// 执行显式 resume 子命令，并把 Codex 退出码原样返回给 main。
fn run_resume(args: ResumeArgs, stdout: &mut impl Write, stderr: &mut impl Write) -> Result<i32> {
    let registry = default_registry()?;
    let catalog = SessionCatalog::from_report(registry.scan_all());
    let session = catalog
        .find(&args.session_id, None)
        .cloned()
        .with_context(|| format!("未找到会话 {}", args.session_id))?;
    if args.repair_provider {
        if args.dry_run {
            bail!("--repair-provider 不能与 --dry-run 同时使用");
        }
        let confirmed = confirm_provider_repair(args.yes, &session, stdout)?;
        let provider = registry
            .get(&session.source)
            .with_context(|| format!("来源 {} 未注册", session.source))?;
        let report = provider.repair_model_provider(&session, RepairOptions { confirmed })?;
        writeln!(stdout, "{}", report.message)?;
        if let Some(backup_path) = report.backup_path {
            writeln!(stdout, "配置备份: {}", backup_path.display())?;
        }
    }
    let execution = execute_session(&registry, &session, args.dry_run)?;
    if args.dry_run {
        writeln!(stdout, "将执行: {}", execution.command)?;
    } else if execution.exit_code != 0 {
        writeln!(
            stderr,
            "恢复命令失败，会话 {}，退出码 {}",
            session.id, execution.exit_code
        )?;
    }
    Ok(execution.exit_code)
}

/// 在未传入 yes 时从真实终端确认 provider 配置修改。
fn confirm_provider_repair(
    assume_yes: bool,
    session: &Session,
    stdout: &mut impl Write,
) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!("非交互环境执行 --repair-provider 时必须同时传入 --yes");
    }
    let provider = session.model_provider.as_deref().unwrap_or("unknown");
    write!(
        stdout,
        "将备份 config.toml 并创建 provider 兼容别名 {provider}，继续吗？[y/N] "
    )?;
    stdout.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(true)
    } else {
        bail!("已取消 provider 配置修复")
    }
}

/// 检查会话恢复状态、构造来源官方命令并交给终端执行器。
fn execute_session(
    registry: &ProviderRegistry,
    session: &Session,
    dry_run: bool,
) -> Result<resume::ResumeExecution> {
    let command = prepare_resume_command(registry, session)?;
    Ok(resume::execute(&command, dry_run)?)
}

/// 通过会话来源查找 provider，完成恢复检查并构造官方命令。
fn prepare_resume_command(registry: &ProviderRegistry, session: &Session) -> Result<CommandSpec> {
    let provider = registry
        .get(&session.source)
        .with_context(|| format!("来源 {} 未注册", session.source))?;
    let status = provider.check_resume(session)?;
    if !status.is_ready() {
        bail!(
            "{}",
            status
                .message
                .unwrap_or_else(|| "当前会话不可恢复".to_owned())
        );
    }
    Ok(provider.build_resume_command(session)?)
}

/// 汇总全部来源诊断并在存在 error 项时返回非零状态。
fn run_doctor(json: bool, stdout: &mut impl Write) -> Result<i32> {
    let registry = default_registry()?;
    let diagnostics = registry
        .providers()
        .flat_map(|provider| provider.diagnose())
        .collect::<Vec<_>>();
    let has_error = diagnostics
        .iter()
        .any(|item| item.severity == DiagnosticSeverity::Error);
    write_diagnostics(stdout, &diagnostics, json)?;
    Ok(i32::from(has_error))
}

/// 从注册表输出所有来源描述和能力。
fn run_sources(json: bool, stdout: &mut impl Write) -> Result<i32> {
    let registry = default_registry()?;
    let sources = registry
        .providers()
        .map(|provider| provider.descriptor())
        .collect::<Vec<_>>();
    write_sources(stdout, &sources, json)?;
    Ok(0)
}

/// 使用 clap 命令模型生成目标 shell 补全脚本。
fn run_completion(shell: Shell, stdout: &mut impl Write) {
    let mut command = Cli::command();
    clap_complete::generate(shell, &mut command, "agentmux", stdout);
}

#[cfg(test)]
mod tests {
    use super::resume_failure_requests_quit;

    /// 验证恢复失败提示只把明确的退出输入识别为退出。
    #[test]
    fn parses_resume_failure_choice() {
        assert!(resume_failure_requests_quit("q\r\n"));
        assert!(resume_failure_requests_quit("QUIT"));
        assert!(!resume_failure_requests_quit("\r\n"));
        assert!(!resume_failure_requests_quit("return"));
    }
}
