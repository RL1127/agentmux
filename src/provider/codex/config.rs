//! Codex config.toml 的 provider 诊断、备份、原子修复和回滚。

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::Utc;
use toml_edit::{DocumentMut, Item, Value};

use crate::domain::RepairReport;
use crate::provider::ProviderError;
use crate::resume::command_for;

/// 检查配置中是否存在指定 model_providers 表。
pub(super) fn provider_exists(config_path: &Path, provider: &str) -> Result<bool, ProviderError> {
    if !config_path.is_file() {
        return Ok(false);
    }
    let bytes = fs::read(config_path).map_err(|source| ProviderError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    let document = parse_document(config_path, &bytes)?;
    Ok(document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.contains_key(provider)))
}

/// 为历史 provider 创建当前默认 provider 的兼容别名。
pub(super) fn repair_alias(
    config_path: &Path,
    cli_program: &Path,
    historical_provider: &str,
    confirmed: bool,
) -> Result<RepairReport, ProviderError> {
    repair_alias_with_validator(config_path, historical_provider, confirmed, |codex_home| {
        validate_with_codex(cli_program, codex_home)
    })
}

/// 执行可注入校验器的修复流程，测试不会访问用户真实配置或 CLI。
fn repair_alias_with_validator<F>(
    config_path: &Path,
    historical_provider: &str,
    confirmed: bool,
    validator: F,
) -> Result<RepairReport, ProviderError>
where
    F: FnOnce(&Path) -> Result<bool, String>,
{
    if !confirmed {
        return Err(ProviderError::Config {
            message: "provider 修复尚未确认".to_owned(),
        });
    }
    if !valid_provider_name(historical_provider) {
        return Err(ProviderError::Config {
            message: "历史 provider 名称包含不支持的字符".to_owned(),
        });
    }

    let original = fs::read(config_path).map_err(|source| ProviderError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    let mut document = parse_document(config_path, &original)?;
    if document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.contains_key(historical_provider))
    {
        return Ok(RepairReport {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            alias: historical_provider.to_owned(),
            changed: false,
            message: format!("provider {historical_provider} 已存在，无需修改"),
        });
    }

    let default_provider = document
        .get("model_provider")
        .and_then(Item::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ProviderError::Config {
            message: "config.toml 未定义当前默认 model_provider".to_owned(),
        })?;
    let source_item = document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(&default_provider))
        .cloned()
        .ok_or_else(|| ProviderError::Config {
            message: format!("默认 provider {default_provider} 没有对应配置表"),
        })?;
    if contains_sensitive_fields(&source_item) {
        return Err(ProviderError::Config {
            message:
                "默认 provider 表包含 credential、token 或 secret 字段，拒绝自动复制，请改用环境变量配置"
                    .to_owned(),
        });
    }

    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| ProviderError::Config {
            message: "model_providers 不是可编辑 TOML 表".to_owned(),
        })?;
    providers.insert(historical_provider, source_item);
    let rendered = document.to_string();
    rendered
        .parse::<DocumentMut>()
        .map_err(|_| ProviderError::Config {
            message: "修复后的 TOML 无法重新解析".to_owned(),
        })?;

    let backup_path = write_sibling_file(config_path, "backup", "bak", &original)
        .map_err(|source| repair_failure(format!("无法创建配置备份: {source}"), None))?;
    let temp_path =
        write_sibling_file(config_path, "write", "tmp", rendered.as_bytes()).map_err(|source| {
            repair_failure(
                format!("无法写入同目录临时配置: {source}"),
                Some(backup_path.clone()),
            )
        })?;
    let temp_guard = TempFileGuard::new(temp_path.clone());

    let temp_bytes = fs::read(&temp_path).map_err(|source| {
        repair_failure(
            format!("无法重新读取临时配置: {source}"),
            Some(backup_path.clone()),
        )
    })?;
    parse_document(&temp_path, &temp_bytes).map_err(|error| {
        repair_failure(
            format!("临时配置校验失败: {error}"),
            Some(backup_path.clone()),
        )
    })?;
    atomic_replace(&temp_path, config_path).map_err(|source| {
        repair_failure(
            format!("无法原子替换配置: {source}"),
            Some(backup_path.clone()),
        )
    })?;
    drop(temp_guard);

    let codex_home = config_path.parent().unwrap_or_else(|| Path::new("."));
    let validation = validate_installed_config(config_path).and_then(|_| validator(codex_home));
    let validator_ran = match validation {
        Ok(ran) => ran,
        Err(message) => {
            let rollback = restore_backup(config_path, &backup_path, &original);
            let message = match rollback {
                Ok(()) => format!("{message}，已自动恢复原配置"),
                Err(error) => format!(
                    "{message}，自动回滚失败: {error}；请使用备份 {}",
                    backup_path.display()
                ),
            };
            return Err(repair_failure(message, Some(backup_path)));
        }
    };
    Ok(RepairReport {
        config_path: config_path.to_path_buf(),
        backup_path: Some(backup_path),
        alias: historical_provider.to_owned(),
        changed: true,
        message: if validator_ran {
            format!("已创建 provider 兼容别名 {historical_provider}，Codex 严格配置校验通过")
        } else {
            format!(
                "已创建 provider 兼容别名 {historical_provider}；Codex CLI 不可用，已完成 TOML 重解析校验"
            )
        },
    })
}

