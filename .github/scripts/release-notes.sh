#!/usr/bin/env bash
# 生成 GitHub release 的说明：CHANGELOG.md 里这个版本的那一段，加上“该下载哪个文件”与校验方法。
#
# 用法：release-notes.sh <版本号> [CHANGELOG 段名]   （在仓库根目录运行，结果写到 stdout）
#
# 段名默认就是版本号。CHANGELOG 里没有这一段时失败：发布前必须先把 Unreleased 改成版本号。
# 试跑（不发布）时版本还没定，段名传 `Unreleased`。
set -euo pipefail

version=$1
section_name=${2:-$version}
repo=${GITHUB_REPOSITORY:-MoYeRanqianzhi/ScapegoatBot}

# 从 `## [<段名>]` 那一行之后取到下一个 `## [` 之前。用 index 做字面匹配：版本号里的点不是正则。
section=$(awk -v head="## [$section_name]" '
    index($0, head) == 1 { on = 1; next }
    on && /^## \[/ { exit }
    on { print }
' CHANGELOG.md)
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section [$section_name]" >&2
    exit 1
fi

printf '%s\n' "$section"
# 下面是原样输出的中文正文，里面的中文引号不是写错的 shell 引号。
# shellcheck disable=SC1111
cat <<EOF

## 下载哪个文件

文件名是 \`sbot-${version}-<平台>.tar.gz\`（Windows 是 \`.zip\`），解开后是一个同名目录，里面是 \`sbot\`
本体、LICENSE 与 README。

| 系统 | 平台 | 说明 |
|------|------|------|
| Linux，glibc ≥ 2.28（Debian 10+、Ubuntu 20.04+、RHEL / Rocky / Alma 8+、Amazon Linux 2023、Fedora、Arch、openSUSE 等） | \`x86_64-unknown-linux-gnu\`、\`aarch64-unknown-linux-gnu\` | 首选 |
| 任何 Linux（包括 Alpine、glibc 更旧的系统） | \`x86_64-unknown-linux-musl\`、\`aarch64-unknown-linux-musl\` | 全静态，不依赖任何系统库 |
| Windows 10 / 11 | \`x86_64-pc-windows-msvc\`、\`aarch64-pc-windows-msvc\` | 不需要安装 VC++ 运行库 |
| macOS 11+ | \`aarch64-apple-darwin\`（Apple 芯片）、\`x86_64-apple-darwin\`（Intel） | |

x86_64 还按 CPU 等级各出一份，等级写在架构后面（\`x86_64_v3-unknown-linux-gnu\`）。等级越高，编译器能用的
指令越新、越快：

- 不带后缀（v1）：任何 x86-64 CPU；
- \`_v2\`：2009 年以后的 CPU（SSE4.2、POPCNT）；
- \`_v3\`：Intel Haswell（2013）及以后、AMD Zen 及以后（AVX2、BMI2、FMA）；
- \`_v4\`：支持 AVX-512 的 CPU（Intel 服务器 Skylake-SP 及以后、AMD Zen 4 及以后等）。

不确定就先下不带后缀的版本，运行 \`sbot --version\`：\`cpu:\` 一行是这台机器能用的最高等级，再换成对应的
文件。选高了，程序一启动就以“非法指令”（Illegal instruction）退出，不会有别的影响。

## 文档

说明书在二进制里，与它同一个版本：\`sbot docs\` 列出全部，\`sbot docs sbot-deploy\` 从安装讲起，
\`sbot docs --export <目录>\` 把整套文件导出成 agent skill。

## 校验

\`\`\`bash
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify sbot-${version}-<平台>.tar.gz --repo ${repo}
\`\`\`

第二条核对构建来源证明：文件确实由本仓库的 release workflow 从 tag 对应的提交构建。
EOF
