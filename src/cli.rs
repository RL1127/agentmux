//! 定义 agentmux 命令行参数和时间过滤解析。

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;
use thiserror::Error;

use crate::catalog::GroupBy;

/// agentmux 顶层命令；不带子命令时进入交互式会话界面。
#[derive(Debug, Parser)]
#[command(
    name = "agentmux",
    version,
    about = "统一检索、诊断和恢复 AI Coding Agent 会话"
)]
pub struct Cli {
    /// 要执行的非交互子命令；省略时启动 TUI。
    #[command(subcommand)]
    pub command: Option<AgentmuxCommand>,
}

/// 列出 agentmux 支持的全部子命令。
#[derive(Debug, Subcommand)]
pub enum AgentmuxCommand {
    /// 非交互列出本地会话。
    List(ListArgs),
    /// 通过来源官方命令恢复指定会话。
    Resume(ResumeArgs),
    /// 检查 Agent CLI、配置和会话目录。
    Doctor(DoctorArgs),
    /// 列出当前注册的 Agent 来源。
    Sources(SourcesArgs),
    /// 生成指定 shell 的命令补全。
    Completion {
        /// 目标 shell 类型。
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// 控制 list 命令的过滤、分组和输出格式。
#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// 仅显示指定 Agent 来源。
    #[arg(long)]
    pub source: Option<String>,
    /// 仅显示指定项目。
    #[arg(long)]
    pub project: Option<String>,
    /// 仅显示指定模型提供商。
    #[arg(long)]
    pub provider: Option<String>,
    /// 仅显示指定时间之后的会话，支持 RFC3339、YYYY-MM-DD、30m、24h、7d、4w。
    #[arg(long)]
    pub since: Option<String>,
    /// 选择 project、date、source 或 provider 分组。
    #[arg(long, value_enum, default_value_t = GroupBy::Project)]
    pub group_by: GroupBy,
    /// 输出稳定 JSON 结构。
    #[arg(long)]
    pub json: bool,
    /// 包含 Codex exec、subagent 等非交互会话。
    #[arg(long)]
    pub include_non_interactive: bool,
    /// 对标题、摘要、路径和会话 ID 执行模糊搜索。
    #[arg(long)]
    pub search: Option<String>,
}

/// 控制恢复前诊断、显式修复和命令预览。
#[derive(Debug, Clone, Args)]
pub struct ResumeArgs {
    /// 来源记录的会话 ID。
    pub session_id: String,
    /// 只显示官方恢复命令，不启动 Codex。
    #[arg(long)]
    pub dry_run: bool,
    /// 显式创建历史模型提供商兼容别名。
    #[arg(long, conflicts_with = "dry_run")]
    pub repair_provider: bool,
    /// 跳过 provider 修复确认提示。
    #[arg(long, requires = "repair_provider")]
    pub yes: bool,
}

/// 控制 doctor 命令输出格式。
#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// 以 JSON 输出诊断项。
    #[arg(long)]
    pub json: bool,
}

/// 控制 sources 命令输出格式。
#[derive(Debug, Clone, Args)]
pub struct SourcesArgs {
    /// 以 JSON 输出来源描述。
    #[arg(long)]
    pub json: bool,
}

/// 表示 since 参数无法解析或超出时间范围。
#[derive(Debug, Error, PartialEq, Eq)]
#[error("无效的 --since 值: {value}")]
pub struct SinceParseError {
    /// 用户传入的原始时间表达式。
    value: String,
}

/// 解析 RFC3339、UTC 日期或相对时长；相对值以传入的 now 为基准。
pub fn parse_since(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, SinceParseError> {
    let value = value.trim();
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        && let Some(timestamp) = date.and_hms_opt(0, 0, 0)
    {
        return Ok(Utc.from_utc_datetime(&timestamp));
    }

    let Some(unit) = value.chars().last() else {
        return Err(SinceParseError {
            value: value.to_owned(),
        });
    };
    let amount_end = value.len().saturating_sub(unit.len_utf8());
    let amount = value[..amount_end]
        .parse::<i64>()
        .ok()
        .filter(|amount| *amount >= 0)
        .ok_or_else(|| SinceParseError {
            value: value.to_owned(),
        })?;
    let duration = match unit {
        'm' => Duration::try_minutes(amount),
        'h' => Duration::try_hours(amount),
        'd' => Duration::try_days(amount),
        'w' => amount.checked_mul(7).and_then(Duration::try_days),
        _ => None,
    }
    .ok_or_else(|| SinceParseError {
        value: value.to_owned(),
    })?;
    now.checked_sub_signed(duration)
        .ok_or_else(|| SinceParseError {
            value: value.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    /// 验证 since 支持相对时长、日期和 RFC3339。
    #[test]
    fn parses_supported_since_formats() {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 10, 12, 0, 0)
            .single()
            .expect("测试时间应有效");
        assert_eq!(
            parse_since("24h", now).expect("相对时间应有效"),
            Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
                .single()
                .expect("测试时间应有效")
        );
        assert_eq!(
            parse_since("2026-07-01", now).expect("日期应有效"),
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
                .single()
                .expect("测试时间应有效")
        );
        assert!(parse_since("2026-07-01T08:00:00+08:00", now).is_ok());
    }

    /// 验证非法 since 值会返回结构化错误。
    #[test]
    fn rejects_invalid_since_value() {
        assert!(parse_since("soon", Utc::now()).is_err());
        assert!(parse_since("-1d", Utc::now()).is_err());
    }
}