/// 创建始终说明备份状态的修复错误，避免失败时遗漏人工恢复位置。
fn repair_failure(message: String, backup_path: Option<PathBuf>) -> ProviderError {
    let backup_message = backup_path
        .as_deref()
        .map(|path| format!("；配置备份: {}", path.display()))
        .unwrap_or_else(|| "；配置备份未创建".to_owned());
    ProviderError::Repair {
        message: format!("{message}{backup_message}"),
        backup_path,
    }
}

/// 解析严格 UTF-8 TOML；读取时兼容已有 BOM，但写入始终不生成 BOM。
fn parse_document(config_path: &Path, bytes: &[u8]) -> Result<DocumentMut, ProviderError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ProviderError::Config {
        message: format!("{} 不是合法 UTF-8", config_path.display()),
    })?;
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .parse::<DocumentMut>()
        .map_err(|_| ProviderError::Config {
            message: format!("{} 不是合法 TOML", config_path.display()),
        })
}

/// 验证 provider 名称适合 TOML 键和 Codex 命令行配置路径。
fn valid_provider_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// 递归检查 provider 表是否包含禁止自动复制的凭据字段。
fn contains_sensitive_fields(item: &Item) -> bool {
    match item {
        Item::Table(table) => table
            .iter()
            .any(|(key, value)| sensitive_key(key) || contains_sensitive_fields(value)),
        Item::ArrayOfTables(tables) => tables.iter().any(|table| {
            table
                .iter()
                .any(|(key, value)| sensitive_key(key) || contains_sensitive_fields(value))
        }),
        Item::Value(value) => contains_sensitive_value(value),
        Item::None => false,
    }
}

/// 递归检查数组和内联表中的凭据字段。
fn contains_sensitive_value(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_sensitive_value),
        Value::InlineTable(table) => table
            .iter()
            .any(|(key, value)| sensitive_key(key) || contains_sensitive_value(value)),
        _ => false,
    }
}

/// 判断 TOML 键是否表示 token、密码、私钥或其他认证材料。
fn sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "authorization"
            | "password"
            | "secret"
            | "token"
            | "api_key"
            | "access_key"
            | "private_key"
            | "credential"
            | "credentials"
    ) || normalized.ends_with("_token")
        || normalized.ends_with("_password")
        || normalized.ends_with("_secret")
}

/// 在配置同目录创建唯一文件并同步内容，避免跨卷替换和编码变化。
fn write_sibling_file(
    config_path: &Path,
    purpose: &str,
    extension: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = config_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("config.toml"));
    for attempt in 0..32 {
        let mut candidate_name = file_name.to_os_string();
        candidate_name.push(format!(
            ".agentmux-{purpose}-{}-{}-{attempt}.{extension}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let candidate = parent.join(candidate_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "无法创建唯一同目录临时文件",
    ))
}

/// 校验替换后的配置仍是 UTF-8 TOML。
fn validate_installed_config(config_path: &Path) -> Result<(), String> {
    let bytes = fs::read(config_path).map_err(|_| "无法读取替换后的配置".to_owned())?;
    parse_document(config_path, &bytes)
        .map(|_| ())
        .map_err(|_| "替换后的配置无法重新解析".to_owned())
}

/// 调用可用的 Codex 严格配置检查；CLI 缺失时返回 false。
fn validate_with_codex(cli_program: &Path, codex_home: &Path) -> Result<bool, String> {
    let args = [
        OsString::from("--strict-config"),
        OsString::from("features"),
        OsString::from("list"),
    ];
    let Some(mut command) = command_for(cli_program, &args) else {
        return Ok(false);
    };
    let status = command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "无法启动 Codex 严格配置校验".to_owned())?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!(
            "Codex 严格配置校验返回非零退出码 {}",
            status.code().unwrap_or(-1)
        ))
    }
}

