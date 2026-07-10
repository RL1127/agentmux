//! Codex 本地会话的只读扫描、摘要读取、恢复检查和环境诊断实现。

mod config;

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::domain::{
    CommandSpec, Diagnostic, DiagnosticSeverity, InteractionType, ProviderCapability,
    RepairOptions, RepairReport, ResumeState, ResumeStatus, ScanReport, ScanWarning, Session,
    SessionSummary, SourceDescriptor, SourceId, WarningKind,
};

use super::{ProviderError, SessionProvider};

const SOURCE_ID: &str = "codex";
const TITLE_LIMIT: usize = 100;
const SUMMARY_LIMIT: usize = 240;

/// 实现 Codex 会话目录发现和 JSONL 解析，所有扫描操作保持只读。
#[derive(Debug, Clone)]
pub struct CodexProvider {
    home: PathBuf,
    cli_program: PathBuf,
}

impl CodexProvider {
    /// 根据 CODEX_HOME 或平台用户目录创建 provider。
    pub fn discover() -> Result<Self, ProviderError> {
        let home = match env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => dirs::home_dir()
                .map(|path| path.join(".codex"))
                .ok_or_else(|| ProviderError::Config {
                    message: "无法确定用户主目录，请设置 CODEX_HOME".to_owned(),
                })?,
        };
        Ok(Self::with_home(home))
    }

    /// 使用显式 Codex 主目录创建 provider，主要用于测试和嵌入式调用。
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            cli_program: PathBuf::from("codex"),
        }
    }

    /// 覆盖 Codex CLI 程序路径，便于测试模拟进程或使用非标准安装位置。
    pub fn with_cli_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.cli_program = program.into();
        self
    }

    /// 返回当前 provider 使用的 Codex 主目录。
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// 返回 Codex 配置文件的标准路径。
    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// 返回会话索引文件路径；索引缺失时仍可扫描 JSONL。
    pub fn session_index_path(&self) -> PathBuf {
        self.home.join("session_index.jsonl")
    }

    /// 列出当前和归档会话目录，保持 Codex 原始文件不变。
    fn session_roots(&self) -> [PathBuf; 2] {
        [
            self.home.join("sessions"),
            self.home.join("archived_sessions"),
        ]
    }

    /// 读取索引中的标题和更新时间；坏行只生成警告。
    fn read_index(&self) -> (HashMap<String, IndexEntry>, Vec<ScanWarning>) {
        let path = self.session_index_path();
        if !path.is_file() {
            return (HashMap::new(), Vec::new());
        }

        let mut entries = HashMap::new();
        let mut warnings = Vec::new();
        for record in read_jsonl(&path, &mut warnings) {
            let Some(id) = string_field(&record, "id") else {
                warnings.push(
                    ScanWarning::new(WarningKind::MissingField, "会话索引记录缺少 id")
                        .with_path(&path),
                );
                continue;
            };
            let title = string_field(&record, "thread_name")
                .and_then(|value| sanitize_text(value, TITLE_LIMIT));
            let updated_at = string_field(&record, "updated_at").and_then(parse_timestamp);
            entries.insert(id.to_owned(), IndexEntry { title, updated_at });
        }
        (entries, warnings)
    }

    /// 扫描单个 JSONL 文件并合并可用索引信息。
    fn parse_session_file(
        &self,
        path: &Path,
        index: Option<&IndexEntry>,
        cli_available: bool,
    ) -> Result<Session, Vec<ScanWarning>> {
        let mut warnings = Vec::new();
        let records = read_jsonl(path, &mut warnings);
        let filename_id = session_id_from_filename(path);
        let mut metadata = Vec::new();
        let mut first_timestamp = None;
        let mut latest_timestamp = None;
        let mut model = None;
        let mut summary = None;

        for record in records {
            let record_timestamp = string_field(&record, "timestamp").and_then(parse_timestamp);
            if let Some(timestamp) = record_timestamp {
                first_timestamp = Some(first_timestamp.unwrap_or(timestamp));
                latest_timestamp = Some(
                    latest_timestamp
                        .map(|current: DateTime<Utc>| current.max(timestamp))
                        .unwrap_or(timestamp),
                );
            }

            match string_field(&record, "type") {
                Some("session_meta") => {
                    if let Some(payload) = record.get("payload") {
                        metadata.push(SessionMetadata::from_payload(payload, record_timestamp));
                    }
                }
                Some("turn_context") => {
                    if let Some(payload) = record.get("payload") {
                        if let Some(value) = string_field(payload, "model") {
                            model = sanitize_text(value, 80);
                        }
                        if let Some(value) = string_field(payload, "summary") {
                            summary = sanitize_text(value, SUMMARY_LIMIT);
                        }
                    }
                }
                _ => {}
            }
        }

        let selected = select_metadata(metadata, filename_id.as_deref());
        let id = filename_id
            .or_else(|| selected.as_ref().and_then(|item| item.id.clone()))
            .filter(|value| !value.trim().is_empty());
        let Some(id) = id else {
            warnings.push(
                ScanWarning::new(
                    WarningKind::MissingField,
                    "会话文件缺少 id，且文件名不包含 UUID",
                )
                .with_path(path),
            );
            return Err(warnings);
        };

        let file_time = file_modified_at(path);
        let created_at = selected
            .as_ref()
            .and_then(|item| item.created_at)
            .or(first_timestamp)
            .or(file_time)
            .unwrap_or_else(Utc::now);
        let updated_at = [
            latest_timestamp,
            index.and_then(|item| item.updated_at),
            file_time,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(created_at);
        let cwd = selected.as_ref().and_then(|item| item.cwd.clone());
        let interaction = selected
            .as_ref()
            .map(SessionMetadata::interaction_type)
            .unwrap_or(InteractionType::Unknown);
        let model_provider = selected.and_then(|item| item.model_provider);
        let resume = if !path.is_file() {
            ResumeStatus::blocked(ResumeState::SessionMissing, "原始会话文件不存在")
        } else if !cli_available {
            ResumeStatus::blocked(ResumeState::CliMissing, "未在 PATH 中找到 Codex CLI")
        } else {
            ResumeStatus::ready()
        };

        Ok(Session {
            id,
            source: SourceId::new(SOURCE_ID),
            title: index.and_then(|item| item.title.clone()),
            summary,
            project: Session::project_from_cwd(cwd.as_deref()),
            cwd,
            created_at,
            updated_at,
            model,
            model_provider,
            interaction,
            raw_path: path.to_path_buf(),
            warnings,
            resume,
        })
    }

    /// 判断配置的 CLI 名称或路径能否在当前环境中解析。
    fn cli_available(&self) -> bool {
        resolve_program(&self.cli_program).is_some()
    }
}

