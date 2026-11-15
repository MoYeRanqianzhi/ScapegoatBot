#!/usr/bin/env bash
# 实际运行一个发布产物（release.yml 的构建任务在本机 CPU 跑得动它时才跑）。
#
# 用法：smoke-test.sh <二进制路径> <期望的平台段> <期望的版本号> [期望的 commit]
#
# 两步：
# 1. `sbot --version`：版本号、`target:`、`commit:` 与期望一致。`target:` 是程序按编译时
#    真正启用的指令集算出来的（src/cli/version.rs），与文件名里的平台段一致才说明 CPU
#    等级参数生效了，包没有装错东西。
# 2. 在一个空的数据目录上走一遍 start → status → doctor → stop：建默认配置、生成主密钥、
#    建库并迁移（SQLite 是随包编译的 C 代码）、加密校验值、IPC、正常退出。NapCat 连不上，
#    守护进程停在“等待 NapCat”，不调用任何模型，也不联网。
set -euo pipefail

bin=$1
want_target=$2
want_version=$3
want_commit=${4:-}

fail() {
    echo "::error::$bin: $*" >&2
    exit 1
}

# --- 1. --version ---
version=$("$bin" --version | tr -d '\r')
echo "$version"
[ "$(sed -n 1p <<<"$version")" = "sbot $want_version" ] ||
    fail "version line is not 'sbot $want_version'"
[ "$(sed -n 's/^target: //p' <<<"$version")" = "$want_target" ] ||
    fail "built for a different target than $want_target (the CPU level flags did not take effect?)"
if [ -n "$want_commit" ]; then
    [ "$(sed -n 's/^commit: //p' <<<"$version")" = "$want_commit" ] ||
        fail "commit line is not $want_commit"
fi

# --- 2. start → status → doctor → stop ---
home=$(mktemp -d)
export SBOT_HOME=$home
"$bin" start >"$home/daemon.log" 2>&1 &
daemon=$!

# 出错时：把守护进程的日志打出来，并确保它不会留在后台。
abort() {
    echo "--- daemon log ---" >&2
    cat "$home/daemon.log" >&2 || true
    kill "$daemon" 2>/dev/null || true
    fail "$*"
}

# 等 IPC 起来：最多 30 秒。守护进程提前退出（启动失败）时不必等满。
ready=false
for _ in $(seq 1 120); do
    if "$bin" status >"$home/status.json" 2>/dev/null; then
        ready=true
        break
    fi
    kill -0 "$daemon" 2>/dev/null || break
    sleep 0.25
done
[ "$ready" = true ] || abort "the daemon did not answer 'sbot status'"
grep -q '"state": "waiting_for_napcat"' "$home/status.json" ||
    abort "unexpected status: $(cat "$home/status.json")"

# doctor 读配置、主密钥与数据库（校验值、schema 版本、索引一致性）；有 error 时非零退出。
"$bin" doctor || abort "sbot doctor reported errors"

"$bin" stop >/dev/null || abort "sbot stop failed"
# 等守护进程自己退出：最多 30 秒，退出码必须是 0。
for _ in $(seq 1 120); do
    kill -0 "$daemon" 2>/dev/null || break
    sleep 0.25
done
if kill -0 "$daemon" 2>/dev/null; then
    abort "the daemon is still running 30 seconds after 'sbot stop'"
fi
wait "$daemon" || abort "the daemon exited with an error"

rm -rf "$home"
echo "ok: $want_target runs"
