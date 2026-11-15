#!/usr/bin/env bash
# 发布产物的链接检查（release.yml 的每个构建任务都跑）。
#
# 用法：check-binary.sh <二进制路径> <平台段，例如 x86_64_v3-unknown-linux-gnu>
#
# 按平台核对“拿到别的机器上能不能跑”：
# - linux-gnu：动态依赖只能是 glibc 自己的库（没有 libssl 之类发行版之间不通用的库），
#   用到的 glibc 符号版本不超过 GLIBC_MAX（默认 2.28，与 manylinux_2_28 容器一致）；
# - linux-musl：完全静态，没有任何动态依赖；
# - apple-darwin：只链接系统库，最低系统版本不高于 MACOS_MIN（默认 11.0，与
#   release-env.sh 设的 MACOSX_DEPLOYMENT_TARGET 一致）；
# - pc-windows-msvc：C 运行时静态链接，不依赖 VCRUNTIME140.dll / UCRT（没装 VC++ 运行库
#   的机器也能跑）。
#
# 写法上的约定：工具的输出先整段存进变量，再用 here-string 喂给 grep / awk。不写成
# `工具 | grep -q`：grep 找到第一处就退出，上游可能被 SIGPIPE 杀掉，在 pipefail 下整条管道
# 算失败，`if` 就走到“没找到”那一支——恰好把该报错的情况放过去。
set -euo pipefail

bin=$1
target=$2
glibc_max=${GLIBC_MAX:-2.28}
macos_min=${MACOS_MIN:-11.0}

fail() {
    echo "::error::$bin: $*" >&2
    exit 1
}

# `version_le a b`：版本号 a ≤ b。
version_le() {
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" = "$2" ]
}

[ -f "$bin" ] || fail "no such file"

case "$target" in
*-unknown-linux-gnu)
    dynamic=$(readelf -d "$bin")
    needed=$(sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' <<<"$dynamic")
    echo "shared libraries: $(tr '\n' ' ' <<<"$needed")"
    for lib in $needed; do
        case "$lib" in
        libc.so.6 | libm.so.6 | libgcc_s.so.1 | libpthread.so.0 | libdl.so.2 | librt.so.1 | ld-linux-*.so.*) ;;
        *) fail "depends on $lib, which is not part of glibc" ;;
        esac
    done
    # 静态程序没有动态符号表（objdump 报错）；有符号表却没有任何 GLIBC_ 符号，同样说明这
    # 不是动态链接 glibc 的程序，与平台段对不上。
    symbols=$(objdump -T "$bin") || fail "not a dynamically linked program"
    glibc=$(grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' <<<"$symbols" | sed 's/^GLIBC_//' | sort -uV | tail -n1 || true)
    [ -n "$glibc" ] || fail "no versioned glibc symbols; is this really a glibc build?"
    echo "highest glibc symbol version: $glibc (limit $glibc_max)"
    version_le "$glibc" "$glibc_max" || fail "needs glibc $glibc, newer than $glibc_max"
    ;;
*-unknown-linux-musl)
    # 静态 PIE 也有 .dynamic 段，但里面没有 NEEDED；`readelf -d` 对完全没有动态段的文件
    # 只打印一句说明，同样不含 NEEDED。
    dynamic=$(readelf -d "$bin")
    if grep -q '(NEEDED)' <<<"$dynamic"; then
        grep '(NEEDED)' <<<"$dynamic"
        fail "the musl build must be fully static"
    fi
    # 有解释器（PT_INTERP）就不是静态程序：要靠目标机器上的 ld-musl 才能启动。
    headers=$(readelf -l "$bin")
    if grep -q 'INTERP' <<<"$headers"; then
        fail "the musl build requests a program interpreter"
    fi
    echo "fully static"
    ;;
*-apple-darwin)
    libraries=$(otool -L "$bin")
    echo "$libraries"
    # 第一行是文件名本身；其余每行一个依赖，只允许系统目录。
    outside=$(tail -n +2 <<<"$libraries" | grep -vE '^[[:space:]]+/(usr/lib|System/Library)/' || true)
    [ -z "$outside" ] || fail "links libraries outside the system: $outside"
    commands=$(otool -l "$bin")
    # LC_BUILD_VERSION 之后的第一个 `minos`；不用 `exit` 提前结束，awk 把输入读完。
    minos=$(awk '/LC_BUILD_VERSION/ { found = 1 } found && !done && $1 == "minos" { print $2; done = 1 }' <<<"$commands")
    [ -n "$minos" ] || fail "no LC_BUILD_VERSION load command"
    echo "minimum macOS: $minos (limit $macos_min)"
    version_le "$minos" "$macos_min" || fail "requires macOS $minos, newer than $macos_min"
    ;;
*-pc-windows-msvc)
    # dumpbin 在 Visual Studio 里，用 vswhere 找。ARM64 机器优先用原生的那一份，找不到再用
    # x64 的（在 ARM64 上靠模拟运行）；x64 机器只能用 x64 的。
    vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
    hosts="Hostx64/x64"
    if [ "${PROCESSOR_ARCHITECTURE:-}" = ARM64 ] || [ "${PROCESSOR_ARCHITEW6432:-}" = ARM64 ]; then
        hosts="Hostarm64/arm64 Hostx64/x64"
    fi
    dumpbin=""
    for host in $hosts; do
        found=$("$vswhere" -latest -products '*' -find "VC/Tools/MSVC/**/bin/$host/dumpbin.exe" | tr -d '\r')
        # 装了多个版本的工具集时有多行，取第一行。
        dumpbin=${found%%$'\n'*}
        [ -z "$dumpbin" ] || break
    done
    [ -n "$dumpbin" ] || fail "dumpbin.exe not found"
    # `-dependents` 用横杠而不是斜杠：Git Bash 会把 `/dependents` 当成路径改写。
    dependents=$("$dumpbin" -nologo -dependents "$bin" | tr -d '\r')
    imports=$(grep -iE '^[[:space:]]+[a-z0-9_.-]+\.dll$' <<<"$dependents" | tr -d ' ' || true)
    # 任何 Windows 程序至少导入 kernel32.dll；一个都没有说明 dumpbin 的输出没按预期解析。
    [ -n "$imports" ] || fail "dumpbin listed no imported DLLs"
    echo "imported DLLs: $(tr '\n' ' ' <<<"$imports")"
    if grep -iqE '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt-)' <<<"$imports"; then
        fail "the C runtime is linked dynamically (build with -C target-feature=+crt-static)"
    fi
    ;;
*)
    fail "unknown target $target"
    ;;
esac
echo "ok: $target"
