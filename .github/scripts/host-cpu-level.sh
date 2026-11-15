#!/usr/bin/env bash
# 打印本机 x86-64 CPU 最高支持的微架构等级（1–4）。只在 x86_64 机器上有意义。
#
# release.yml 用它判断“这台 runner 跑不跑得动刚编出来的产物”：GitHub 的 x86_64 runner 不一定
# 支持 AVX-512，v4 的产物在那样的机器上一启动就是非法指令。先独立地问 CPU，再决定跑不跑：
# 该能跑的等级（不高于本机）崩了算失败，不会被当成“机器不支持”放过去。
#
# 判断标准与 `sbot --version` 的 `target:` 一致（src/cli/version.rs 的 build_level）：每一级
# 只看有代表性的几项——v2 = SSE4.2 + POPCNT + CMPXCHG16B，v3 = AVX2 + BMI2 + FMA，
# v4 = AVX-512 F/BW/CD/DQ/VL。
set -euo pipefail

case "$(uname -s)" in
Darwin)
    # Intel Mac：每项指令集一个 `hw.optional.*`，1 为支持；没有这个键也算不支持。
    has() {
        [ "$(sysctl -n "hw.optional.$1" 2>/dev/null || echo 0)" = 1 ]
    }
    # Mac 上有 SSE4.2 的 CPU 都有 POPCNT 与 CMPXCHG16B，这两项没有单独的键。
    v2() { has sse4_2; }
    v3() { has avx2_0 && has bmi2 && has fma; }
    v4() { has avx512f && has avx512bw && has avx512cd && has avx512dq && has avx512vl; }
    ;;
*)
    # Linux，以及 Windows 上的 Git Bash（MSYS2 提供同样格式的 /proc/cpuinfo）。
    # 取第一个逻辑核的 flags 一行，两端补空格，按“空格 + 名字 + 空格”整词匹配。
    flags=" $(sed -n '/^flags[[:space:]]*:/{s/^[^:]*:[[:space:]]*//p;q;}' /proc/cpuinfo) "
    has() {
        case "$flags" in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
        esac
    }
    v2() { has sse4_2 && has popcnt && has cx16; }
    v3() { has avx2 && has bmi2 && has fma; }
    v4() { has avx512f && has avx512bw && has avx512cd && has avx512dq && has avx512vl; }
    ;;
esac

level=1
if v2; then
    level=2
    if v3; then
        level=3
        if v4; then
            level=4
        fi
    fi
fi
echo "$level"
