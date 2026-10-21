#!/usr/bin/env bash
#
# linux_smoke.sh —— 在 **Linux（含 WSL2）** 上跑一遍 FileWatch 的验收。
#
# 覆盖什么：
#   1. 构建（release）+ 单元测试
#   2. 内置自检（31 项，含 inotify 实机投递）
#   3. 卷枚举（/proc/self/mounts + statvfs）
#   4. 目录遍历全量扫描（--json 断言文件数与体积）
#   5. 快照 → 改动 → 对比（F11）
#   6. 实时监控（inotify）：改文件时必须收到"修改"事件
#   7. 重复文件查找（F12）
#
# 只依赖 bash + coreutils + cargo，**不引 jq / python**（WSL 新装环境里
# 未必有）。JSON 用 grep 取字段，够用且不会因为格式化差异误判。
#
# 用法：
#   bash tools/linux_smoke.sh            # 先构建再测
#   SKIP_BUILD=1 bash tools/linux_smoke.sh
#
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

# ── 小工具 ──────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); printf '  \033[32m✅\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31m❌\033[0m %s\n' "$1"; }
skip() { SKIP=$((SKIP+1)); printf '  \033[33m⏭\033[0m %s\n' "$1"; }
# check <描述> <期望> <实际>
check() { if [ "$2" = "$3" ]; then ok "$1（$2）"; else bad "$1：期望「$2」，实际「$3」"; fi; }

json_num() { # json_num <字段> <<< 文本
    sed -n "s/.*\"$1\":\([0-9-]*\).*/\1/p" | head -1
}

BOX="$(mktemp -d)"
cleanup() { rm -rf "$BOX"; }
trap cleanup EXIT

echo "=============================================================================="
echo "FileWatch Linux 冒烟测试"
echo "  内核：$(uname -srm)"
echo "  临时目录：$BOX"
echo "=============================================================================="

# ── 1. 构建 ─────────────────────────────────────────────────────────────────
BIN="target/release/filewatch"
if [ -z "${SKIP_BUILD:-}" ]; then
    echo
    echo "── 1. 构建（release） ──────────────────────────────────────────────"
    if cargo build --release 2>&1 | tail -3; then
        ok "cargo build --release"
    else
        bad "cargo build --release 失败"
        echo "构建不过就没法继续，退出。"
        exit 1
    fi
else
    skip "跳过构建（SKIP_BUILD=1）"
fi

if [ ! -x "$BIN" ]; then
    bad "找不到可执行文件 $BIN"
    exit 1
fi

# ── 2. 单元测试 ─────────────────────────────────────────────────────────────
echo
echo "── 2. 单元测试 ────────────────────────────────────────────────────"
TEST_LOG="$BOX/cargo-test.log"
if cargo test --release >"$TEST_LOG" 2>&1; then
    TOTAL_OK=$(grep -c '^test result: ok' "$TEST_LOG")
    TOTAL_NG=$(grep -c 'FAILED' "$TEST_LOG" || true)
    ok "cargo test：$TOTAL_OK 个测试二进制全绿（失败标记 $TOTAL_NG 处）"
else
    bad "cargo test 有失败，最后 20 行："
    tail -20 "$TEST_LOG"
fi

# ── 3. 自检 ─────────────────────────────────────────────────────────────────
echo
echo "── 3. 内置自检（selftest） ─────────────────────────────────────────"
SELF_LOG="$BOX/selftest.log"
if "$BIN" selftest >"$SELF_LOG" 2>&1; then
    LINE=$(grep '^结果：' "$SELF_LOG" | tail -1)
    ok "selftest 退出码 0 —— $LINE"
    # 跳过项要看得见：全"跳过"等于没测
    if grep -q '降级模式端到端' "$SELF_LOG"; then
        if grep -q '跳过.*降级模式端到端' "$SELF_LOG"; then
            bad "端到端自检被跳过（环境中 inotify 不可用？）"
        else
            ok "端到端自检已实际执行（未被跳过）"
        fi
    fi
else
    bad "selftest 失败"
    tail -30 "$SELF_LOG"
fi

