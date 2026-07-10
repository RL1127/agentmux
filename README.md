# agentmux

agentmux 是一个跨平台 Rust CLI/TUI 工具，用于统一检索、分组、诊断和恢复本地 AI Coding Agent 会话。

当前版本完整支持 OpenAI Codex。本地会话由 agentmux 只读扫描，恢复操作始终委托给官方 codex resume 命令，不会重放、拼接或改写会话内容。核心查询和界面只依赖通用 SessionProvider 接口，后续可以接入 Claude Code、Gemini CLI、Cursor 等来源。

## 功能

- 扫描 Codex 当前会话和 archived_sessions 中的归档会话。
- 从 session_index.jsonl 读取标题，从 JSONL 元数据读取目录、模型、provider 和交互类型。
- 按项目、日期、来源或模型 provider 分组，默认按更新时间倒序。
- 按标题、安全摘要、工作目录和会话 ID 模糊搜索。
- 默认隐藏 exec、subagent 等非交互会话，可显式包含。
- 使用 ratatui 和 crossterm 提供响应式 TUI。
- 使用官方 codex resume SESSION_ID 恢复，继承真实终端的 stdin、stdout 和 stderr。
- 在恢复前诊断历史 model_provider，并可显式创建兼容别名。
- 输出环境诊断、来源能力和 shell 补全。
- 损坏文件或坏 JSONL 行不会中断其他会话扫描。

agentmux 不会在普通列表中读取或展示完整提示词。标题和摘要会限制长度，并对常见 token、密码和密钥形式做脱敏。

## 环境要求

- Rust 1.85 或更高版本。
- Codex CLI 已安装并可通过 PATH 找到。
- 交互恢复需要 PowerShell、CMD、Windows Terminal 或其他真实 TTY。

已在 Windows 上验证 npm 安装产生的 codex.cmd 包装器。Unix 平台直接执行 PATH 中的 Codex 可执行文件。

## 安装

从源码安装到 Cargo bin 目录：

    git clone https://github.com/RL1127/agentmux.git
    cd agentmux
    cargo install --path .

也可以直接使用 release 构建：

    cargo build --release
    .\target\release\agentmux.exe --help

Unix 平台的二进制路径为 target/release/agentmux。

## 命令

不带子命令时进入交互式会话选择界面：

    agentmux

列出会话：

    agentmux list
    agentmux list --project agentmux
    agentmux list --source codex --provider custom
    agentmux list --since 7d --group-by date
    agentmux list --search "中文项目"
    agentmux list --include-non-interactive
    agentmux list --json

since 支持 RFC3339、YYYY-MM-DD 以及 30m、24h、7d、4w 等相对时间。

恢复会话：

    agentmux resume 019f4a53-ac29-72c2-bed9-d71a8aa34390

只查看将执行的官方命令：

    agentmux resume 019f4a53-ac29-72c2-bed9-d71a8aa34390 --dry-run

检查环境：

    agentmux doctor
    agentmux doctor --json

列出已支持来源：

    agentmux sources
    agentmux sources --json

生成 shell 补全：

    agentmux completion bash
    agentmux completion zsh
    agentmux completion fish
    agentmux completion powershell

completion 将脚本写到 stdout，可按对应 shell 的补全目录和加载方式安装。

## list 参数

| 参数 | 说明 |
| --- | --- |
| --source SOURCE | 按 Agent 来源筛选 |
| --project PROJECT | 按工作目录末级项目名筛选 |
| --provider PROVIDER | 按历史 model_provider 筛选 |
| --since TIME | 按更新时间下限筛选 |
| --group-by project\|date\|source\|provider | 选择分组维度 |
| --search TEXT | 模糊搜索标题、摘要、路径和会话 ID |
| --json | 输出稳定 JSON 分组结构和解析警告 |
| --include-non-interactive | 包含 exec、subagent 等非交互会话 |

## TUI 快捷键

| 按键 | 操作 |
| --- | --- |
| 方向键、j、k | 移动选择 |
| / | 进入实时模糊搜索 |
| Enter | 结束搜索，或恢复选中会话 |
| Tab | 循环切换项目、日期、来源和 provider 分组 |
| q、Esc | 退出；搜索模式下 Esc 先结束搜索 |

宽终端使用左右布局，窄终端自动切换为上下布局。中文路径按终端显示宽度截断，不按 UTF-8 字节截断。

## Codex 数据目录

agentmux 按以下优先级确定 Codex 主目录：

1. CODEX_HOME 环境变量。
2. 平台用户主目录下的 .codex。

使用的文件和目录：

| 路径 | 用途 |
| --- | --- |
| CODEX_HOME/sessions | 当前 JSONL 会话 |
| CODEX_HOME/archived_sessions | 归档 JSONL 会话 |
| CODEX_HOME/session_index.jsonl | 会话标题和更新时间索引 |
| CODEX_HOME/config.toml | model_provider 诊断和显式修复 |

扫描操作不会修改、移动或删除上述会话文件。

## provider 诊断与修复

Codex 会话历史会记录 model_provider。恢复前，agentmux 检查 config.toml 的 model_providers 表是否仍包含该名称。

只诊断，不修改：

    agentmux resume SESSION_ID --dry-run
    agentmux doctor

