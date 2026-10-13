# -*- coding: utf-8 -*-
"""
对 tools/check_api.py 做变异测试：故意注入各类错误，验证检查器真的能抓到。
一个「报 0 错误」的检查器若通不过变异测试，其报 0 就毫无意义。

# 为什么不是「每轮重建副本」

早期实现每轮都 `rmtree` + `copytree` 整份源码树 —— 21 轮下来要删除一千多个
文件，既慢，又会撞上环境的「批量删除保护」。现在改成：

* `PRIME`：干净副本，**只建一次**；
* `WORK` ：工作副本，每轮只把上一轮**动过的那几个文件**从 `PRIME` 还原回去
  （变异新增的文件则删掉），其余文件不碰。

于是每次运行的文件删除量从 ~1400 降到个位数，而且单轮耗时从「复制整棵树」
变成「复制一两个文件」。
"""
import io, os, re, shutil, subprocess, sys, tempfile

PY = r"C:\Users\HY\.workbuddy\binaries\python\versions\3.13.12\python.exe"
BASE = r"C:\Users\HY\WorkBuddy\2026-10-13-22-04-36\filewatch"
# 刻意放在系统临时目录而不是工程目录里：里面全是复制出来的副本，
# 放工程里既污染目录列表，又会被 check_api.py 的「孤儿文件」检查当成源码。
ROOT = os.path.join(tempfile.gettempdir(), "filewatch_muttest")
PRIME = os.path.join(ROOT, "prime")   # 干净副本，只建一次
WORK = os.path.join(ROOT, "work")     # 当前变异用的工作副本
TMP = WORK                            # 旧名字，check_api.py 的目标目录

# 复制时跳过的目录（道理同上：它们不是源码）
SKIP_DIRS = ("target",)

# 本轮变异动过的**相对路径**。`fresh()` 靠它做定向还原。
TOUCHED = []


def read(p):
    return io.open(p, encoding='utf-8').read()


def write(p, s):
    TOUCHED.append(os.path.relpath(p, WORK))
    io.open(p, 'w', encoding='utf-8', newline='').write(s)


def _sync(src, dst):
    """`src` → `dst`：覆盖同名、补齐缺失（不删除 `dst` 里的额外文件）。"""
    os.makedirs(dst, exist_ok=True)
    for root, dirs, files in os.walk(src):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        rel = os.path.relpath(root, src)
        target = dst if rel == '.' else os.path.join(dst, rel)
        os.makedirs(target, exist_ok=True)
        for f in files:
            shutil.copyfile(os.path.join(root, f), os.path.join(target, f))


def _extras(src, dst):
    """`dst` 里存在、`src` 里没有的文件（相对路径）。"""
    out = []
    for root, dirs, files in os.walk(dst):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        rel = os.path.relpath(root, dst)
        for f in files:
            r = f if rel == '.' else os.path.join(rel, f)
            if not os.path.isfile(os.path.join(src, r)):
                out.append(r)
    return out


def prepare():
    """建好 `PRIME`（若不存在）并把 `WORK` 对齐到 `PRIME`。只在开跑前调用一次。"""
    os.makedirs(ROOT, exist_ok=True)
    if not os.path.isfile(os.path.join(PRIME, 'Cargo.toml')):
        if os.path.isdir(PRIME):
            # 上次跑到一半留下的残缺副本
            shutil.rmtree(PRIME)
        os.makedirs(PRIME)
        for d in ('crates', 'apps'):
            shutil.copytree(os.path.join(BASE, d), os.path.join(PRIME, d))
        shutil.copy(os.path.join(BASE, 'Cargo.toml'), PRIME)
        shutil.copy(os.path.join(BASE, 'tools', 'check_api.py'), PRIME)
    os.makedirs(WORK, exist_ok=True)
    # 上次异常退出可能在 WORK 里留下变异产物 → 清掉（通常 0~1 个文件）
    for r in _extras(PRIME, WORK):
        os.remove(os.path.join(WORK, r))
    _sync(PRIME, WORK)
    TOUCHED.clear()


def fresh():
    """定向还原：只把上一轮动过的文件恢复到干净状态。"""
    for rel in TOUCHED:
        src = os.path.join(PRIME, rel)
        dst = os.path.join(WORK, rel)
        if os.path.isfile(src):
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copyfile(src, dst)
        elif os.path.isfile(dst):
            # 变异**新增**的文件（如孤儿源文件）→ 删掉。每轮至多一个。
            os.remove(dst)
    TOUCHED.clear()


