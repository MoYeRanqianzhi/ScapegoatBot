//! Windows 命名管道服务端。
//!
//! 设计依据：设计 §2.2。
//!
//! - 端点 `\\.\pipe\<name>`。
//! - 单实例由 `first_pipe_instance(true)` 保证：第二个进程创建同名管道会得到
//!   `ERROR_ACCESS_DENIED`，而不是静默共享。
//! - DACL 显式限定为 SYSTEM / Administrators / 当前用户（`OW`），
//!   不使用进程默认 DACL，避免被同机其他用户连接。

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::watch;
use tracing::{debug, warn};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

use super::Handler;

/// 管道 DACL：SY = SYSTEM，BA = Administrators，OW = 对象所有者（创建者）。
/// 只授予 GENERIC_ALL；不授予 Everyone / Authenticated Users。
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

/// 创建一条管道实例。
///
/// 安全描述符由 SDDL 转换得到；内核在 `CreateNamedPipeW` 时复制它，
/// 因此创建完成后立即释放本地内存是安全的。
fn create_instance(pipe: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let wide: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();

    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: 传入的字符串以 NUL 结尾；输出指针与大小指针都指向栈上有效内存。
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    let mut options = ServerOptions::new();
    // 第一条实例占位；后续实例由同一个进程在连接后循环创建。
    options.first_pipe_instance(first);

    // SAFETY: `attrs` 指向有效的 SECURITY_ATTRIBUTES，且在调用期间保持存活。
    let result = unsafe {
        options.create_with_security_attributes_raw(pipe, &mut attrs as *mut _ as *mut c_void)
    };

    // SAFETY: descriptor 由 ConvertStringSecurityDescriptor... 通过 LocalAlloc 分配。
    unsafe {
        LocalFree(descriptor as _);
    }

    result
}

/// 接受连接，直到 `shutdown` 变为 `true`。
pub async fn serve(
    pipe: &str,
    handler: Arc<dyn Handler>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut first = true;

    loop {
        if *shutdown.borrow() {
            debug!("ipc server shutting down");
            return Ok(());
        }

        let server = match create_instance(pipe, first) {
            Ok(server) => server,
            Err(error) if first => {
                // 第一条实例就失败，通常意味着已有守护进程在运行。
                return Err(anyhow::anyhow!(
                    "cannot create pipe {pipe}: {error}. Another sbot instance may already be running"
                ));
            }
            Err(error) => {
                // 后续实例创建失败（例如瞬时资源不足）：短暂退避后重试，不终止服务。
                warn!(%error, "cannot create next pipe instance; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        first = false;

        tokio::select! {
            result = server.connect() => {
                result.context("named pipe connect failed")?;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
        }

        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            super::handle_connection(server, handler).await;
        });
    }
}
