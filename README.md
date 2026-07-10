# agentmux

统一检索、分组、诊断和恢复本地 AI Coding Agent 会话的跨平台 CLI/TUI 工具。

agentmux 当前完整支持 OpenAI Codex，并通过通用 SessionProvider 接口为 Claude Code、Gemini CLI、Cursor 等来源预留扩展能力。

## 特性

- 只读扫描 Codex 当前及归档会话。
- 按项目、日期、来源或模型 provider 分组。
- 按标题、摘要、路径和会话 ID 模糊搜索。
- 提供普通文本和 JSON 输出。
- 提供“目录列表 → 目录内会话”的两级交互式界面。
- 使用官方 codex resume 命令恢复会话。
- 恢复成功后通过系统 `codex:` 协议自动打开对应 Codex App 任务。
- 检查 Codex CLI、配置和会话目录。
- 诊断并显式修复历史 model_provider 别名。
- 支持 Bash、Zsh、Fish 和 PowerShell 补全。
- 单个损坏文件不会中断其他会话扫描。

agentmux 不会修改原始会话文件，也不会读取或输出认证 token、密码和密钥。

## 安装

环境要求：

- Rust 1.85+
- Codex CLI

从源码安装：

    git clone https://github.com/RL1127/agentmux.git
    cd agentmux
    cargo install --path .

或构建 release 版本：

    cargo build --release

Windows 二进制位于 target/release/agentmux.exe，Unix 平台位于 target/release/agentmux。

## 快速开始

进入交互式界面：

    agentmux

列出会话：

    agentmux list

搜索和筛选：

    agentmux list --search agentmux
    agentmux list --project agentmux --since 7d
    agentmux list --provider custom --group-by provider
    agentmux list --include-non-interactive
    agentmux list --json

恢复会话：

    agentmux resume SESSION_ID

仅查看将执行的命令：

    agentmux resume SESSION_ID --dry-run

恢复后不打开 Codex App：

    agentmux resume SESSION_ID --no-open-in-app

检查环境：

    agentmux doctor

列出支持的来源：

    agentmux sources

生成补全：

    agentmux completion powershell
    agentmux completion bash

list 支持以下常用参数：

| 参数 | 说明 |
| --- | --- |
| --source | 按 Agent 来源筛选 |
| --project | 按项目筛选 |
| --provider | 按 model_provider 筛选 |
| --since | 支持 RFC3339、日期或 30m、24h、7d、4w |
| --group-by | project、date、source 或 provider |
| --search | 模糊搜索会话 |
| --json | 输出 JSON |
| --include-non-interactive | 包含 exec、subagent 等会话 |

## TUI 快捷键

| 按键 | 操作 |
| --- | --- |
| 方向键、j、k | 移动选择 |
| / | 搜索当前层级 |
| Enter | 进入目录、结束搜索或在 Codex App 打开会话 |
| c | 在会话层通过官方 Codex CLI 恢复 |
| Esc、左方向键 | 返回目录层；目录层中退出 |
| q | 直接退出 |

第一层只显示完整工作目录，进入目录后才显示会话列表和详情。会话层按 Enter 会立即通过系统 `codex:` 协议切换到 Codex App，不会先启动阻塞终端的 Codex CLI；需要 CLI TUI 时按 c。界面会根据终端宽度自动切换布局，并正确处理中文路径和标题。

## Codex 数据目录

agentmux 优先读取 CODEX_HOME，否则使用用户主目录下的 .codex。扫描范围包括 sessions、archived_sessions 和 session_index.jsonl。

会话扫描始终只读。恢复操作最终执行：

    codex resume SESSION_ID

Codex 子进程继承 stdin、stdout 和 stderr，退出码由 agentmux 原样传递。实际恢复必须在 PowerShell、CMD、Windows Terminal 或其他真实 TTY 中运行。

恢复命令成功结束后，agentmux 默认通过当前 Codex App 注册的 `codex://threads/SESSION_ID` 协议请求系统打开对应任务。该导航属于 best-effort 桌面集成，不会修改 App SQLite；Codex App 未安装、协议不可用或未来版本调整路由时只输出警告，不影响已经成功的 CLI 恢复。使用全局参数 `--no-open-in-app` 可关闭自动导航。

## Provider 修复

当历史会话中的 model_provider 已不在 config.toml 中时，可显式创建兼容别名：

    agentmux resume SESSION_ID --repair-provider

非交互环境需要明确确认：

    agentmux resume SESSION_ID --repair-provider --yes

修复前会备份原配置，写后校验失败时自动回滚。包含 credential、token、password、secret 或 authorization 字段的 provider 配置不会被自动复制。

## 故障排查

- 恢复新版本 Codex 创建的历史会话失败时，先运行 `codex --version` 检查版本，并通过 `npm install -g @openai/codex@latest` 更新 CLI。

- 找不到 Codex CLI：运行 codex --version 和 agentmux doctor。
- 会话未显示：尝试 --include-non-interactive，并通过 --json 查看解析警告。
- 无法恢复：确认直接运行于真实终端，并使用 --dry-run 检查命令。
- 恢复成功但 App 未切换：运行 `agentmux doctor` 检查 `codex-app-navigation`，或使用 `--no-open-in-app` 关闭该能力。
- provider 缺失：确认配置更名关系后再使用 --repair-provider。

## 扩展新的 Agent

新增来源时：

1. 在 src/provider 下创建独立模块。
2. 实现 SessionProvider。
3. 将来源数据转换为统一 Session。
4. 使用来源官方恢复入口构造 CommandSpec。
5. 在 default_registry 中注册 provider。
6. 使用临时目录和模拟 CLI 添加测试。

过滤、分组、JSON 输出和 TUI 不需要针对新来源修改。

## 开发

    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets
    cargo build --release

测试使用隔离临时目录，不会修改用户真实 Codex 会话或配置。

## License

MIT