def run():
    r = subprocess.run([PY, 'check_api.py', TMP], cwd=TMP,
                       capture_output=True, text=True, encoding='utf-8')
    return r.stdout

def result(out):
    errs = re.findall(r'^\s+\[ERROR/(\w+)\] (.+)$', out, re.M)
    warns = re.findall(r'^\s+\[WARN/(\w+)\] (.+)$', out, re.M)
    return errs, warns

MUTATIONS = []

def mut(name, expect, fn):
    MUTATIONS.append((name, expect, fn))

# ── A
def m_orphan():
    write(os.path.join(TMP, 'crates/core-viz/src/bogus.rs'), 'pub fn orphan() {}\n')
mut("孤儿源文件 core-viz/src/bogus.rs", "A", m_orphan)

def m_missing_mod():
    p = os.path.join(TMP, 'crates/core-viz/src/lib.rs')
    write(p, read(p) + '\nmod does_not_exist;\n')
mut("lib.rs 声明了不存在的 mod", "A", m_missing_mod)

# ── B
def m_dep_missing():
    p = os.path.join(TMP, 'crates/core-monitor/Cargo.toml')
    write(p, re.sub(r'(?m)^core-usn = .*$\n', '', read(p)))
mut("Cargo.toml 删掉被使用的依赖 core-usn", "B", m_dep_missing)

def m_dep_path():
    p = os.path.join(TMP, 'crates/core-ntfs/Cargo.toml')
    write(p, read(p).replace('path = "../core-io"', 'path = "../core-io-typo"'))
mut("path 依赖指向不存在的目录", "B", m_dep_path)

# ── D（三种 use 形态）
def m_use_braced():
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    s = read(p)
    write(p, 'use core_index::{NotARealSymbol};\n' + s)
mut("use core_x::{不存在的符号}", "D", m_use_braced)

def m_use_plain():
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    s = read(p)
    write(p, 'use core_index::bogus_mod::Whatever;\n' + s)
mut("use core_x::不存在的模块::Item", "D", m_use_plain)

def m_use_mod_braced():
    p = os.path.join(TMP, 'crates/core-index/src/tree.rs')
    s = read(p)
    write(p, s.replace('use core_ntfs::filename::{',
                       'use core_ntfs::filename::{NoSuchEnumerator,\n', 1))
mut("use core_x::mod::{不存在的项}", "D", m_use_mod_braced)

def m_use_crate_rel():
    p = os.path.join(TMP, 'crates/core-scan/src/session.rs')
    s = read(p)
    write(p, 'use crate::nonexistent_mod::Thing;\n' + s)
mut("use crate::不存在的模块（同 crate）", "D", m_use_crate_rel)

# ── E / F
def m_e():
    p = os.path.join(TMP, 'apps/cli/src/main.rs')
    write(p, read(p) + '\nfn _probe_e() { let _ = core_io::winapi::NoSuchFn; }\n')
mut("全限定 core_x::mod::不存在的项", "E", m_e)

def m_f():
    p = os.path.join(TMP, 'crates/core-index/src/tree.rs')
    write(p, read(p) + '\nfn _probe_f() { let _ = crate::arena::NoSuchItem; }\n')
mut("crate::mod::不存在的项", "F", m_f)

# ── G
def m_use_unused():
    p = os.path.join(TMP, 'crates/core-index/src/tree.rs')
    s = read(p)
    write(p, 'use core_index::ExtStat;\n' + s)
mut("导入了但文件内未使用的项", "G", m_use_unused)

# ── 回归守卫：这些「看着可疑但完全合法」的写法必须零报错
def m_regress_ffi():
    """extern 块内的 pub fn 必须可见（否则手写 FFI 的表全被误判）"""
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    write(p, read(p) + '\nfn _probe_ffi() { let _ = core_io::winapi::SetConsoleCtrlHandler; }\n')
mut("回归：extern 块内 pub fn 仍可见", None, m_regress_ffi)

def m_regress_module_import():
    """`use crate::m;`（导入模块本身）合法"""
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    write(p, read(p) + '\nfn _probe_mi() { let _ = crate::rules::RuleKind::Absolute; }\n')
mut("回归：crate::模块::项 合法引用", None, m_regress_module_import)

