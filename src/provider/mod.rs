//! 定义可扩展的 Agent 会话来源接口和运行时注册表。

pub mod codex;

use std::path::Path;

use thiserror::Error;

use crate::domain::{
    CommandSpec, Diagnostic, RepairOptions, RepairReport, ResumeStatus, ScanReport, ScanWarning,
    Session, SessionSummary, SourceDescriptor, SourceId, WarningKind,
};

/// 表示来源扫描、恢复或配置处理中的结构化错误。
#[derive(Debug, Error)]
pub enum ProviderError {
    /// 文件或目录操作失败。
    #[error("无法访问 {path}: {source}")]
    Io {
        /// 出错路径。
        path: std::path::PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 来源数据无法满足恢复所需的最低结构。
    #[error("会话数据无效: {reason}")]
    InvalidData {
        /// 相关文件。
        path: std::path::PathBuf,
        /// 可选的一基行号。
        line: Option<usize>,
        /// 不包含原始敏感正文的原因说明。
        reason: String,
    },
    /// 所需 Agent CLI 不存在。
    #[error("找不到 Agent CLI: {program}")]
    CommandUnavailable {
        /// 缺失的程序名称。
        program: String,
    },
    /// 当前来源不支持请求的能力。
    #[error("来源 {provider_id} 不支持 {operation}")]
    Unsupported {
        /// 来源 ID。
        provider_id: SourceId,
        /// 请求的操作。
        operation: &'static str,
    },
    /// 配置文件存在结构或校验问题。
    #[error("配置错误: {message}")]
    Config {
        /// 已脱敏的配置错误说明。
        message: String,
    },
    /// 外部命令执行失败。
    #[error("外部命令执行失败: {program}")]
    Process {
        /// 命令名称。
        program: String,
        /// 可选退出码；被信号终止时为空。
        exit_code: Option<i32>,
    },
    /// 配置修复失败，若已生成备份则携带其路径。
    #[error("配置修复失败: {message}")]
    Repair {
        /// 已脱敏的失败说明。
        message: String,
        /// 可用于人工恢复的原配置备份。
        backup_path: Option<std::path::PathBuf>,
    },
}

/// 每个 Agent 来源必须实现的扫描、摘要、恢复和诊断契约。
pub trait SessionProvider: Send + Sync {
    /// 返回来源标识、说明和能力列表。
    fn descriptor(&self) -> SourceDescriptor;

    /// 只读扫描本地会话；单文件错误应进入报告警告而非中断全部扫描。
    fn scan_sessions(&self) -> Result<ScanReport, ProviderError>;

    /// 从给定原始文件按需读取已脱敏的标题和摘要。
    fn read_summary(&self, raw_path: &Path) -> Result<SessionSummary, ProviderError>;

    /// 检查会话文件、CLI 和来源配置是否允许恢复。
    fn check_resume(&self, session: &Session) -> Result<ResumeStatus, ProviderError>;

    /// 使用来源官方入口构造恢复命令，调用方不得自行重放会话内容。
    fn build_resume_command(&self, session: &Session) -> Result<CommandSpec, ProviderError>;

    /// 构造来源桌面应用的会话 URI；默认表示该来源没有桌面导航能力。
    fn build_app_uri(&self, _session: &Session) -> Result<Option<String>, ProviderError> {
        Ok(None)
    }

    /// 检查 CLI、配置和会话目录，并返回不含认证信息的诊断项。
    fn diagnose(&self) -> Vec<Diagnostic>;

    /// 为历史模型提供商创建兼容配置；默认明确报告来源不支持修复。
    fn repair_model_provider(
        &self,
        _session: &Session,
        _options: RepairOptions,
    ) -> Result<RepairReport, ProviderError> {
        Err(ProviderError::Unsupported {
            provider_id: self.descriptor().id,
            operation: "模型提供商修复",
        })
    }
}

/// 保存所有启用的来源实现，核心筛选和界面只依赖此注册表。
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn SessionProvider>>,
}