impl SessionProvider for CodexProvider {
    /// 返回 Codex 来源描述和当前能力。
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            id: SourceId::new(SOURCE_ID),
            display_name: "OpenAI Codex".to_owned(),
            description: "扫描 Codex JSONL 会话并通过官方 codex resume 恢复".to_owned(),
            capabilities: vec![
                ProviderCapability::Scan,
                ProviderCapability::Summary,
                ProviderCapability::Resume,
                ProviderCapability::Diagnose,
                ProviderCapability::RepairProvider,
            ],
        }
    }

    /// 只读扫描 Codex 当前与归档会话，并按更新时间倒序返回。
    fn scan_sessions(&self) -> Result<ScanReport, ProviderError> {
        let (index, mut warnings) = self.read_index();
        let cli_available = self.cli_available();
        let mut sessions = Vec::new();

        for root in self.session_roots() {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(&root).follow_links(false) {
                let entry = match entry {
                    Ok(value) => value,
                    Err(error) => {
                        warnings.push(
                            ScanWarning::new(
                                WarningKind::Io,
                                format!("无法遍历 Codex 会话目录: {error}"),
                            )
                            .with_path(&root),
                        );
                        continue;
                    }
                };
                let path = entry.path();
                if !entry.file_type().is_file()
                    || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                {
                    continue;
                }

                let indexed = session_id_from_filename(path)
                    .as_deref()
                    .and_then(|id| index.get(id));
                match self.parse_session_file(path, indexed, cli_available) {
                    Ok(session) => {
                        warnings.extend(session.warnings.iter().cloned());
                        sessions.push(session);
                    }
                    Err(file_warnings) => warnings.extend(file_warnings),
                }
            }
        }

        for session in &mut sessions {
            session.resume = self.check_resume(session).unwrap_or_else(|_| {
                ResumeStatus::blocked(ResumeState::Blocked, "Codex 恢复检查失败")
            });
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(ScanReport { sessions, warnings })
    }

    /// 从单个 Codex JSONL 文件读取安全标题和摘要，不返回消息正文。
    fn read_summary(&self, raw_path: &Path) -> Result<SessionSummary, ProviderError> {
        let (index, _) = self.read_index();
        let indexed = session_id_from_filename(raw_path)
            .as_deref()
            .and_then(|id| index.get(id));
        self.parse_session_file(raw_path, indexed, self.cli_available())
            .map(|session| SessionSummary {
                title: session.title,
                summary: session.summary,
            })
            .map_err(|warnings| ProviderError::InvalidData {
                path: raw_path.to_path_buf(),
                line: warnings.first().and_then(|warning| warning.line),
                reason: warnings
                    .first()
                    .map(|warning| warning.message.clone())
                    .unwrap_or_else(|| "无法解析 Codex 会话".to_owned()),
            })
    }

    /// 检查会话来源、原始文件和 Codex CLI 是否可用于恢复。
    fn check_resume(&self, session: &Session) -> Result<ResumeStatus, ProviderError> {
        if session.source.as_str() != SOURCE_ID {
            return Ok(ResumeStatus::blocked(
                ResumeState::Unsupported,
                "会话不属于 Codex 来源",
            ));
        }
        if !session.raw_path.is_file() {
            return Ok(ResumeStatus::blocked(
                ResumeState::SessionMissing,
                "原始会话文件不存在",
            ));
        }
        if !self.cli_available() {
            return Ok(ResumeStatus::blocked(
                ResumeState::CliMissing,
                "未在 PATH 中找到 Codex CLI",
            ));
        }
        if let Some(provider) = session.model_provider.as_deref() {
            match config::provider_exists(&self.config_path(), provider) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(ResumeStatus::blocked(
                        ResumeState::ProviderMissing,
                        format!(
                            "历史 provider {provider} 未在 config.toml 中定义；使用 --repair-provider 显式修复"
                        ),
                    ));
                }
                Err(_) => {
                    return Ok(ResumeStatus::blocked(
                        ResumeState::Blocked,
                        "Codex config.toml 无法解析，请先运行 agentmux doctor",
                    ));
                }
            }
        }
        Ok(ResumeStatus::ready())
    }

    /// 构造官方 codex resume SESSION_ID 命令，不读取或重放会话内容。
    fn build_resume_command(&self, session: &Session) -> Result<CommandSpec, ProviderError> {
        if session.source.as_str() != SOURCE_ID {
            return Err(ProviderError::Unsupported {
                provider_id: SourceId::new(SOURCE_ID),
                operation: "恢复其他来源会话",
            });
        }
        if Uuid::parse_str(&session.id).is_err() {
            return Err(ProviderError::InvalidData {
                path: session.raw_path.clone(),
                line: None,
                reason: "Codex 会话 ID 不是合法 UUID".to_owned(),
            });
        }
        Ok(CommandSpec::new(
            self.cli_program.clone(),
            vec![OsString::from("resume"), OsString::from(&session.id)],
        ))
    }

    /// 检查 Codex CLI、配置文件和会话目录，不解析或输出认证内容。
    fn diagnose(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        diagnostics.push(if self.cli_available() {
            Diagnostic {
                name: "codex-cli".to_owned(),
                severity: DiagnosticSeverity::Info,
                message: "已找到 Codex CLI".to_owned(),
                suggestion: None,
            }
        } else {
            Diagnostic {
                name: "codex-cli".to_owned(),
                severity: DiagnosticSeverity::Error,
                message: "未在 PATH 中找到 Codex CLI".to_owned(),
                suggestion: Some("安装 Codex CLI 后重新运行 agentmux doctor".to_owned()),
            }
        });

        let config_path = self.config_path();
        diagnostics.push(if !config_path.is_file() {
            Diagnostic {
                name: "codex-config".to_owned(),
                severity: DiagnosticSeverity::Warning,
                message: format!("配置文件不存在: {}", config_path.display()),
                suggestion: Some("先运行 codex 完成初始配置".to_owned()),
            }
        } else {
            match config::inspect_config(&config_path) {
                Ok(summary) => Diagnostic {
                    name: "codex-config".to_owned(),
                    severity: DiagnosticSeverity::Info,
                    message: format!(
                        "配置 TOML 有效，默认 provider: {}，已定义 {} 个 provider",
                        summary.default_provider.as_deref().unwrap_or("未设置"),
                        summary.provider_count
                    ),
                    suggestion: None,
                },
                Err(_) => Diagnostic {
                    name: "codex-config".to_owned(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("配置文件无法解析: {}", config_path.display()),
                    suggestion: Some(
                        "修正 config.toml 后运行 codex --strict-config features list".to_owned(),
                    ),
                },
            }
        });

        let session_count = self
            .session_roots()
            .into_iter()
            .filter(|path| path.is_dir())
            .flat_map(|path| WalkDir::new(path).follow_links(false))
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            })
            .count();
        diagnostics.push(if session_count > 0 {
            Diagnostic {
                name: "codex-sessions".to_owned(),
                severity: DiagnosticSeverity::Info,
                message: format!(
                    "已找到 {session_count} 个 Codex 会话文件: {}",
                    self.home.display()
                ),
                suggestion: None,
            }
        } else {
            Diagnostic {
                name: "codex-sessions".to_owned(),
                severity: DiagnosticSeverity::Warning,
                message: format!("未找到 Codex 会话目录: {}", self.home.display()),
                suggestion: Some("先使用 Codex 创建至少一个本地会话".to_owned()),
            }
        });
        diagnostics
    }

    /// 将当前默认 provider 复制为会话历史名称，并执行备份、原子替换和校验。
    fn repair_model_provider(
        &self,
        session: &Session,
        options: RepairOptions,
    ) -> Result<RepairReport, ProviderError> {
        let historical_provider =
            session
                .model_provider
                .as_deref()
                .ok_or_else(|| ProviderError::Config {
                    message: "会话历史未记录 model_provider，无法创建兼容别名".to_owned(),
                })?;
        config::repair_alias(
            &self.config_path(),
            &self.cli_program,
            historical_provider,
            options.confirmed,
        )
    }
}

