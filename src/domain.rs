//! 与具体 AI Agent 无关的统一会话领域模型。

use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

/// 标识一个会话来源，例如 `codex`、`claude-code`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SourceId(String);

impl SourceId {
    /// 创建来源标识；调用方应传入稳定、适合命令行筛选的小写名称。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 返回来源标识的字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for SourceId {
    /// 将来源标识输出为命令行可读文本。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 描述会话由交互式界面还是非交互命令创建。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionType {
    /// 可以通过官方交互界面继续的会话。
    Interactive,
    /// 由批处理或无交互命令创建的会话。
    NonInteractive,
    /// 来源未提供足够信息，暂时无法分类。
    Unknown,
}

/// 标识解析会话时发现的问题类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// 文件不是合法 UTF-8 文本。
    InvalidUtf8,
    /// 某一行不是合法的结构化记录。
    InvalidRecord,
    /// 核心字段缺失，已使用兼容回退值。
    MissingField,
    /// 字段存在但类型或内容无效。
    InvalidField,
    /// 文件读取或目录遍历失败。
    Io,
    /// 来源级扫描失败，但未中断其他来源。
    Provider,
}

/// 保存单个会话文件或扫描过程中的非致命警告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanWarning {
    /// 警告类别，便于调用方结构化处理。
    pub kind: WarningKind,
    /// 相关文件；来源级错误可不提供路径。
    pub path: Option<PathBuf>,
    /// JSONL 行号；文件级错误可不提供行号。
    pub line: Option<usize>,
    /// 已脱敏的用户可读说明。
    pub message: String,
}

impl ScanWarning {
    /// 创建新的扫描警告，并由调用方决定是否附带文件和行号。
    pub fn new(kind: WarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: None,
            line: None,
            message: message.into(),
        }
    }

    /// 为警告补充关联文件路径并返回更新后的值。
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// 为警告补充一基 JSONL 行号并返回更新后的值。
    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }
}

/// 描述会话当前能否由来源的官方命令恢复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeState {
    /// 所需会话文件和 Agent CLI 均可用。
    Ready,
    /// Agent CLI 未安装或不在 `PATH` 中。
    CliMissing,
    /// 原始会话文件已经不存在。
    SessionMissing,
    /// 历史模型提供商未在当前配置中定义。
    ProviderMissing,
    /// 当前来源不支持恢复该类会话。
    Unsupported,
    /// 已发现其他会阻止恢复的问题。
    Blocked,
}

/// 包含结构化恢复状态和已脱敏说明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeStatus {
    /// 可供 CLI/TUI 判断的稳定状态。
    pub state: ResumeState,
    /// 面向用户的可选诊断说明。
    pub message: Option<String>,
}

impl ResumeStatus {
    /// 创建可恢复状态。
    pub fn ready() -> Self {
        Self {
            state: ResumeState::Ready,
            message: None,
        }
    }

    /// 创建带说明的不可恢复状态。
    pub fn blocked(state: ResumeState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: Some(message.into()),
        }
    }

    /// 判断当前状态是否允许直接执行恢复命令。
    pub fn is_ready(&self) -> bool {
        self.state == ResumeState::Ready
    }
}

/// 统一表示任意 Agent 来源中的一条本地会话。
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    /// 来源内部使用的稳定会话 ID。
    pub id: String,
    /// 创建该会话的 Agent 来源。
    pub source: SourceId,
    /// 会话标题；通常来自来源自己的索引。
    pub title: Option<String>,
    /// 已截断并脱敏的简短摘要，不包含完整提示词。
    pub summary: Option<String>,
    /// 会话关联的工作目录。
    pub cwd: Option<PathBuf>,
    /// 用于默认分组和筛选的项目名称。
    pub project: String,
    /// 来源记录的创建时间，统一转换为 UTC。
    pub created_at: DateTime<Utc>,
    /// 会话文件或索引记录的最后更新时间。
    pub updated_at: DateTime<Utc>,
    /// 最近一次回合使用的模型。
    pub model: Option<String>,
    /// 历史记录中的模型提供商名称。
    pub model_provider: Option<String>,
    /// 会话的交互类型。
    pub interaction: InteractionType,
    /// 只读扫描得到的原始文件路径。
    pub raw_path: PathBuf,
    /// 仅影响当前会话的解析警告。
    pub warnings: Vec<ScanWarning>,
    /// 当前环境下的恢复检查结果。
    pub resume: ResumeStatus,
}

impl Session {
    /// 从工作目录推导项目名；根目录或缺失目录统一归入 `unknown`。
    pub fn project_from_cwd(cwd: Option<&Path>) -> String {
        cwd.and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("unknown")
            .to_owned()
    }

