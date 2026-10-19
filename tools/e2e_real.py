# -*- coding: utf-8 -*-
"""真实目录上的端到端对拍（F11 快照 / F12 重复文件）。

# 为什么单有一个脚本

`cargo test` 与 `selftest` 里的树都是现造的小树，断言的是"我们以为的语义"。
而重复文件与快照这两件事**只看屏幕不算数** —— 少一组、多一组、少算一个字节
都不会报错，只会安静地给出一个看起来合理的数字。所以这里换一个角度：
用 Python 的 `os.walk` / `hashlib` **独立实现**同一条语义，然后逐项比数字。

「屏幕对、日志错」和「路径自我 append」这两个 bug 都是这种真实目录实测才
暴露的，不是单测能cover到的。

# 覆盖什么

1. `dupes`   —— 组数 / 可回收字节 / CSV 路径集合，与「体积 + SHA-256」双重分组对拍。
   树里特意放了**同体积但内容不同**的文件，用来确认它真的被内容哈希筛掉。
2. `snapshot` —— 文件数 / 总字节，与 `os.walk` 对拍。
3. `diff`    —— 四类变更计数与净变化，与手算值对拍。

用法：
    python tools/e2e_real.py            # 用默认 release 产物
    python tools/e2e_real.py <exe>      # 指定别的 filewatch.exe
"""

import hashlib
import os
import subprocess
import sys
from safe_test_dir import SafeTestDir

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_EXE = os.path.join(ROOT, "target", "x86_64-pc-windows-gnu", "release", "filewatch.exe")
EXE = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_EXE)

OWNED = SafeTestDir("e2e-real")
TMP = str(OWNED.path)
TREE = os.path.join(TMP, "tree")
SNAP1 = os.path.join(TMP, "snapshot-1.tsv")
SNAP2 = os.path.join(TMP, "snapshot-2.tsv")
DUP_CSV = os.path.join(TMP, "duplicates.csv")
DIFF_CSV = os.path.join(TMP, "diff.csv")

fails = []
passes = [0]


def ok(cond, what):
    if cond:
        passes[0] += 1
        print("  ✅ %s" % what)
    else:
        fails.append(what)
        print("  ❌ %s" % what)


def run(*args):
    r = subprocess.run([EXE] + list(args), capture_output=True)
    return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")


def why(rc, err):
    """失败时把 stderr 的最后一段带上。成功时 `err` 里全是进度条的回车残影，
    混进断言文本只会让输出更难读，所以成功就什么都不带。"""
    if rc == 0:
        return ""
    tail = [l for l in err.replace("\r", "\n").splitlines() if l.strip()]
    return "　→ " + (tail[-1].strip() if tail else "(无 stderr)")