/// 保存索引中可公开显示的标题和更新时间。
#[derive(Debug, Clone)]
struct IndexEntry {
    title: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

/// 保存从 session_meta 提取的非敏感字段。
#[derive(Debug, Clone)]
struct SessionMetadata {
    id: Option<String>,
    created_at: Option<DateTime<Utc>>,
    cwd: Option<PathBuf>,
    source: Option<String>,
    thread_source: Option<String>,
    model_provider: Option<String>,
}

impl SessionMetadata {
    /// 从动态 JSON payload 中提取已知字段，忽略所有未知字段以保持向前兼容。
    fn from_payload(payload: &Value, record_timestamp: Option<DateTime<Utc>>) -> Self {
        let id = string_field(payload, "id")
            .or_else(|| string_field(payload, "session_id"))
            .map(ToOwned::to_owned);
        let created_at = string_field(payload, "timestamp")
            .and_then(parse_timestamp)
            .or(record_timestamp);
        let cwd = string_field(payload, "cwd")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        let source = payload.get("source").and_then(source_label);
        let thread_source = string_field(payload, "thread_source").map(ToOwned::to_owned);
        let model_provider =
            string_field(payload, "model_provider").and_then(|value| sanitize_text(value, 80));
        Self {
            id,
            created_at,
            cwd,
            source,
            thread_source,
            model_provider,
        }
    }