# ── 4. 卷枚举 ───────────────────────────────────────────────────────────────
echo
echo "── 4. 卷枚举（/proc/self/mounts + statvfs） ────────────────────────"
VOL_LOG="$BOX/volumes.log"
if "$BIN" volumes >"$VOL_LOG" 2>&1; then
    ok "volumes 退出码 0：$(head -1 "$VOL_LOG")"
    # Linux 上根挂载点必须出现；且不能出现 // 这种拼坏的前缀
    if grep -q ' / ' "$VOL_LOG" || grep -qE '^\s*/ ' "$VOL_LOG"; then
        ok "列表里包含根挂载点 /"
    else
        bad "列表里没有 / —— 卷枚举解析可能出错"
    fi
    if grep -q '//' "$VOL_LOG"; then
        bad "输出里出现双斜杠（根前缀拼错了）"
    else
        ok "没有双斜杠前缀"
    fi
    # Linux 不该宣称有 MFT 直读
    if grep -q 'MFT 直读（极速）' "$VOL_LOG"; then
        bad "Linux 上不该出现「MFT 直读」提示"
    else
        ok "未错误宣称 MFT 直读"
    fi
else
    bad "volumes 退出码非 0"
    cat "$VOL_LOG"
fi

# ── 造一棵夹具树 ────────────────────────────────────────────────────────────
TREE="$BOX/tree"
mkdir -p "$TREE/docs" "$TREE/media" "$TREE/empty dir"
printf 'hello world\n' > "$TREE/a.txt"                    # 12
printf '0123456789'   > "$TREE/docs/b.bin"                # 10
head -c 5000 /dev/zero > "$TREE/media/c.dat"              # 5000
printf 'dup-content-1234\n' > "$TREE/dup1.txt"
printf 'dup-content-1234\n' > "$TREE/media/dup2.txt"
EXPECT_FILES=5
EXPECT_BYTES=$((12 + 10 + 5000 + 17 + 17))

# ── 5. 全量扫描 ─────────────────────────────────────────────────────────────
echo
echo "── 5. 目录遍历全量扫描 ────────────────────────────────────────────"
SCAN_JSON="$BOX/scan.json"
if "$BIN" scan "$TREE" --force-traverse --json --top=3 >"$SCAN_JSON" 2>"$BOX/scan.err"; then
    FILES=$(json_num files < "$SCAN_JSON")
    BYTES=$(json_num logical < "$SCAN_JSON")
    BACKEND=$(sed -n 's/.*"backend":"\([a-z]*\)".*/\1/p' "$SCAN_JSON")
    check "文件数" "$EXPECT_FILES" "${FILES:-?}"
    check "逻辑体积" "$EXPECT_BYTES" "${BYTES:-?}"
    check "后端" "traverse" "${BACKEND:-?}"
    if grep -q '"allocated":[1-9]' "$SCAN_JSON"; then
        ok "占用大小已统计（非 0）"
    else
        bad "占用大小为 0 —— st_blocks 读取可能失效"
    fi
    if grep -q '"errors":0' "$SCAN_JSON"; then
        ok "遍历错误数为 0"
    else
        skip "遍历错误数非 0（可见 stderr）"
    fi
else
    bad "scan 失败"
    tail -5 "$BOX/scan.err"
fi

# 路径前缀：必须是真实扫描根，而不是 "/" 或 "//"
if grep -q "\"path\":\"$TREE" "$SCAN_JSON" 2>/dev/null; then
    ok "Top-N 路径带真实扫描根前缀（PathRemap 生效）"