def write(rel, data):
    p = os.path.join(TREE, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(data)
    return p


def build_tree():
    os.mkdir(TREE)
    big = b"A" * 200_000
    write("A/dup1.bin", big)
    write("B/dup2.bin", big)
    write("C/dup3.bin", big)
    # 同体积、内容不同：必须在第二轮（部分哈希）或第三轮被筛掉，
    # 而且**绝不能**出现在结果里
    write("D/uniq.bin", b"B" * 200_000)
    # 小于下限：必须被排除
    write("E/small1.txt", b"x" * 100)
    write("E/small2.txt", b"x" * 100)


def py_dups(root, min_size):
    """独立实现：先按体积分桶，桶内再按 SHA-256 分桶。"""
    by_size = {}
    for dp, _dn, fns in os.walk(root):
        for fn in fns:
            p = os.path.join(dp, fn)
            sz = os.path.getsize(p)
            if sz >= max(min_size, 1):
                by_size.setdefault(sz, []).append(p)
    groups, wasted = [], 0
    for sz, ps in by_size.items():
        if len(ps) < 2:
            continue  # 体积唯一 → 引擎一个字节都不读
        by_hash = {}
        for p in ps:
            with open(p, "rb") as f:
                h = hashlib.sha256(f.read()).hexdigest()
            by_hash.setdefault(h, []).append(p)
        for _h, same in by_hash.items():
            if len(same) > 1:
                groups.append(sorted(same))
                wasted += sz * (len(same) - 1)
    return groups, wasted


def py_stat(root):
    n = total = 0
    for dp, _dn, fns in os.walk(root):
        for fn in fns:
            n += 1
            total += os.path.getsize(os.path.join(dp, fn))
    return n, total


def check_dupes():
    print("\n① dupes（F12）")
    rc, out, err = run("dupes", TREE, "--min-size=1K", "--top=20", "--csv=" + DUP_CSV)
    ok(rc == 0, "退出码 0（实际 %d）%s" % (rc, why(rc, err)))
    if rc != 0:
        return

    groups, wasted = py_dups(TREE, 1024)
    ok("重复 %d 组" % len(groups) in out, "组数与 Python 一致（%d 组）" % len(groups))
    ok("可回收 %.2f KB" % (wasted / 1024) in out, "可回收字节与 Python 一致（%d B）" % wasted)
    # 那三个 200 KB 的相同文件被合成一组，同体积不同内容的那个不在里面
    ok(len(groups) == 1 and len(groups[0]) == 3, "正好一组三份")
    ok("uniq.bin" not in out, "同体积但内容不同的文件**没有**被报成重复")

    rows = [l for l in open(DUP_CSV, encoding="utf-8").read().splitlines() if l.strip()]
    csv_paths = {l.rsplit(",", 1)[1] for l in rows[1:]}
    ok(csv_paths == {p for g in groups for p in g}, "CSV 里的路径集合与 Python 完全相等")


def check_snapshot():
    print("\n② snapshot（F11）")
    rc, out, err = run("snapshot", TREE, "--out=" + SNAP1)
    ok(rc == 0, "退出码 0（实际 %d）%s" % (rc, why(rc, err)))
    if rc != 0:
        return

    n, total = py_stat(TREE)
    body = [
        l for l in open(SNAP1, encoding="utf-8").read().splitlines()
        if l and not l.startswith("#")
    ]
    snap_total = sum(int(l.split("\t")[1]) for l in body)
    ok(len(body) == n, "条目数 = os.walk 的文件数（%d）" % n)
    ok(snap_total == total, "总字节 = os.walk 的合计（%d）" % total)
    ok(open(SNAP1, encoding="utf-8").readline().startswith("#filewatch-snapshot"),
       "首行是魔数（认错文件时能立刻发现）")


def check_diff():
    print("\n③ diff（F11）")
    # 删除一个 / 变大一个 / 新增一个 / 变小一个 —— 四类变更各来一个
    OWNED.remove_file(os.path.join(TREE, "B", "dup2.bin"))
    with open(os.path.join(TREE, "A", "dup1.bin"), "ab") as f:
        f.write(b"Z" * 4096)
    write("F/new.bin", b"N" * 3000)
    write("E/small1.txt", b"x" * 50)

    rc, out, err = run("snapshot", TREE, "--out=" + SNAP2)
    ok(rc == 0, "第二份快照写入成功%s" % why(rc, err))
    if rc != 0:
        return

    rc, out, err = run("diff", SNAP1, SNAP2, "--top=50", "--csv=" + DIFF_CSV)
    ok(rc == 0, "退出码 0（实际 %d）%s" % (rc, why(rc, err)))
    if rc != 0:
        return

    for want in ("新增 1 个", "删除 1 个", "变大 1 个", "变小 1 个"):
        ok(want in out, "四类变更计数正确：%s" % want)

    # 净变化 = +3000（新增）+4096（变大）−200000（删除）−50（变小） = −192954
    net = 3000 + 4096 - 200_000 - 50
    expect = ("−" if net < 0 else "+") + "%.2f KB" % (abs(net) / 1024)
    ok(expect in out, "净变化 = %s（手算 %d B）" % (expect, net))


def main():
    print("=" * 72)
    print("真实目录端到端对拍（F11 / F12）")
    print("=" * 72)
    print("被测程序 %s" % EXE)
    print("工作目录 %s" % TREE)
    if not os.path.exists(EXE):
        print("\n找不到 %s —— 先 cargo build --release" % EXE)
        return 1

    build_tree()
    check_dupes()
    check_snapshot()
    check_diff()

    print("\n" + "=" * 72)
    if fails:
        print("❌ 失败 %d 项：" % len(fails))
        for f in fails:
            print("   · " + f)
        return 1
    print("✅ 通过 %d 项断言" % passes[0])
    print("（目录与 CSV 留在临时目录，可自行删除）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