    /// 根据 Codex source 和 thread_source 判断交互类型。
    fn interaction_type(&self) -> InteractionType {
        if self.thread_source.as_deref() == Some("subagent")
            || matches!(self.source.as_deref(), Some("exec" | "subagent" | "mcp"))
        {
            return InteractionType::NonInteractive;
        }
        if matches!(
            self.source.as_deref(),
            Some("cli" | "vscode" | "desktop" | "user")
        ) || self.thread_source.as_deref() == Some("user")
        {
            return InteractionType::Interactive;
        }
        InteractionType::Unknown
    }
}

/// 逐行读取 UTF-8 JSONL；坏行与编码错误会被隔离成警告。
fn read_jsonl(path: &Path, warnings: &mut Vec<ScanWarning>) -> Vec<Value> {
    let file = match File::open(path) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(
                ScanWarning::new(WarningKind::Io, format!("无法读取会话文件: {error}"))
                    .with_path(path),
            );
            return Vec::new();
        }
    };
    let mut records = Vec::new();
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut line_number = 0;

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                line_number += 1;
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                if buffer.is_empty() {
                    continue;
                }
                let text = match std::str::from_utf8(&buffer) {
                    Ok(value) => value,
                    Err(_) => {
                        warnings.push(
                            ScanWarning::new(
                                WarningKind::InvalidUtf8,
                                "JSONL 行不是合法 UTF-8，已跳过该行",
                            )
                            .with_path(path)
                            .with_line(line_number),
                        );
                        continue;
                    }
                };
                match serde_json::from_str(text) {
                    Ok(value) => records.push(value),
                    Err(error) => warnings.push(
                        ScanWarning::new(
                            WarningKind::InvalidRecord,
                            format!("JSONL 记录格式错误，已跳过: {error}"),
                        )
                        .with_path(path)
                        .with_line(line_number),
                    ),
                }
            }
            Err(error) => {
                warnings.push(
                    ScanWarning::new(WarningKind::Io, format!("读取 JSONL 失败: {error}"))
                        .with_path(path),
                );
                break;
            }
        }
    }
    records
}

