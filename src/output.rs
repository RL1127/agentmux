//! 将统一会话分组输出为安全的文本或 JSON。

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::catalog::{GroupBy, SessionGroup};
use crate::domain::{ScanWarning, Session};

/// 输出 list 查询结果；JSON 包含分组和警告，文本仅展示已脱敏元数据。
pub fn write_list(
    mut writer: impl Write,
    groups: &[SessionGroup<'_>],
    warnings: &[ScanWarning],
    group_by: GroupBy,
    json: bool,
) -> Result<()> {
    if json {
        write_json(&mut writer, groups, warnings, group_by)?;
    } else {
        write_text(&mut writer, groups)?;
    }
    Ok(())
}

/// 写入稳定 JSON 结构，保留统一会话模型和结构化扫描警告。
fn write_json(
    writer: &mut impl Write,
    groups: &[SessionGroup<'_>],
    warnings: &[ScanWarning],
    group_by: GroupBy,
) -> Result<()> {
    let serializable_groups = groups
        .iter()
        .map(|group| JsonGroup {
            key: &group.key,
            sessions: &group.sessions,
        })
        .collect::<Vec<_>>();
    let output = JsonListOutput {
        group_by,
        groups: serializable_groups,
        warnings,
    };
    serde_json::to_writer_pretty(&mut *writer, &output)?;
    writeln!(writer)?;
    Ok(())
}

/// 写入适合终端扫描的紧凑文本，不展示原始路径、提示词或认证信息。
fn write_text(writer: &mut impl Write, groups: &[SessionGroup<'_>]) -> Result<()> {
    if groups.is_empty() {
        writeln!(writer, "未找到匹配的会话。")?;
        return Ok(());
    }

    for group in groups {
        writeln!(writer, "{} ({})", group.key, group.sessions.len())?;
        for session in &group.sessions {
            let provider = session.model_provider.as_deref().unwrap_or("-");
            writeln!(
                writer,
                "  {}  {:<8} {:<12} {}  {}",
                session.updated_at.format("%Y-%m-%d %H:%MZ"),
                session.source,
                provider,
                session.id,
                truncate_chars(session.display_title(), 80)
            )?;
        }
        writeln!(writer)?;
    }
    Ok(())
}

/// 按 Unicode 字符边界截断显示文本，避免中文路径或标题产生无效 UTF-8。
fn truncate_chars(value: &str, limit: usize) -> String {
    let mut result = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        result.push('…');
    }
    result
}

/// JSON list 命令的顶层稳定结构。
#[derive(Serialize)]
struct JsonListOutput<'a> {
    group_by: GroupBy,
    groups: Vec<JsonGroup<'a>>,
    warnings: &'a [ScanWarning],
}

/// JSON 中的单个会话分组。
#[derive(Serialize)]
struct JsonGroup<'a> {
    key: &'a str,
    sessions: &'a [&'a Session],
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};

    use crate::catalog::SessionGroup;
    use crate::domain::{InteractionType, ResumeStatus, SourceId};

    use super::*;

    /// 创建输出测试使用的安全会话。
    fn test_session() -> Session {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 7, 10, 12, 0, 0)
            .single()
            .expect("测试时间应有效");
        Session {
            id: "019f4a0b-a42e-7103-ac37-2ceffc73cb52".to_owned(),
            source: SourceId::new("codex"),
            title: Some("中文会话".to_owned()),
            summary: None,
            cwd: Some(PathBuf::from(r"D:\项目\agentmux")),
            project: "agentmux".to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
            model: Some("gpt-5".to_owned()),
            model_provider: Some("custom".to_owned()),
            interaction: InteractionType::Interactive,
            raw_path: PathBuf::from("session.jsonl"),
            warnings: Vec::new(),
            resume: ResumeStatus::ready(),
        }
    }

    /// 验证普通输出包含恢复所需 ID，但不包含原始路径。
    #[test]
    fn text_output_omits_raw_path() {
        let session = test_session();
        let groups = vec![SessionGroup {
            key: "agentmux".to_owned(),
            sessions: vec![&session],
        }];
        let mut output = Vec::new();

        write_list(&mut output, &groups, &[], GroupBy::Project, false).expect("文本输出应成功");
        let text = String::from_utf8(output).expect("输出应为 UTF-8");
        assert!(text.contains(&session.id));
        assert!(text.contains("中文会话"));
        assert!(!text.contains("session.jsonl"));
    }

    /// 验证 JSON 输出包含稳定分组字段并保持合法 UTF-8。
    #[test]
    fn json_output_is_structured() {
        let session = test_session();
        let groups = vec![SessionGroup {
            key: "agentmux".to_owned(),
            sessions: vec![&session],
        }];
        let mut output = Vec::new();

        write_list(&mut output, &groups, &[], GroupBy::Project, true).expect("JSON 输出应成功");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("输出应为合法 JSON");
        assert_eq!(value["group_by"], "project");
        assert_eq!(value["groups"][0]["sessions"][0]["id"], session.id);
    }
}
