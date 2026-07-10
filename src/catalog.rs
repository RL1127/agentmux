//! 提供与来源无关的会话过滤、模糊搜索、排序和分组。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::Serialize;

use crate::domain::{InteractionType, ScanReport, ScanWarning, Session};

/// 定义列表和 TUI 可切换的分组维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// 按工作目录末级名称分组。
    Project,
    /// 按 UTC 更新日期分组。
    Date,
    /// 按 Agent 来源分组。
    Source,
    /// 按模型提供商分组。
    Provider,
}

impl GroupBy {
    /// 返回下一个分组维度，供 TUI 的 Tab 循环切换。
    pub fn next(self) -> Self {
        match self {
            Self::Project => Self::Date,
            Self::Date => Self::Source,
            Self::Source => Self::Provider,
            Self::Provider => Self::Project,
        }
    }
}

/// 保存一次查询中的可选过滤条件。
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    /// 仅保留指定 Agent 来源。
    pub source: Option<String>,
    /// 仅保留指定项目名称。
    pub project: Option<String>,
    /// 仅保留指定模型提供商。
    pub provider: Option<String>,
    /// 仅保留更新时间不早于该时间的会话。
    pub since: Option<DateTime<Utc>>,
    /// 是否包含明确标记为非交互的会话。
    pub include_non_interactive: bool,
    /// 对标题、摘要、路径和会话 ID 执行模糊搜索。
    pub search: Option<String>,
}

/// 持有扫描结果，并为 CLI 与 TUI 提供统一查询入口。
#[derive(Debug, Clone)]
pub struct SessionCatalog {
    sessions: Vec<Session>,
    warnings: Vec<ScanWarning>,
}

impl SessionCatalog {
    /// 从多来源扫描报告创建目录，并预先按更新时间倒序排列。
    pub fn from_report(mut report: ScanReport) -> Self {
        report
            .sessions
            .sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Self {
            sessions: report.sessions,
            warnings: report.warnings,
        }
    }

    /// 返回扫描时收集的全部非致命警告。
    pub fn warnings(&self) -> &[ScanWarning] {
        &self.warnings
    }

    /// 返回未过滤的全部会话，顺序为更新时间倒序。
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// 应用过滤和模糊搜索，并按匹配分数、更新时间倒序返回引用。
    pub fn query(&self, query: &SessionQuery) -> Vec<&Session> {
        let matcher = SkimMatcherV2::default().ignore_case();
        let search = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut matched = self
            .sessions
            .iter()
            .filter(|session| matches_filters(session, query))
            .filter_map(|session| {
                let score = search
                    .map(|needle| matcher.fuzzy_match(&session.searchable_text(), needle))
                    .unwrap_or(Some(0))?;
                Some((session, score))
            })
            .collect::<Vec<_>>();
        matched.sort_by(|(left, left_score), (right, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        matched.into_iter().map(|(session, _)| session).collect()
    }

    /// 按来源和会话 ID 查找唯一会话，供恢复命令使用。
    pub fn find(&self, id: &str, source: Option<&str>) -> Option<&Session> {
        self.sessions.iter().find(|session| {
            session.id == id
                && source
                    .map(|value| session.source.as_str().eq_ignore_ascii_case(value))
                    .unwrap_or(true)
        })
    }
}

/// 保存一个分组名称和其中按更新时间倒序排列的会话。
#[derive(Debug)]
pub struct SessionGroup<'a> {
    /// 分组显示名称。
    pub key: String,
    /// 属于该分组的会话。
    pub sessions: Vec<&'a Session>,
}

/// 将已排序会话按指定维度分组，并保留各组首次出现的顺序。
pub fn group_sessions<'a>(
    sessions: impl IntoIterator<Item = &'a Session>,
    group_by: GroupBy,
) -> Vec<SessionGroup<'a>> {
    let mut groups = Vec::<SessionGroup<'a>>::new();
    let mut indices = HashMap::<String, usize>::new();
    for session in sessions {
        let key = group_key(session, group_by);
        let index = match indices.get(&key) {
            Some(index) => *index,
            None => {
                let index = groups.len();
                groups.push(SessionGroup {
                    key: key.clone(),
                    sessions: Vec::new(),
                });
                indices.insert(key, index);
                index
            }
        };
        groups[index].sessions.push(session);
    }
    groups
}