/// 从对象中读取字符串字段，字段缺失或类型未知时返回空值。
fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

/// 将 Codex source 的字符串或单键对象形式归一化为标签。
fn source_label(value: &Value) -> Option<String> {
    match value {
        Value::String(label) => Some(label.clone()),
        Value::Object(fields) if fields.len() == 1 => fields.keys().next().cloned(),
        _ => None,
    }
}

/// 从 RFC 3339 字符串解析 UTC 时间，无法解析时返回空值。
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// 读取文件修改时间作为缺失时间戳的回退值。
fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

/// 从 rollout 文件名末尾提取 UUID，不依赖日期前缀格式。
fn session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate: String = stem
        .chars()
        .rev()
        .take(36)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Uuid::parse_str(&candidate)
        .ok()
        .map(|value| value.to_string())
}

/// 优先选择与文件名 UUID 一致的 metadata，否则使用首条有效 metadata。
fn select_metadata(
    metadata: Vec<SessionMetadata>,
    filename_id: Option<&str>,
) -> Option<SessionMetadata> {
    if let Some(expected) = filename_id
        && let Some(index) = metadata
            .iter()
            .position(|item| item.id.as_deref() == Some(expected))
    {
        return metadata.into_iter().nth(index);
    }
    metadata.into_iter().next()
}

/// 截断并脱敏标题或摘要，避免普通输出泄露 token、密码或完整提示词。
fn sanitize_text(value: &str, max_chars: usize) -> Option<String> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let redacted = secret_pattern()
        .replace_all(&collapsed, "$name=[REDACTED]")
        .into_owned();
    let redacted = bearer_pattern()
        .replace_all(&redacted, "Bearer [REDACTED]")
        .into_owned();
    let redacted = openai_key_pattern()
        .replace_all(&redacted, "[REDACTED]")
        .into_owned();
    let mut truncated: String = redacted.chars().take(max_chars).collect();
    if redacted.chars().count() > max_chars {
        truncated.push('…');
    }
    Some(truncated)
}

/// 延迟编译常见键值形式的秘密字段匹配规则。
fn secret_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?i)(?<name>api[_-]?key|access[_-]?token|token|password|secret)\s*[:=]\s*[A-Za-z0-9._~+/=-]+",
        )
        .expect("静态秘密字段正则必须有效")
    })
}

/// 延迟编译 Bearer 凭据匹配规则。
fn bearer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]+").expect("静态 Bearer 正则必须有效")
    })
}

/// 延迟编译常见 OpenAI 风格密钥匹配规则。
fn openai_key_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").expect("静态密钥正则必须有效"))
}