impl ProviderRegistry {
    /// 创建空注册表，调用方随后注册实际来源。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个来源实现；新增来源无需修改筛选、分组或 TUI。
    pub fn register(&mut self, provider: impl SessionProvider + 'static) {
        self.providers.push(Box::new(provider));
    }

    /// 按注册顺序返回全部来源实现的只读迭代器。
    pub fn providers(&self) -> impl Iterator<Item = &dyn SessionProvider> {
        self.providers.iter().map(Box::as_ref)
    }

    /// 按稳定来源 ID 查找实现。
    pub fn get(&self, source: &SourceId) -> Option<&dyn SessionProvider> {
        self.providers()
            .find(|provider| provider.descriptor().id == *source)
    }

    /// 扫描全部来源；单个来源失败时生成警告并继续其他来源。
    pub fn scan_all(&self) -> ScanReport {
        let mut combined = ScanReport::default();
        for provider in self.providers() {
            match provider.scan_sessions() {
                Ok(report) => combined.extend(report),
                Err(error) => {
                    let descriptor = provider.descriptor();
                    combined.warnings.push(ScanWarning::new(
                        WarningKind::Provider,
                        format!("来源 {} 扫描失败: {error}", descriptor.id),
                    ));
                }
            }
        }
        combined
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{DiagnosticSeverity, InteractionType, ProviderCapability, ResumeState};

    use super::*;

    /// 提供注册表测试所需的最小模拟来源。
    struct MockProvider {
        id: &'static str,
        fail: bool,
    }

    impl SessionProvider for MockProvider {
        /// 返回模拟来源信息。
        fn descriptor(&self) -> SourceDescriptor {
            SourceDescriptor {
                id: SourceId::new(self.id),
                display_name: self.id.to_owned(),
                description: "测试来源".to_owned(),
                capabilities: vec![ProviderCapability::Scan],
            }
        }

        /// 返回一条固定会话，或按测试参数模拟来源级失败。
        fn scan_sessions(&self) -> Result<ScanReport, ProviderError> {
            if self.fail {
                return Err(ProviderError::Config {
                    message: "模拟失败".to_owned(),
                });
            }
            let now = chrono::Utc::now();
            Ok(ScanReport {
                sessions: vec![Session {
                    id: format!("{}-session", self.id),
                    source: SourceId::new(self.id),
                    title: None,
                    summary: None,
                    cwd: None,
                    project: "unknown".to_owned(),
                    created_at: now,
                    updated_at: now,
                    model: None,
                    model_provider: None,
                    interaction: InteractionType::Unknown,
                    raw_path: PathBuf::from("mock.jsonl"),
                    warnings: Vec::new(),
                    resume: ResumeStatus::blocked(ResumeState::Unsupported, "测试来源"),
                }],
                warnings: Vec::new(),
            })
        }

        /// 模拟来源不提供摘要。
        fn read_summary(&self, _raw_path: &Path) -> Result<SessionSummary, ProviderError> {
            Ok(SessionSummary {
                title: None,
                summary: None,
            })
        }

        /// 返回模拟不可恢复状态。
        fn check_resume(&self, _session: &Session) -> Result<ResumeStatus, ProviderError> {
            Ok(ResumeStatus::blocked(ResumeState::Unsupported, "测试来源"))
        }

        /// 模拟来源不支持恢复命令。
        fn build_resume_command(&self, _session: &Session) -> Result<CommandSpec, ProviderError> {
            Err(ProviderError::Unsupported {
                provider_id: SourceId::new(self.id),
                operation: "恢复",
            })
        }

        /// 返回一个固定的诊断项。
        fn diagnose(&self) -> Vec<Diagnostic> {
            vec![Diagnostic {
                name: "mock".to_owned(),
                severity: DiagnosticSeverity::Info,
                message: "正常".to_owned(),
                suggestion: None,
            }]
        }
    }

    /// 验证注册表会隔离来源级失败并保留其他会话。
    #[test]
    fn registry_isolates_provider_failures() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider {
            id: "healthy",
            fail: false,
        });
        registry.register(MockProvider {
            id: "broken",
            fail: true,
        });

        let report = registry.scan_all();
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.sessions[0].id, "healthy-session");
    }
}
