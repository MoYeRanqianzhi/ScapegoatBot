#!/usr/bin/env bash
# 一个发布产物的编译参数（release.yml 的每个构建任务都跑，输出追加到 $GITHUB_ENV）。
#
# 用法：release-env.sh <目标三元组> [x86-64 等级 1–4]
# 输出：每行一个 `名字=值`：
# - SBOT_TARGET：发布文件名里的平台段，也是 `sbot --version` 的 `target:` 一行应有的值，
#   例如 `x86_64_v3-unknown-linux-gnu`（v1 与 aarch64 不带后缀）；
# - RUSTFLAGS：Rust 代码的 CPU 等级、Windows 的静态 C 运行时；
# - CFLAGS_<三元组>：C 依赖（SQLite、mimalloc、ring 的 C 部分）的 CPU 等级；
# - 平台需要的其他变量（macOS 的最低系统版本、aarch64 musl 的 C 编译器）。
#
# 为什么要单独给 C 一份参数：`cc` crate 不会把 rustc 的 `-C target-cpu` 转给 C 编译器，不设
# CFLAGS 的话，v3 / v4 的包里 SQLite 与 mimalloc 仍然按最低等级编译。两份参数都从同一个等级
# 在这里算出来，文件名、Rust 代码、C 代码三者不会对不上。
#
# 等级是 x86-64 psABI 的微架构等级：v1 是目标平台自己的基线（Linux / Windows 是最初的
# x86-64；macOS 是 penryn，Apple 没有出过比它更老的 64 位 Intel 机器），所以 v1 不传任何
# CPU 参数；v2–v4 传 `x86-64-vN`，调优目标仍是 generic，不偏向某一家的 CPU。
# aarch64 只有一档，用 rustc 对该目标的默认 CPU：Linux / Windows 是 ARMv8.0 基线（树莓派
# 3 / 4 这样的机器也能跑），macOS 是 apple-m1。
set -euo pipefail

triple=${1:?usage: release-env.sh <target triple> [x86-64 level]}
level=${2:-}

arch=${triple%%-*}
platform=${triple#*-}

rustflags=()
cflags=""

case "$arch" in
x86_64)
    case "$level" in
    1)
        label=$arch
        ;;
    2 | 3 | 4)
        label=${arch}_v$level
        rustflags+=("-Ctarget-cpu=x86-64-v$level")
        if [ "$platform" = pc-windows-msvc ]; then
            # MSVC 没有 x86-64-vN 的写法，用它自己的 /arch 档位：SSE4.2 ≈ v2，AVX2（连同
            # FMA、BMI）≈ v3，AVX512（F/CD/BW/DQ/VL）= v4。写成 `-arch:` 而不是 `/arch:`：
            # cl 两种都认，而 Git Bash 会把以 `/` 开头的环境变量值当成路径改写。
            case "$level" in
            2) cflags=-arch:SSE4.2 ;;
            3) cflags=-arch:AVX2 ;;
            4) cflags=-arch:AVX512 ;;
            esac
        else
            # GCC ≥ 11 与 Clang ≥ 12（含 Apple Clang）都认识 x86-64-vN。
            cflags=-march=x86-64-v$level
        fi
        ;;
    *)
        echo "x86_64 needs a CPU level from 1 to 4, got '$level'" >&2
        exit 1
        ;;
    esac
    ;;
aarch64)
    if [ -n "$level" ]; then
        echo "aarch64 has no CPU levels, got '$level'" >&2
        exit 1
    fi
    label=$arch
    ;;
*)
    echo "unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

case "$platform" in
unknown-linux-gnu) ;;
unknown-linux-musl)
    if [ "$arch" = aarch64 ]; then
        # `cc` crate 在 aarch64 上找的是 `aarch64-linux-musl-gcc`；Ubuntu 的 musl-tools 只提供
        # `musl-gcc`（x86_64 上 `cc` 自己会退回它）。
        echo "CC_aarch64_unknown_linux_musl=musl-gcc"
    fi
    ;;
pc-windows-msvc)
    # C 运行时静态链接：不依赖 VCRUNTIME140.dll，没装 VC++ 运行库的机器也能跑。`cc` crate
    # 看到这个特性会给 C 代码用 /MT，两边一致。
    rustflags+=("-Ctarget-feature=+crt-static")
    ;;
apple-darwin)
    # 最低 macOS 11：Apple 芯片从 11 起步，Intel 版与它对齐。check-binary.sh 会核对产物
    # 的 minos 不高于这个值（它的 MACOS_MIN 默认值要与这里一致）。
    echo "MACOSX_DEPLOYMENT_TARGET=11.0"
    ;;
*)
    echo "unsupported platform: $platform" >&2
    exit 1
    ;;
esac

echo "SBOT_TARGET=$label-$platform"
if [ ${#rustflags[@]} -gt 0 ]; then
    echo "RUSTFLAGS=${rustflags[*]}"
fi
if [ -n "$cflags" ]; then
    echo "CFLAGS_${triple//-/_}=$cflags"
fi
