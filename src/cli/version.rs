//! `sbot --version` 的构建信息：这个二进制是为哪个平台、哪一级 CPU 编译的，以及本机 CPU
//! 最高能跑哪一级。
//!
//! 发布页对 x86_64 按 x86-64 微架构等级（psABI 的 v1–v4）各出一份：等级越高，编译器能用的
//! 指令越新、越快，但在不支持的 CPU 上一启动就因非法指令崩溃。`target:` 一行与发布文件名里
//! 的平台段一字不差（`sbot-<版本>-<target>.tar.gz`），`cpu:` 一行给出本机能用的最高等级，
//! 使用者（或替他部署的 agent）不用查 CPU 型号就能挑对文件。release workflow 也用 `target:`
//! 核对每个产物的编译参数确实生效。

use std::sync::OnceLock;

/// `sbot --version` 打印的长版本串（`sbot -V` 只打印版本号）。
///
/// clap 要 `&'static str`，内容又要在运行时检测 CPU，所以算一次存起来。
pub fn long_version() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let mut text = format!("{}\ntarget: {}", env!("CARGO_PKG_VERSION"), build_target());
        // 发布构建由 workflow 传入提交号；本地构建没有这一行。
        if let Some(commit) = option_env!("SBOT_BUILD_COMMIT") {
            text.push_str(&format!("\ncommit: {commit}"));
        }
        if let Some(level) = host_level() {
            text.push_str(&format!(
                "\ncpu:    {} (the highest level this CPU supports)",
                arch_label(std::env::consts::ARCH, Some(level))
            ));
        }
        text
    })
}

/// 本二进制在发布文件名里的平台段，例如 `x86_64_v3-unknown-linux-musl`。
fn build_target() -> String {
    format!(
        "{}-{}",
        arch_label(std::env::consts::ARCH, build_level()),
        platform()
    )
}

/// 架构标签：x86_64 的 v2 及以上带 `_vN` 后缀，`level` 为 `None`（非 x86_64）或 1 时不带。
///
/// 与 python-build-standalone 的命名一致：`x86_64-unknown-linux-gnu` 是 v1（任何 x86-64 CPU），
/// `x86_64_v3-unknown-linux-gnu` 要求 AVX2 等。
fn arch_label(arch: &str, level: Option<u8>) -> String {
    match level {
        Some(level) if level >= 2 => format!("{arch}_v{level}"),
        _ => arch.to_owned(),
    }
}

/// 三元组里架构之后的部分。
///
/// 没有构建脚本拿不到编译目标的原文，按 `cfg` 拼出来；只需要覆盖发布的平台，其余平台
/// 退回操作系统名，仍然能看出是什么。
fn platform() -> &'static str {
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        "unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_env = "msvc")) {
        "pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_env = "gnu")) {
        "pc-windows-gnu"
    } else if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        std::env::consts::OS
    }
}

/// 编译时启用的 x86-64 等级（看 `-C target-cpu` 打开了哪些指令集）；非 x86_64 为 `None`。
///
/// 每一级只看它新增的、有代表性的几项：v4 = AVX-512（F/BW/CD/DQ/VL），v3 = AVX2 + BMI2 +
/// FMA，v2 = SSE4.2 + POPCNT + CMPXCHG16B。
fn build_level() -> Option<u8> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }
    let level = if cfg!(all(
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512cd",
        target_feature = "avx512dq",
        target_feature = "avx512vl"
    )) {
        4
    } else if cfg!(all(
        target_feature = "avx2",
        target_feature = "bmi2",
        target_feature = "fma"
    )) {
        3
    } else if cfg!(all(
        target_feature = "sse4.2",
        target_feature = "popcnt",
        target_feature = "cmpxchg16b"
    )) {
        2
    } else {
        1
    };
    Some(level)
}

/// 本机 CPU（连同操作系统对 AVX 寄存器状态的支持）最高能跑的 x86-64 等级；非 x86_64 为
/// `None`。按 psABI 每一级的完整指令集检测。
#[cfg(target_arch = "x86_64")]
fn host_level() -> Option<u8> {
    use std::arch::is_x86_feature_detected as has;

    let v2 = has!("sse3")
        && has!("ssse3")
        && has!("sse4.1")
        && has!("sse4.2")
        && has!("popcnt")
        && has!("cmpxchg16b");
    let v3 = v2
        && has!("avx")
        && has!("avx2")
        && has!("bmi1")
        && has!("bmi2")
        && has!("f16c")
        && has!("fma")
        && has!("lzcnt")
        && has!("movbe");
    let v4 = v3
        && has!("avx512f")
        && has!("avx512bw")
        && has!("avx512cd")
        && has!("avx512dq")
        && has!("avx512vl");
    Some(match (v2, v3, v4) {
        (_, _, true) => 4,
        (_, true, _) => 3,
        (true, _, _) => 2,
        _ => 1,
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn host_level() -> Option<u8> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_labels_match_the_release_file_names() {
        assert_eq!(arch_label("x86_64", Some(1)), "x86_64");
        assert_eq!(arch_label("x86_64", Some(3)), "x86_64_v3");
        assert_eq!(arch_label("aarch64", None), "aarch64");
    }

    /// 能跑起来，就说明本机支持编译时的等级。
    #[test]
    fn this_cpu_supports_the_level_the_test_was_built_for() {
        assert!(host_level() >= build_level());
        assert_eq!(host_level().is_some(), cfg!(target_arch = "x86_64"));
    }

    #[test]
    fn the_long_version_names_the_build() {
        let text = long_version();
        assert!(text.starts_with(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(
            text.contains(&format!("\ntarget: {}", build_target())),
            "{text}"
        );
    }
}