/// 判断单个会话是否满足非搜索过滤条件。
fn matches_filters(session: &Session, query: &SessionQuery) -> bool {
    if !query.include_non_interactive && session.interaction == InteractionType::NonInteractive {
        return false;
    }
    if let Some(source) = query.source.as_deref()
        && !session.source.as_str().eq_ignore_ascii_case(source)
    {
        return false;
    }
    if let Some(project) = query.project.as_deref()
        && !session.project.eq_ignore_ascii_case(project)
    {
        return false;
    }
    if let Some(provider) = query.provider.as_deref()
        && !session
            .model_provider
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(provider))
    {
        return false;
    }
    if let Some(since) = query.since
        && session.updated_at < since
    {
        return false;
    }
    true
}

/// 根据分组维度计算稳定显示键。
fn group_key(session: &Session, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::Project => session.project.clone(),
        GroupBy::Date => session.updated_at.format("%Y-%m-%d").to_string(),
        GroupBy::Source => session.source.to_string(),
        GroupBy::Provider => session
            .model_provider
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::TimeZone;

    use crate::domain::{ResumeStatus, SourceId};

    use super::*;

    /// 创建具有稳定时间和元数据的测试会话。
    fn session(
        id: &str,
        project: &str,
        provider: Option<&str>,
        interaction: InteractionType,
        hour: u32,
    ) -> Session {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 7, 10, hour, 0, 0)
            .single()
            .expect("测试时间应有效");
        Session {
            id: id.to_owned(),
            source: SourceId::new("codex"),
            title: Some(format!("处理 {project}")),
            summary: Some("统一会话检索".to_owned()),
            cwd: Some(PathBuf::from(format!(r"D:\项目\{project}"))),
            project: project.to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
            model: Some("gpt-5".to_owned()),
            model_provider: provider.map(ToOwned::to_owned),
            interaction,
            raw_path: PathBuf::from(format!("{id}.jsonl")),
            warnings: Vec::new(),
            resume: ResumeStatus::ready(),
        }
    }

    /// 验证来源、项目、provider、时间和非交互过滤可组合使用。
    #[test]
    fn filters_sessions_with_combined_conditions() {
        let report = ScanReport {
            sessions: vec![
                session(
                    "interactive",
                    "agentmux",
                    Some("custom"),
                    InteractionType::Interactive,
                    12,
                ),
                session(
                    "non-interactive",
                    "agentmux",
                    Some("custom"),
                    InteractionType::NonInteractive,
                    13,
                ),
                session(
                    "other",
                    "其他项目",
                    Some("openai"),
                    InteractionType::Interactive,
                    14,
                ),
            ],
            warnings: Vec::new(),
        };
        let catalog = SessionCatalog::from_report(report);
        let query = SessionQuery {
            source: Some("CODEX".to_owned()),
            project: Some("agentmux".to_owned()),
            provider: Some("CUSTOM".to_owned()),
            since: Some(
                Utc.with_ymd_and_hms(2026, 7, 10, 11, 0, 0)
                    .single()
                    .expect("测试时间应有效"),
            ),
            include_non_interactive: false,
            search: None,
        };

        let result = catalog.query(&query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "interactive");
    }

    /// 验证模糊搜索覆盖中文标题、路径和会话 ID。
    #[test]
    fn fuzzy_searches_title_path_and_id() {
        let catalog = SessionCatalog::from_report(ScanReport {
            sessions: vec![session(
                "019f-agentmux",
                "智能助手",
                Some("custom"),
                InteractionType::Interactive,
                12,
            )],
            warnings: Vec::new(),
        });

        for needle in ["处理智能", "项目智能", "019f"] {
            let query = SessionQuery {
                search: Some(needle.to_owned()),
                ..SessionQuery::default()
            };
            assert_eq!(catalog.query(&query).len(), 1);
        }
    }

    /// 验证分组按最近会话决定组顺序，并保持组内倒序。
    #[test]
    fn groups_by_project_in_recent_order() {
        let catalog = SessionCatalog::from_report(ScanReport {
            sessions: vec![
                session(
                    "older",
                    "agentmux",
                    Some("custom"),
                    InteractionType::Interactive,
                    10,
                ),
                session(
                    "newer",
                    "其他项目",
                    Some("openai"),
                    InteractionType::Interactive,
                    14,
                ),
                session(
                    "middle",
                    "agentmux",
                    Some("custom"),
                    InteractionType::Interactive,
                    12,
                ),
            ],
            warnings: Vec::new(),
        });
        let groups = group_sessions(catalog.query(&SessionQuery::default()), GroupBy::Project);

        assert_eq!(groups[0].key, "其他项目");
        assert_eq!(groups[1].key, "agentmux");
        assert_eq!(groups[1].sessions[0].id, "middle");
        assert_eq!(groups[1].sessions[1].id, "older");
    }
}