else
    if [ -f "$SCAN_JSON" ]; then
        bad "Top-N 路径不是真实扫描根下的路径：$(grep -o '"path":"[^"]*"' "$SCAN_JSON" | head -1)"
    fi
fi

# ── 6. 快照与对比（F11） ────────────────────────────────────────────────────
echo
echo "── 6. 快照 + 对比（F11） ──────────────────────────────────────────"
SNAP1="$BOX/s1.tsv"; SNAP2="$BOX/s2.tsv"
"$BIN" snapshot "$TREE" --force-traverse --out="$SNAP1" >/dev/null 2>&1
GROW="$TREE/media/c.dat"
head -c 20000 /dev/zero > "$GROW"          # 变大
rm -f "$TREE/a.txt"                        # 删除
printf 'brand new\n' > "$TREE/new.txt"     # 新增
"$BIN" snapshot "$TREE" --force-traverse --out="$SNAP2" >/dev/null 2>&1
DIFF_LOG="$BOX/diff.log"
if "$BIN" diff "$SNAP1" "$SNAP2" --top=10 >"$DIFF_LOG" 2>&1; then
    ok "diff 退出码 0"
    grep -q '变大' "$DIFF_LOG" && ok "识别出「变大」" || bad "没识别出「变大」"
    grep -q '删除' "$DIFF_LOG" && ok "识别出「删除」" || bad "没识别出「删除」"
    grep -q '新增' "$DIFF_LOG" && ok "识别出「新增」" || bad "没识别出「新增」"
    grep -q 'c.dat' "$DIFF_LOG" && ok "差异行带真实文件名" || bad "差异行没有 c.dat"
else
    bad "diff 失败"
    tail -10 "$DIFF_LOG"
fi

# ── 7. 实时监控（inotify） ──────────────────────────────────────────────────
echo
echo "── 7. 实时监控（inotify） ────────────────────────────────────────"
WATCH="$BOX/watch"
mkdir -p "$WATCH/sub"
printf 'init\n' > "$WATCH/init.txt"
MON_LOG="$BOX/monitor.log"
"$BIN" monitor "$WATCH" --duration=6s --interval=300ms --no-prealloc \
    >"$MON_LOG" 2>&1 &
MON_PID=$!
# 等它把目录树扫描完、watch 挂好（inotify 在 start() 里同步挂，不怕早）
for _ in $(seq 1 40); do
    grep -q '开始监听变更' "$MON_LOG" && break
    sleep 0.1
done
printf 'first write\n' > "$WATCH/live.txt"
sleep 0.6
printf 'second write longer\n' > "$WATCH/live.txt"
sleep 0.6
mkdir -p "$WATCH/sub/deep"
printf 'nested\n' > "$WATCH/sub/deep/nested.txt"
sleep 0.6
rm -f "$WATCH/live.txt"
wait $MON_PID 2>/dev/null
MON_RC=$?

if [ "$MON_RC" -eq 0 ]; then
    ok "monitor 正常退出（退出码 0）"
else
    bad "monitor 退出码 $MON_RC"
fi
grep -q 'live.txt' "$MON_LOG" && ok "收到 live.txt 的事件" || bad "没收到 live.txt 的事件（inotify 未生效？）"
grep -q 'nested.txt' "$MON_LOG" && ok "收到新建子目录里 nested.txt 的事件（子树 watch 补齐生效）" \
    || bad "没收到 nested.txt —— 新建子目录的 watch 没有补上"
grep -q '删除' "$MON_LOG" && ok "收到「删除」事件" || bad "没收到删除事件"
if grep -qi '中断（错误码' "$MON_LOG"; then
    bad "监控报告事件流中断：$(grep -i '中断' "$MON_LOG" | head -1)"
else
    ok "事件流未中断"
fi

# ── 8. 重复文件（F12） ──────────────────────────────────────────────────────
echo
echo "── 8. 重复文件查找（F12） ────────────────────────────────────────"
DUP_LOG="$BOX/dupes.log"
if "$BIN" dupes "$TREE" --force-traverse --min-size=1 >"$DUP_LOG" 2>&1; then
    ok "dupes 退出码 0"
    if grep -q 'dup2.txt' "$DUP_LOG" && grep -q 'dup1.txt' "$DUP_LOG"; then
        ok "找到 dup1.txt / dup2.txt 这一组"
    else
        bad "没找到预期的重复文件组"
        tail -10 "$DUP_LOG"
    fi
else
    bad "dupes 失败"
    tail -10 "$DUP_LOG"
fi

# ── 汇总 ────────────────────────────────────────────────────────────────────
echo
echo "=============================================================================="
printf '通过 %d / 失败 %d / 跳过 %d\n' "$PASS" "$FAIL" "$SKIP"
echo "=============================================================================="
if [ "$FAIL" -ne 0 ]; then
    echo "❌ 有失败项。日志在 $BOX（本脚本退出后会被清理，需要保留请自行 --keep）"
    exit 1
fi
echo "✅ 全部通过"