/// 使用保留的备份副本原子恢复原配置，并验证字节完全一致。
fn restore_backup(
    config_path: &Path,
    backup_path: &Path,
    expected_original: &[u8],
) -> io::Result<()> {
    let backup = fs::read(backup_path)?;
    if backup != expected_original {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "备份内容与原配置不一致",
        ));
    }
    let rollback_path = write_sibling_file(config_path, "rollback", "tmp", &backup)?;
    let rollback_guard = TempFileGuard::new(rollback_path.clone());
    atomic_replace(&rollback_path, config_path)?;
    drop(rollback_guard);
    let restored = fs::read(config_path)?;
    if restored != expected_original {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "回滚后的配置与原配置不一致",
        ));
    }
    Ok(())
}

/// 在 Unix 上使用同目录 rename 原子替换现有配置。
#[cfg(not(windows))]
fn atomic_replace(replacement: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(replacement, destination)
}

/// 在 Windows 上使用 ReplaceFileW 原子替换并请求写穿缓存。
#[cfg(windows)]
fn atomic_replace(replacement: &Path, destination: &Path) -> io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let replacement_wide = replacement
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: 两个路径均为以 NUL 结尾且在调用期间保持有效的 UTF-16 缓冲区，
    // 其余可选指针按 Windows API 契约传入空值。
    let result = unsafe {
        ReplaceFileW(
            destination_wide.as_ptr(),
            replacement_wide.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 在作用域结束时清理尚未被原子移动的临时文件。
struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    /// 创建指向同目录临时文件的清理守卫。
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFileGuard {
    /// 尝试删除残留临时文件；已经原子移动时删除会自然忽略不存在。
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// 返回包含注释、顺序和尾随注释的标准测试配置。
    fn valid_config() -> String {
        concat!(
            "# 顶部注释\n",
            "model_provider = \"current\"\n",
            "model = \"gpt-5\"\n",
            "\n",
            "[model_providers.current] # 当前 provider\n",
            "name = \"Current\"\n",
            "base_url = \"https://example.invalid/v1\"\n",
            "wire_api = \"responses\"\n",
            "requires_openai_auth = true\n",
            "\n",
            "[features]\n",
            "example = true # 保留尾随注释\n"
        )
        .to_owned()
    }

    /// 在临时目录中以 UTF-8 无 BOM 写入 config.toml。
    fn fixture_config(content: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("应能创建临时目录");
        let path = directory.path().join("config.toml");
        fs::write(&path, content.as_bytes()).expect("应能写入 UTF-8 测试配置");
        (directory, path)
    }

    /// 返回配置目录中残留的 agentmux 临时文件。
    fn temporary_files(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("应能读取测试目录")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".agentmux-") && name.ends_with(".tmp"))
            })
            .collect()
    }

    /// 验证 provider 存在性诊断区分已配置和历史缺失名称。
    #[test]
    fn diagnoses_provider_presence() {
        let (_directory, path) = fixture_config(&valid_config());
        assert!(provider_exists(&path, "current").expect("诊断应成功"));
        assert!(!provider_exists(&path, "legacy").expect("诊断应成功"));
    }

    /// 验证修复保留 TOML 注释和顺序，写入 UTF-8 无 BOM，并保留原始备份。
    #[test]
    fn repairs_alias_preserving_format_and_utf8() {
        let original = valid_config();
        let (directory, path) = fixture_config(&original);

        let report =
            repair_alias_with_validator(&path, "legacy", true, |_| Ok(true)).expect("修复应成功");

        assert!(report.changed);
        assert_eq!(report.alias, "legacy");
        let backup_path = report.backup_path.expect("成功修复应生成备份");
        assert_eq!(
            fs::read(&backup_path).expect("应能读取备份"),
            original.as_bytes()
        );
        let bytes = fs::read(&path).expect("应能读取修复配置");
        assert!(!bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        let text = std::str::from_utf8(&bytes).expect("修复配置应为 UTF-8");
        assert!(text.contains("# 顶部注释"));
        assert!(text.contains("# 当前 provider"));
        assert!(text.contains("# 保留尾随注释"));
        assert!(text.contains("[model_providers.legacy]"));
        let document = parse_document(&path, &bytes).expect("修复配置应为合法 TOML");
        assert_eq!(
            document["model_providers"]["legacy"]["base_url"].as_str(),
            Some("https://example.invalid/v1")
        );
        assert!(temporary_files(directory.path()).is_empty());
    }

    /// 验证 Codex 校验失败会从备份自动回滚且保留备份位置。
    #[test]
    fn rolls_back_when_validator_fails() {
        let original = valid_config();
        let (directory, path) = fixture_config(&original);

        let error = repair_alias_with_validator(&path, "legacy", true, |_| {
            Err("模拟 Codex 校验失败".to_owned())
        })
        .expect_err("校验失败应触发回滚");
        let error_text = error.to_string();

        let backup_path = match error {
            ProviderError::Repair {
                backup_path: Some(path),
                ..
            } => path,
            other => panic!("应返回带备份路径的修复错误: {other}"),
        };
        assert!(error_text.contains(&backup_path.display().to_string()));
        assert_eq!(
            fs::read(&path).expect("应能读取回滚配置"),
            original.as_bytes()
        );
        assert_eq!(
            fs::read(&backup_path).expect("回滚后备份应继续存在"),
            original.as_bytes()
        );
        assert!(temporary_files(directory.path()).is_empty());
    }

    /// 创建校验参数正确时返回成功的模拟 Codex CLI。
    fn fake_codex_validator(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let path = directory.join("fake-codex.cmd");
            let body = concat!(
                "@echo off\r\n",
                "if not \"%1\"==\"--strict-config\" exit /b 11\r\n",
                "if not \"%2\"==\"features\" exit /b 12\r\n",
                "if not \"%3\"==\"list\" exit /b 13\r\n",
                "if \"%CODEX_HOME%\"==\"\" exit /b 14\r\n",
                "exit /b 0\r\n"
            );
            fs::write(&path, body.as_bytes()).expect("应能写入 UTF-8 模拟 Codex 批处理");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let path = directory.join("fake-codex");
            let body = concat!(
                "#!/bin/sh\n",
                "test \"$1\" = \"--strict-config\" || exit 11\n",
                "test \"$2\" = \"features\" || exit 12\n",
                "test \"$3\" = \"list\" || exit 13\n",
                "test -n \"$CODEX_HOME\" || exit 14\n",
                "exit 0\n"
            );
            fs::write(&path, body.as_bytes()).expect("应能写入 UTF-8 模拟 Codex 脚本");
            let mut permissions = fs::metadata(&path)
                .expect("应能读取模拟 Codex 权限")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("应能设置模拟 Codex 执行权限");
            path
        }
    }

    /// 验证成功修复会调用 Codex 严格配置检查及其只读 features list 子命令。
    #[test]
    fn runs_codex_strict_config_validation() {
        let original = valid_config();
        let (directory, path) = fixture_config(&original);
        let program = fake_codex_validator(directory.path());

        let report = repair_alias(&path, &program, "legacy", true).expect("严格配置校验应通过");

        assert!(report.changed);
        assert!(report.message.contains("Codex 严格配置校验通过"));
    }

    /// 验证未确认修复不会创建备份或修改配置。
    #[test]
    fn refuses_unconfirmed_repair() {
        let original = valid_config();
        let (directory, path) = fixture_config(&original);

        let result = repair_alias_with_validator(&path, "legacy", false, |_| Ok(true));

        assert!(matches!(result, Err(ProviderError::Config { .. })));
        assert_eq!(
            fs::read(&path).expect("应能读取原配置"),
            original.as_bytes()
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("应能读取目录")
                .count(),
            1
        );
    }

    /// 验证已存在别名时不重复备份或写入。
    #[test]
    fn keeps_existing_alias_unchanged() {
        let config = format!(
            "{}\n[model_providers.legacy]\nname = \"Legacy\"\nbase_url = \"https://legacy.invalid\"\nwire_api = \"responses\"\n",
            valid_config()
        );
        let (_directory, path) = fixture_config(&config);

        let report = repair_alias_with_validator(&path, "legacy", true, |_| {
            panic!("别名已存在时不应调用校验器")
        })
        .expect("已存在别名应返回成功结果");

        assert!(!report.changed);
        assert!(report.backup_path.is_none());
        assert_eq!(fs::read(&path).expect("应能读取原配置"), config.as_bytes());
    }

    /// 验证包含认证材料字段的 provider 表不会被自动复制。
    #[test]
    fn refuses_to_copy_sensitive_fields() {
        let config = concat!(
            "model_provider = \"current\"\n",
            "[model_providers.current]\n",
            "name = \"Current\"\n",
            "authorization = \"Bearer must-not-copy\"\n"
        );
        let (directory, path) = fixture_config(config);

        let result = repair_alias_with_validator(&path, "legacy", true, |_| Ok(true));

        assert!(matches!(result, Err(ProviderError::Config { .. })));
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("应能读取目录")
                .count(),
            1
        );
    }

    /// 验证非法 UTF-8 配置被结构化拒绝。
    #[test]
    fn rejects_invalid_utf8_config() {
        let directory = tempfile::tempdir().expect("应能创建临时目录");
        let path = directory.path().join("config.toml");
        fs::write(&path, [0xff, 0xfe]).expect("应能写入非法 UTF-8 测试数据");

        assert!(matches!(
            provider_exists(&path, "legacy"),
            Err(ProviderError::Config { .. })
        ));
    }
}