当历史 provider 缺失时，显式创建兼容别名：

    agentmux resume SESSION_ID --repair-provider

非交互环境必须明确确认：

    agentmux resume SESSION_ID --repair-provider --yes

修复流程：

1. 严格按 UTF-8 和 TOML 解析 config.toml。
2. 确认当前默认 model_provider 有对应配置表。
3. 拒绝复制包含 credential、token、password、secret、authorization 或私钥字段的 provider 表。
4. 在同目录创建原配置的精确备份。
5. 使用 toml_edit 复制默认 provider，保留原有注释、顺序和格式。
6. 将 UTF-8 无 BOM 内容写入同目录临时文件并重新解析。
7. 使用平台原子替换更新 config.toml。
8. 若 Codex CLI 可用，执行 codex --strict-config features list。
9. 任一写后校验失败时，从备份自动回滚；错误中始终给出备份状态或位置。

--repair-provider 与 --dry-run 不能同时使用。TUI 不会自动修改配置，provider 缺失时请退出后使用显式 CLI 修复。

## 恢复与退出码

agentmux 只执行来源 provider 构造的官方命令。Codex 会话对应：

    codex resume SESSION_ID

实际恢复前会再次检查：

- 会话文件仍然存在。
- Codex CLI 可用。
- 历史 model_provider 仍然有效。
- 当前 stdin、stdout 和 stderr 都连接到真实终端。

Codex 子进程继承三个标准流。Codex 正常或失败退出后，其退出码由 agentmux 原样传递。被信号或平台异常终止且没有退出码时，agentmux 返回错误。

## 风险说明

- 会话扫描是只读操作，agentmux 不会修复或整理原始 JSONL。
- config.toml 只有在显式传入 --repair-provider 并确认后才会修改。
- 配置备份是原文件的精确副本，应按与 config.toml 相同的权限保护。
- agentmux 不读取 auth.json，也不会输出认证 token、密码或密钥。
- JSON 输出包含统一模型中的本地路径，适合本机自动化，不应直接发布到不可信环境。
- 最终能否恢复归档或旧版本会话由官方 Codex CLI 决定。

## 故障排查

### 未找到 Codex CLI

确认以下命令可运行：

    codex --version
    codex resume --help

然后运行：

    agentmux doctor

### 非交互环境拒绝恢复

CI、重定向管道和普通子进程通常不是 TTY。请在 PowerShell、CMD、Windows Terminal 或其他交互终端直接运行 agentmux resume。

dry-run 不要求 TTY：

    agentmux resume SESSION_ID --dry-run

### 会话没有显示

- 使用 agentmux list --include-non-interactive 检查是否为 exec 或 subagent 会话。
- 使用 agentmux list --json 查看结构化解析警告。
- 检查 CODEX_HOME 是否指向正确目录。
- 损坏文件会被跳过，不会阻止其他会话显示。

### provider 缺失

先运行：

    agentmux doctor
    agentmux resume SESSION_ID --dry-run

确认历史 provider 确实只是更名后，再使用 --repair-provider。agentmux 不会覆盖已经存在的同名 provider。

### config.toml 校验失败

运行：

    codex --strict-config features list

修复失败时查看 agentmux 输出的备份路径。自动回滚成功后，原配置和备份都会保留。

## 架构

主要模块：

| 模块 | 职责 |
| --- | --- |
| src/domain.rs | 统一 Session、诊断、恢复和修复模型 |
| src/provider/mod.rs | SessionProvider trait 与 ProviderRegistry |
| src/provider/codex.rs | Codex 会话目录、JSONL、索引和恢复命令 |
| src/provider/codex/config.rs | Codex provider 诊断、备份、原子修复和回滚 |
| src/catalog.rs | 来源无关的过滤、搜索、排序和分组 |
| src/tui.rs | 来源无关的 ratatui 交互界面 |
| src/resume.rs | TTY 检查、结构化进程启动和退出码传递 |
| src/app.rs | 命令分发和默认 provider 注册 |

SessionProvider 要求来源实现以下能力：

- descriptor：来源标识和能力。
- scan_sessions：只读扫描并隔离单文件错误。
- read_summary：按需返回安全标题或摘要。
- check_resume：检查当前恢复条件。
- build_resume_command：构造官方恢复命令。
- diagnose：返回结构化环境诊断。
- repair_model_provider：可选的 provider 兼容修复。

## 接入 Claude Code

新增 Claude Code provider 时：

1. 在 src/provider 下新增独立模块，例如 claude.rs。
2. 实现 SessionProvider，不把 Claude 文件格式散落到 catalog、TUI 或 app。
3. 将 Claude 会话转换为统一 Session，并对坏文件返回 ScanWarning。
4. 只通过 Claude 官方恢复入口构造 CommandSpec。
5. 在 default_registry 中注册 provider。
6. 添加临时目录、模拟 CLI 和未知字段兼容测试。

完成注册后，list 的过滤/分组/JSON、TUI 搜索和恢复流程无需修改。当前限制是尚未实现 Claude Code 的本地格式解析、诊断和官方恢复命令适配。

## 开发与验证

运行定向和全量检查：

    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets
    cargo build --release

测试只使用 tempfile 创建隔离会话、配置和模拟可执行文件，不会修改用户真实 Codex 数据。