    /// 构造供模糊搜索使用的文本，包含 ID、标题、摘要、项目和路径。
    pub fn searchable_text(&self) -> String {
        let cwd = self
            .cwd
            .as_deref()
            .map(Path::to_string_lossy)
            .unwrap_or_default();
        format!(
            "{} {} {} {} {}",
            self.id,
            self.title.as_deref().unwrap_or_default(),
            self.summary.as_deref().unwrap_or_default(),
            self.project,
            cwd
        )
    }

    /// 返回标题优先、摘要次之、最后回退到会话 ID 的显示名称。
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.summary.as_deref())
            .unwrap_or(&self.id)
    }
}

/// 汇总一次或多来源扫描得到的会话与非致命警告。
#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanReport {
    /// 成功解析的全部会话。
    pub sessions: Vec<Session>,
    /// 未导致全局失败的扫描与解析问题。
    pub warnings: Vec<ScanWarning>,
}

impl ScanReport {
    /// 合并另一次扫描结果，保留全部会话和警告。
    pub fn extend(&mut self, other: ScanReport) {
        self.sessions.extend(other.sessions);
        self.warnings.extend(other.warnings);
    }
}

/// 表示来源按需读取的安全摘要信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    /// 已截断并脱敏的标题。
    pub title: Option<String>,
    /// 已截断并脱敏的摘要。
    pub summary: Option<String>,
}

/// 描述来源实现具备的功能，供 `sources` 命令展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    /// 支持本地会话扫描。
    Scan,
    /// 支持摘要读取。
    Summary,
    /// 支持恢复会话。
    Resume,
    /// 支持环境诊断。
    Diagnose,
    /// 支持模型提供商兼容别名修复。
    RepairProvider,
}

/// 提供一个 Agent 来源的稳定标识和能力说明。
#[derive(Debug, Clone, Serialize)]
pub struct SourceDescriptor {
    /// 用于筛选和注册表查找的稳定来源 ID。
    pub id: SourceId,
    /// 面向用户的来源名称。
    pub display_name: String,
    /// 当前实现的简短说明。
    pub description: String,
    /// 来源实现支持的功能集合。
    pub capabilities: Vec<ProviderCapability>,
}

/// 描述环境诊断项的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// 检查通过或仅提供信息。
    Info,
    /// 存在兼容性风险，但部分功能仍可使用。
    Warning,
    /// 核心功能不可用。
    Error,
}

/// 表示一个来源环境检查结果。
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    /// 检查项的稳定短名称。
    pub name: String,
    /// 检查严重程度。
    pub severity: DiagnosticSeverity,
    /// 不包含密钥、token 或密码的结果说明。
    pub message: String,
    /// 可选的修复建议。
    pub suggestion: Option<String>,
}

/// 描述恢复或诊断所需执行的外部命令，不通过 shell 重新解析参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// 可执行程序路径或名称。
    pub program: PathBuf,
    /// 按原始参数边界保存的参数列表。
    pub args: Vec<OsString>,
}

impl CommandSpec {
    /// 创建命令描述；调用方负责保证参数不包含秘密。
    pub fn new(program: impl Into<PathBuf>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// 生成仅用于提示和 `--dry-run` 的安全显示文本，执行时仍使用结构化参数。
    pub fn display(&self) -> String {
        let mut parts = vec![quote_argument(&self.program.to_string_lossy())];
        parts.extend(
            self.args
                .iter()
                .map(|argument| quote_argument(&argument.to_string_lossy())),
        );
        parts.join(" ")
    }
}

/// 描述显式 provider 修复请求，确认逻辑由命令层负责。
#[derive(Debug, Clone, Copy, Default)]
pub struct RepairOptions {
    /// 表示用户已通过 `--yes` 或交互确认授权修改配置。
    pub confirmed: bool,
}

/// 汇总 provider 配置修复的路径与结果。
#[derive(Debug, Clone, Serialize)]
pub struct RepairReport {
    /// 被修改的配置文件。
    pub config_path: PathBuf,
    /// 修改前生成的备份文件。
    pub backup_path: Option<PathBuf>,
    /// 新增的历史 provider 兼容别名。
    pub alias: String,
    /// 是否实际写入了新别名。
    pub changed: bool,
    /// 修复完成后的说明。
    pub message: String,
}

/// 为显示文本中的空白或引号添加最小引用，不参与实际进程启动。
fn quote_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| !character.is_whitespace() && character != '"')
    {
        argument.to_owned()
    } else {
        format!("\"{}\"", argument.replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证中文工作目录能够稳定推导项目名。
    #[test]
    fn derives_project_name_from_chinese_path() {
        let path = Path::new(r"C:\工作区\智能助手");
        assert_eq!(Session::project_from_cwd(Some(path)), "智能助手");
    }

    /// 验证命令展示会引用包含空格的参数，但不改变参数边界。
    #[test]
    fn displays_command_with_quoted_arguments() {
        let command = CommandSpec::new(
            "codex",
            vec![OsString::from("resume"), OsString::from("thread name")],
        );
        assert_eq!(command.display(), "codex resume \"thread name\"");
    }
}
