#!/usr/bin/env bash
#
# linux_check.sh —— **在 Windows 上**对 Linux 目标做类型检查（不链接）。
#
# 为什么需要它：`cargo check --target x86_64-unknown-linux-gnu` 只做前端分析，
# 不需要 Linux 链接器，所以在一台纯 Windows 机器上就能验证
# `#[cfg(not(windows))]` 的那半份代码是否自洽。这能挡住 90% 的移植错误
# （类型不符、字段不存在、cfg 漏写），但**挡不住运行时行为问题** ——
# 真正的验收请用 `tools/linux_smoke.sh`（在 Linux/WSL2 里跑）。
#
# 前置：Linux 目标的标准库
#   rustup target add x86_64-unknown-linux-gnu
# 若镜像返回 404（实测清华源对某些日期缺包），换成 rsproxy：
#   RUSTUP_DIST_SERVER=https://rsproxy.cn rustup target add x86_64-unknown-linux-gnu
#
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
CARGO="${CARGO:-cargo}"

echo "=== 1/2  Linux 目标类型检查（含测试代码） ==="
# `--all-targets` 必须加：只查 lib 的话，`#[cfg(test)] mod tests` 里的
# Linux 专属断言（例如 symlink → reparse 位）根本没被编译过。
if ! "$CARGO" check --target "$TARGET" --all-targets; then
    echo "❌ 类型检查失败"
    exit 1
fi
echo "✅ 类型检查通过"

echo
echo "=== 2/2  Linux 目标 clippy（尽力而为） ==="
if "$CARGO" clippy --version >/dev/null 2>&1; then
    if "$CARGO" clippy --target "$TARGET" --all-targets -- -D warnings 2>&1 | tee /tmp/fw-clippy.log | tail -5; then
        echo "✅ clippy 无告警"
    else
        echo "⚠ clippy 有输出（见 /tmp/fw-clippy.log）—— 不阻断"
    fi
else
    echo "（未安装 clippy，跳过）"
fi

echo
echo "注意：这里只证明【能编译】。运行时行为请在 Linux/WSL2 里跑："
echo "  bash tools/linux_smoke.sh"
