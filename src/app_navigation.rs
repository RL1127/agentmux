//! 通过操作系统已注册的 URL 协议打开 Agent 桌面应用页面。

use std::io;

use thiserror::Error;

use crate::domain::{Diagnostic, DiagnosticSeverity};

/// 表示桌面应用导航协议不可用或启动失败。
#[derive(Debug, Error)]
pub enum AppNavigationError {
    /// 当前系统没有可用的 URL 打开入口。
    #[error("当前系统不支持打开 Codex App URL")]
    UnsupportedPlatform,
    /// Windows 未注册 Codex App 的 `codex:` URL 协议。
    #[error("未检测到 Codex App 的 codex: URL 协议")]
    ProtocolUnavailable,
    /// 操作系统无法创建负责打开 URL 的进程。
    #[error("无法打开 Codex App URL: {source}")]
    Spawn {
        /// 底层进程创建错误。
        #[source]
        source: io::Error,
    },
    /// 系统 URL 处理器返回失败状态。
    #[error("Codex App URL 处理器失败: {reason}")]
    Launch {
        /// 不包含用户数据的失败原因。
        reason: String,
    },
}

/// 使用系统 URL 处理器打开 provider 构造的安全 App URI。
pub fn open_uri(uri: &str) -> Result<(), AppNavigationError> {
    platform::open_uri(uri)
}

/// 检查当前平台是否具备 Codex App 导航入口。
pub fn is_available() -> bool {
    platform::is_available()
}

/// 返回 `doctor` 使用的 Codex App URL 协议诊断项。
pub fn diagnostic() -> Diagnostic {
    if is_available() {
        Diagnostic {
            name: "codex-app-navigation".to_owned(),
            severity: DiagnosticSeverity::Info,
            message: "已检测到 Codex App 导航入口".to_owned(),
            suggestion: None,
        }
    } else {
        Diagnostic {
            name: "codex-app-navigation".to_owned(),
            severity: DiagnosticSeverity::Warning,
            message: "未检测到 Codex App 导航入口，CLI 恢复仍可正常使用".to_owned(),
            suggestion: Some(
                "安装或启动 Codex App，或使用 --no-open-in-app 关闭自动导航".to_owned(),
            ),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::iter;
    use std::ptr;

    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use super::AppNavigationError;

    /// 把 Rust UTF-8 字符串转换为以 NUL 结尾的 Windows UTF-16 字符串。
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(iter::once(0)).collect()
    }

    /// 检查指定注册表根节点下是否存在 Codex URL 协议键。
    fn protocol_key_exists(root: HKEY, path: &str) -> bool {
        let path = wide(path);
        let mut key: HKEY = ptr::null_mut();
        let status = unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ, &mut key) };
        if status == 0 {
            unsafe {
                RegCloseKey(key);
            }
            true
        } else {
            false
        }
    }

    /// 检查当前用户或系统类注册表中是否注册了 `codex:` 协议。
    pub(super) fn is_available() -> bool {
        protocol_key_exists(HKEY_CURRENT_USER, r"Software\Classes\codex")
            || protocol_key_exists(HKEY_CLASSES_ROOT, "codex")
    }

    /// 使用 ShellExecuteW 交给 Windows 已注册的 `codex:` 协议处理器。
    pub(super) fn open_uri(uri: &str) -> Result<(), AppNavigationError> {
        if !is_available() {
            return Err(AppNavigationError::ProtocolUnavailable);
        }
        let operation = wide("open");
        let uri = wide(uri);
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                uri.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        let code = result as isize;
        if code > 32 {
            Ok(())
        } else {
            Err(AppNavigationError::Launch {
                reason: format!("ShellExecuteW 返回代码 {code}"),
            })
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;

    use super::AppNavigationError;

    /// macOS 使用系统 `open` 命令处理 Codex App URL。
    pub(super) fn open_uri(uri: &str) -> Result<(), AppNavigationError> {
        let status = Command::new("open")
            .arg(uri)
            .status()
            .map_err(|source| AppNavigationError::Spawn { source })?;
        if status.success() {
            Ok(())
        } else {
            Err(AppNavigationError::Launch {
                reason: format!("open 退出码 {}", status.code().unwrap_or(-1)),
            })
        }
    }

    /// macOS 系统自带 `open`，具体协议是否注册由实际调用结果判断。
    pub(super) fn is_available() -> bool {
        true
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::path::Path;
    use std::process::Command;

    use super::AppNavigationError;

    /// Linux 使用桌面环境通用的 `xdg-open` 处理 URL。
    pub(super) fn open_uri(uri: &str) -> Result<(), AppNavigationError> {
        let status = Command::new("xdg-open")
            .arg(uri)
            .status()
            .map_err(|source| AppNavigationError::Spawn { source })?;
        if status.success() {
            Ok(())
        } else {
            Err(AppNavigationError::Launch {
                reason: format!("xdg-open 退出码 {}", status.code().unwrap_or(-1)),
            })
        }
    }

    /// 检查 PATH 中是否存在桌面 URL 打开命令。
    pub(super) fn is_available() -> bool {
        crate::resume::command_for(Path::new("xdg-open"), &[]).is_some()
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::AppNavigationError;

    /// 未支持的平台明确返回能力不可用。
    pub(super) fn open_uri(_uri: &str) -> Result<(), AppNavigationError> {
        Err(AppNavigationError::UnsupportedPlatform)
    }

    /// 未支持的平台不提供 App 导航入口。
    pub(super) fn is_available() -> bool {
        false
    }
}