/// 在显式路径或 PATH 中解析程序；Windows 同时考虑 PATHEXT 脚本包装器。
fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file().then(|| program.to_path_buf());
    }
    let path_value = env::var_os("PATH")?;
    let names = executable_names(program);
    env::split_paths(&path_value)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
}

/// 构造当前平台可执行文件候选名称。
fn executable_names(program: &Path) -> Vec<OsString> {
    let base = program.as_os_str().to_os_string();
    #[cfg(windows)]
    {
        if program.extension().is_some() {
            return vec![base];
        }
        let extensions = env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|item| !item.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                vec![
                    ".COM".to_owned(),
                    ".EXE".to_owned(),
                    ".BAT".to_owned(),
                    ".CMD".to_owned(),
                ]
            });
        let mut names = vec![base.clone()];
        names.extend(extensions.into_iter().map(|extension| {
            let mut name = base.clone();
            name.push(extension);
            name
        }));
        names
    }
    #[cfg(not(windows))]
    {
        vec![base]
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "019f4a0b-a42e-7103-ac37-2ceffc73cb52";

    /// 创建测试 Codex 目录和一条标准会话，返回临时目录与文件路径。
    fn fixture(session_body: &str) -> (TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("应能创建临时目录");
        let session_directory = directory.path().join("sessions/2026/07/10");
        fs::create_dir_all(&session_directory).expect("应能创建测试会话目录");
        let path =
            session_directory.join(format!("rollout-2026-07-10T11-21-49-{SESSION_ID}.jsonl"));
        fs::write(&path, session_body.as_bytes()).expect("应能写入 UTF-8 测试会话");
        (directory, path)
    }

    /// 返回包含中文 Windows 路径、模型和摘要的有效 JSONL。
    fn valid_session() -> String {
        concat!(
            "{\"timestamp\":\"2026-07-10T03:21:49Z\",\"type\":\"session_meta\",\"payload\":{",
            "\"id\":\"019f4a0b-a42e-7103-ac37-2ceffc73cb52\",",
            "\"timestamp\":\"2026-07-10T03:21:49Z\",",
            "\"cwd\":\"D:\\\\项目\\\\智能助手\",\"source\":\"vscode\",",
            "\"thread_source\":\"user\",\"model_provider\":\"custom\",\"future\":true}}\n",
            "{\"timestamp\":\"2026-07-10T03:22:49Z\",\"type\":\"turn_context\",\"payload\":{",
            "\"model\":\"gpt-5\",\"summary\":\"实现扫描 token=secret-value，保持安全\",",
            "\"unknown_field\":{\"nested\":true}}}\n"
        )
        .to_owned()
    }

    /// 验证正常会话、未知字段、中文路径、索引标题和秘密脱敏。
    #[test]
    fn parses_valid_session_and_index() {
        let (directory, _) = fixture(&valid_session());
        let index = format!(
            "{{\"id\":\"{SESSION_ID}\",\"thread_name\":\"中文标题\",\"updated_at\":\"2026-07-10T03:23:49Z\"}}\n"
        );
        fs::write(
            directory.path().join("session_index.jsonl"),
            index.as_bytes(),
        )
        .expect("应能写入 UTF-8 索引");
        let provider = CodexProvider::with_home(directory.path()).with_cli_program("missing-cli");

        let report = provider.scan_sessions().expect("扫描应成功");
        assert_eq!(report.sessions.len(), 1);
        let session = &report.sessions[0];
        assert_eq!(session.id, SESSION_ID);
        assert_eq!(session.title.as_deref(), Some("中文标题"));
        assert_eq!(session.project, "智能助手");
        assert_eq!(session.model.as_deref(), Some("gpt-5"));
        assert_eq!(session.model_provider.as_deref(), Some("custom"));
        assert_eq!(session.interaction, InteractionType::Interactive);
        assert_eq!(
            session.summary.as_deref(),
            Some("实现扫描 token=[REDACTED]，保持安全")
        );
        assert_eq!(session.resume.state, ResumeState::CliMissing);
    }

    /// 验证损坏 JSONL 行被记录后仍能解析同文件中的有效记录。
    #[test]
    fn skips_corrupt_jsonl_line() {
        let body = format!("{}not-json\n", valid_session());
        let (directory, _) = fixture(&body);
        let provider = CodexProvider::with_home(directory.path());

        let report = provider.scan_sessions().expect("扫描应成功");
        assert_eq!(report.sessions.len(), 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.kind == WarningKind::InvalidRecord)
        );
    }

    /// 验证缺少 metadata 时可从文件名回退 ID，且未知记录不会中断扫描。
    #[test]
    fn handles_missing_and_unknown_fields() {
        let body = concat!(
            "{\"timestamp\":\"2026-07-10T03:21:49Z\",\"type\":\"future_record\",\"payload\":{}}\n",
            "{\"timestamp\":\"2026-07-10T03:22:49Z\",\"type\":\"turn_context\",\"payload\":{}}\n"
        );
        let (directory, _) = fixture(body);
        let provider = CodexProvider::with_home(directory.path());

        let report = provider.scan_sessions().expect("扫描应成功");
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].id, SESSION_ID);
        assert_eq!(report.sessions[0].interaction, InteractionType::Unknown);
    }

    /// 验证非法 UTF-8 行被跳过，其他有效行仍可建立会话。
    #[test]
    fn skips_invalid_utf8_line() {
        let (directory, path) = fixture(&valid_session());
        let mut bytes = fs::read(&path).expect("应能读取测试会话");
        bytes.extend_from_slice(&[0xff, b'\n']);
        fs::write(&path, bytes).expect("应能写入包含坏行的测试数据");
        let provider = CodexProvider::with_home(directory.path());

        let report = provider.scan_sessions().expect("扫描应成功");
        assert_eq!(report.sessions.len(), 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.kind == WarningKind::InvalidUtf8)
        );
    }

    /// 验证扫描过程不会修改原始会话文件。
    #[test]
    fn scan_is_read_only() {
        let (directory, path) = fixture(&valid_session());
        let before = fs::read(&path).expect("应能读取测试会话");
        let provider = CodexProvider::with_home(directory.path());

        provider.scan_sessions().expect("扫描应成功");

        let after = fs::read(&path).expect("应能再次读取测试会话");
        assert_eq!(before, after);
    }

    /// 验证恢复命令严格使用 Codex 官方入口和原始会话 ID。
    #[test]
    fn builds_official_resume_command() {
        let (directory, _) = fixture(&valid_session());
        let provider = CodexProvider::with_home(directory.path());
        let session = provider
            .scan_sessions()
            .expect("扫描应成功")
            .sessions
            .remove(0);

        let command = provider
            .build_resume_command(&session)
            .expect("应能构造恢复命令");
        assert_eq!(command.display(), format!("codex resume {SESSION_ID}"));
    }

    /// 验证异常 payload id 不会覆盖 rollout 文件名中的可信 UUID。
    #[test]
    fn filename_uuid_takes_precedence_over_payload_id() {
        let body = concat!(
            "{\"timestamp\":\"2026-07-10T03:21:49Z\",\"type\":\"session_meta\",\"payload\":{",
            "\"id\":\"unsafe & command\",\"timestamp\":\"2026-07-10T03:21:49Z\",",
            "\"cwd\":\"D:\\\\项目\\\\智能助手\",\"source\":\"vscode\",\"thread_source\":\"user\",",
            "\"model_provider\":\"custom\"}}\n"
        );
        let (directory, _) = fixture(body);
        let provider = CodexProvider::with_home(directory.path());

        let session = provider
            .scan_sessions()
            .expect("扫描应成功")
            .sessions
            .remove(0);

        assert_eq!(session.id, SESSION_ID);
        assert!(
            provider.build_resume_command(&session).is_ok(),
            "文件名 UUID 应可安全构造恢复命令"
        );
    }

    /// 验证无法追溯到 UUID 的会话不会进入官方恢复命令。
    #[test]
    fn rejects_non_uuid_resume_id() {
        let (directory, _) = fixture(&valid_session());
        let provider = CodexProvider::with_home(directory.path());
        let mut session = provider
            .scan_sessions()
            .expect("扫描应成功")
            .sessions
            .remove(0);
        session.id = "unsafe & command".to_owned();

        assert!(matches!(
            provider.build_resume_command(&session),
            Err(ProviderError::InvalidData { .. })
        ));
    }
}
