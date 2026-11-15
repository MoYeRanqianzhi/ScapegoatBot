#!/usr/bin/env bash
# 把一个发布产物打成压缩包（release.yml 的每个构建任务都跑）。
#
# 用法：package.sh <二进制路径> <包名，例如 sbot-0.2.0-x86_64_v3-unknown-linux-gnu>
# 结果：dist/<包名>.tar.gz（Windows 是 dist/<包名>.zip），解开后是一个同名目录，里面是
# sbot 本体、LICENSE 与 README.md。在仓库根目录运行。
set -euo pipefail

bin=$1
name=$2

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bin" LICENSE README.md "$stage/$name/"
mkdir -p dist

case "$bin" in
*.exe)
    archive=dist/$name.zip
    # Windows 自带的 bsdtar：`-a` 按扩展名选格式，写出 zip。不用 Git Bash 的 tar（GNU tar
    # 不会写 zip）。
    tar="$SYSTEMROOT/System32/tar.exe"
    "$tar" -a -c -f "$archive" -C "$stage" "$name"
    listing=$("$tar" -t -f "$archive" | tr -d '\r')
    ;;
*)
    archive=dist/$name.tar.gz
    chmod 755 "$stage/$name/$(basename "$bin")"
    chmod 644 "$stage/$name/LICENSE" "$stage/$name/README.md"
    # 包里的属主写成 0:0。不这样做的话记下的是构建机器的用户（uid 1001 / 501）：以 root
    # 解包时 tar 默认还原属主，装到 /usr/local/bin 的程序就归了目标机器上恰好是这个 uid
    # 的普通用户，他能把它换掉。GNU tar（Linux）与 bsdtar（macOS）的写法不同。
    version=$(tar --version)
    case "$version" in
    *"GNU tar"*) owner=(--owner=0 --group=0 --numeric-owner) ;;
    *) owner=(--uid 0 --gid 0 --uname root --gname root) ;;
    esac
    # COPYFILE_DISABLE：macOS 的 tar 不把扩展属性存成 `._` 开头的伴随文件。
    # gzip -n：不记原文件名与时间戳。
    COPYFILE_DISABLE=1 tar -c -f - "${owner[@]}" -C "$stage" "$name" | gzip -9 -n >"$archive"
    listing=$(tar -t -z -f "$archive")
    ;;
esac

# 核对包里正好是那三个文件（外加目录本身），顺序不限。
expected=$(printf '%s\n' "$name/" "$name/$(basename "$bin")" "$name/LICENSE" "$name/README.md" | sort)
if [ "$(sort <<<"$listing")" != "$expected" ]; then
    echo "::error::$archive has unexpected contents:" >&2
    echo "$listing" >&2
    exit 1
fi
echo "$archive:"
echo "$listing"