def m_regress_trait():
    """trait 靠方法语法使用，不得报 unused"""
    p = os.path.join(TMP, 'crates/core-scan/src/traverse.rs')
    s = read(p)
    write(p, s)   # 不改动，仅确认既有 MetadataExt 不误报
mut("回归：trait 导入（MetadataExt）不误报", None, m_regress_trait)


# ── H（结构体字面量字段完整性）
def m_struct_missing():
    """删掉 EngineConfig 全量字面量里的 collect_events（无 ..rest）"""
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    s = read(p)
    write(p, s.replace('            collect_events: false,\n', '', 1))
mut("结构体字面量缺少字段（E0063）", "H", m_struct_missing)

def m_struct_extra():
    """往字面量里塞一个不存在的字段"""
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    s = read(p)
    write(p, s.replace('            collect_events: false,\n',
                       '            collect_events: false,\n            nonexistent_field: 1,\n', 1))
mut("结构体字面量含未声明字段（E0560）", "H", m_struct_extra)

def m_struct_typo():
    """字段名拼错 —— 同时触发「缺一个 + 多一个」"""
    p = os.path.join(TMP, 'crates/core-monitor/src/engine.rs')
    s = read(p)
    write(p, s.replace('            queue_capacity: DEFAULT_QUEUE_CAPACITY,\n',
                       '            queue_capcity: DEFAULT_QUEUE_CAPACITY,\n', 1))
mut("结构体字面量字段名拼错", "H", m_struct_typo)

# ── H 回归守卫
def m_regress_rettype():
    """`fn f() -> Rect {` 是返回类型不是字面量，不得误报"""
    p = os.path.join(TMP, 'crates/core-viz/src/camera.rs')
    write(p, read(p) + '\nfn _probe_ret() -> Rect { Rect::new(0.0, 0.0, 1.0, 1.0) }\n')
mut("回归：`-> Rect {` 返回类型不误报", None, m_regress_rettype)

def m_regress_rest():
    """`Config { a: 1, ..Default::default() }` 合法"""
    p = os.path.join(TMP, 'crates/core-viz/src/layout.rs')
    write(p, read(p) + '\nfn _probe_rest() -> LodPolicy { LodPolicy { min_side: 1.0, ..Default::default() } }\n')
mut("回归：带 ..Default::default() 的部分字面量", None, m_regress_rest)

def m_regress_self():
    """`Self { ... }` 在 impl 内应解析为所属类型"""
    p = os.path.join(TMP, 'crates/core-viz/src/rect.rs')
    write(p, read(p) + '\nfn _probe_self() -> Rect { Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 } }\n')
mut("回归：`Self { ... }` / 普通字面量不误报", None, m_regress_self)


print("=" * 78)
print("变异测试：验证 tools/check_api.py 的检出能力")
print("=" * 78)
print("干净副本 %s" % PRIME)
print("工作副本 %s" % WORK)
print("=" * 78)
prepare()
ok = bad = 0
for name, expect, fn in MUTATIONS:
    fresh()
    fn()
    out = run()
    errs, warns = result(out)
    got_kinds = {k for k, _ in errs} | {k for k, _ in warns}
    if expect is None:
        if not errs and not warns:
            print("  ✅ %-42s 零报错（符合预期）" % name); ok += 1
        else:
            print("  ❌ %-42s 误报！" % name)
            for k, m in errs:  print("        [ERROR/%s] %s" % (k, m))
            for k, m in warns: print("        [WARN/%s] %s" % (k, m))
            bad += 1
    else:
        if expect in got_kinds:
            hit = [m for k, m in (errs + warns) if k == expect][0]
            print("  ✅ %-42s 命中 %s：%s" % (name, expect, hit[:62])); ok += 1
        else:
            print("  ❌ %-42s 未检出（期望 %s）" % (name, expect)); bad += 1
print("=" * 78)
print("通过 %d / 失败 %d" % (ok, bad))
# 刻意**不**删除副本目录：那是一次上百个文件的批量删除，既没必要（下次运行
# 会原地复用并自我对齐），也很容易触发环境的批量删除保护。
# 想彻底清掉就自己 `rm -rf %s`。
print("副本留在 %s（下次运行会复用，可自行删除）" % ROOT)
sys.exit(1 if bad else 0)
