# -*- coding: utf-8 -*-
"""
静态预编译检查器 —— 在无 rustc 环境下替代第一轮 `cargo check` 的可判定部分。

检查项
  A  模块/文件：`mod X;` 是否有文件；是否有未声明的孤儿 .rs
  B  依赖：Cargo.toml 的 path 依赖是否存在；是否「用了未声明」/「声明未用」
  C  可见符号表：跨 crate 可见的顶层名（lib.rs 的 pub use 重导出 + 各模块深度 0 的 pub item）
  D  use 树：`use core_x::{A, B as C, m::D};`、`use core_x::m::D;` 每个叶子路径是否可解析
  E  全限定路径 `core_x::mod::Item` 是否可解析
  F  同 crate `crate::mod::Item` 是否可解析
  G  未使用的 use（rustc `unused_imports`）；已排除 pub use 与「靠方法语法使用」的 trait

自检：strip_noise 严格保持长度与换行不变（脚本启动时断言），因此所有位置/行号在两份文本
      （原始 / 去噪）中一致。

用法：python _check_api.py [项目根目录]
"""
import os, re, sys, collections

ROOT = sys.argv[1] if len(sys.argv) > 1 else r"C:\Users\HY\WorkBuddy\2026-10-13-22-04-36\filewatch"

CRATES = {
    "core_io":      "crates/core-io",
    "core_ntfs":    "crates/core-ntfs",
    "core_usn":     "crates/core-usn",
    "core_index":   "crates/core-index",
    "core_viz":     "crates/core-viz",
    "core_monitor": "crates/core-monitor",
    "core_scan":    "crates/core-scan",
}
BIN_CRATES = {"filewatch": "apps/cli"}
KNOWN = set(CRATES)

# 靠「方法语法 / 宏」使用的 trait：文本里可能不出现标识符，G 检查对其一律放行
TRAIT_LIKE = {
    'Write', 'Read', 'Seek', 'BufRead', 'MetadataExt', 'FileExt', 'FileTypeExt',
    'OsStrExt', 'OsStringExt', 'FromStr', 'FromIterator', 'Iterator', 'IntoIterator',
    'DoubleEndedIterator', 'ExactSizeIterator', 'AsRef', 'AsMut', 'TryFrom', 'TryInto',
    'Send', 'Sync', 'Deref', 'DerefMut', 'Add', 'Sub', 'Mul', 'Div', 'Rem',
    'Index', 'IndexMut', 'PartialOrd', 'Ord', 'PartialEq', 'Eq', 'Hash',
    'Clone', 'Copy', 'Drop', 'Default', 'Display', 'Debug', 'Error', 'Termination',
    'Future', 'Fn', 'FnMut', 'FnOnce', 'ToString', 'From', 'Into', 'Extend',
    'Sized', 'Unpin', 'Step', 'RangeBounds', 'ThreadId',
}

problems = []
def P(level, chk, msg):
    problems.append((level, chk, msg))

def rd(p):
    with open(p, "r", encoding="utf-8") as f:
        return f.read()

# ────────────────────────────────────────────── 去噪：长度与换行严格不变
def strip_noise(src):
    """把注释/字符串/字符字面量的内容替换为空格，保留换行。
    保证 len(返回值) == len(src) 且 '\\n' 的位置完全一致。"""
    buf = list(src)
    i, n = 0, len(src)
    def blank(a, b):
        for t in range(a, min(b, n)):
            if src[t] != '\n':
                buf[t] = ' '
    while i < n:
        c = src[i]
        if c == '/' and i+1 < n and src[i+1] == '/':
            j = i
            while j < n and src[j] != '\n':
                j += 1
            blank(i, j); i = j; continue
        if c == '/' and i+1 < n and src[i+1] == '*':
            depth = 1; j = i + 2
            while j < n and depth:
                if src[j] == '/' and j+1 < n and src[j+1] == '*':
                    depth += 1; j += 2
                elif src[j] == '*' and j+1 < n and src[j+1] == '/':
                    depth -= 1; j += 2
                else:
                    j += 1
            blank(i, j); i = j; continue
        if c == 'r' and i+1 < n and src[i+1] in '#"':
            j = i+1; hashes = 0
            while j < n and src[j] == '#':
                hashes += 1; j += 1
            if j < n and src[j] == '"':
                k = src.find('"' + '#'*hashes, j+1)
                k = n if k == -1 else k + 1 + hashes
                blank(i, k); i = k; continue
        if c == '"':
            j = i+1
            while j < n:
                if src[j] == '\\': j += 2; continue
                if src[j] == '"': break
                j += 1
            blank(i, j+1); i = j+1; continue
        if c == "'":
            m = re.match(r"'(?:\\.|[^\\'])'", src[i:])
            if m:
                blank(i, i + len(m.group(0))); i += len(m.group(0)); continue
            i += 1; continue
        i += 1
    return ''.join(buf)

# strip_noise 自检：长度 / 换行 / 括号均不得失真
_selftest = 'let a = "br{ace"; // }{ comment\n/* { } */ let c = \'"\'; /* 多行\n{ */'
assert len(strip_noise(_selftest)) == len(_selftest), "strip_noise 破坏长度"
assert strip_noise(_selftest).count('\n') == _selftest.count('\n'), "strip_noise 破坏换行"
assert strip_noise(_selftest).count('{') == 0, "strip_noise 漏掉花括号"

# ────────────────────────────────────────────── 花括号深度（extern 块不计入）
def build_depth_array(src_raw, clean):
    """`extern "X" { ... }` 的成对花括号不计入深度 —— rustc 语义下 extern 块里的
    `pub fn` 就是模块级项（否则手写 FFI 的整个接口表都会被判成不可见）。"""
    ignore = set()
    for m in re.finditer(r'\bextern\s+"[^"]*"\s*\{', src_raw):
        i = m.end() - 1
        d = 0; j = i
        while j < len(clean):
            if clean[j] == '{': d += 1
            elif clean[j] == '}':
                d -= 1
                if d == 0: break
            j += 1
        ignore.add(i)
        if j < len(clean): ignore.add(j)
    dep = [0] * (len(clean) + 1)
    d = 0
    for k, ch in enumerate(clean):
        if ch == '{' and k not in ignore: d += 1
        elif ch == '}' and k not in ignore: d -= 1
        dep[k + 1] = d
    return dep

# ────────────────────────────────────────────── pub item 抽取（仅模块层级）
RE_FN     = re.compile(r'\bpub(?:\([^)]*\))?\s+(?:const\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)')
RE_STRUCT = re.compile(r'\bpub(?:\([^)]*\))?\s+(?:struct|enum|union|trait)\s+([A-Za-z_]\w*)')
RE_TYPE   = re.compile(r'\bpub(?:\([^)]*\))?\s+type\s+([A-Za-z_]\w*)')
RE_CONST  = re.compile(r'\bpub(?:\([^)]*\))?\s+(?:const|static)\s+([A-Za-z_]\w*)')
RE_MACRO  = re.compile(r'\bmacro_rules!\s*([A-Za-z_]\w*)')

def module_level_pub_items(src_raw):
    clean = strip_noise(src_raw)
    dep = build_depth_array(src_raw, clean)
    found = set()
    for rx in (RE_FN, RE_STRUCT, RE_TYPE, RE_CONST, RE_MACRO):
        for m in rx.finditer(clean):
            if dep[m.start()] == 0:
                found.add(m.group(1))
    return found

# ────────────────────────────────────────────── use 树解析
def split_top_level(text, sep):
    out = []; depth = 0; cur = ''
    for ch in text:
        if ch in '{(': depth += 1
        elif ch in '})': depth -= 1
        if ch == sep and depth == 0:
            out.append(cur); cur = ''
        else:
            cur += ch
    out.append(cur)
    return out

def use_tree_paths(s):
    """把一棵 use 树展开为 [(kind, segs)]，kind ∈ {'path','glob'}"""
    res = []
    def walk(prefix, text):
        for p in split_top_level(text, ','):
            p = p.strip()
            if not p:
                continue
            if p == 'self':
                if prefix: res.append(('path', prefix))
                continue
            g = re.match(r'^(.*?)::\*\s*$', p)
            if g:
                res.append(('glob', prefix + [x for x in g.group(1).split('::') if x])); continue
            if '{' in p:
                bi = p.find('{')
                head = p[:bi].rstrip(':').strip()
                inner = p[bi+1:]
                if '}' in inner: inner = inner[:inner.rfind('}')]
                newpre = prefix + [x for x in head.split('::') if x] if head else list(prefix)
                walk(newpre, inner); continue
            p = re.sub(r'\s+as\s+[A-Za-z_]\w*\s*$', '', p).strip()
            segs = [x for x in p.split('::') if x]
            if segs and segs[0] == 'self':
                res.append(('path', prefix + segs[1:]))
            else:
                res.append(('path', prefix + segs))
    walk([], s)
    return res

# ────────────────────────────────────────────── A. 模块与文件
crate_data = {}
for cname, rel in list(CRATES.items()) + list(BIN_CRATES.items()):
    srcdir = os.path.join(ROOT, rel.replace('/', os.sep), "src")
    if not os.path.isdir(srcdir):
        P("ERROR", "A", "%s: src 目录不存在 -> %s" % (cname, srcdir)); continue
    files = sorted(f[:-3] for f in os.listdir(srcdir) if f.endswith(".rs"))
    rootstem = "lib" if "lib" in files else "main"
    rootfile = rootstem + ".rs"
    root_raw = rd(os.path.join(srcdir, rootfile))
    clean = strip_noise(root_raw)
    mods = set(re.findall(r'(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*;', clean))
    subfiles = set(files) - {rootstem}
    if mods - subfiles:
        P("ERROR", "A", "%s: %s 声明了 mod 但文件缺失 -> %s"
          % (cname, rootfile, ", ".join(sorted(mods - subfiles))))
    if subfiles - mods:
        P("WARN", "A", "%s: 存在未被 %s 声明的孤儿文件 -> %s"
          % (cname, rootfile, ", ".join(sorted(subfiles - mods))))
    crate_data[cname] = {"dir": srcdir, "files": files, "mods": mods,
                         "rootstem": rootstem, "root": rootfile, "root_clean": clean}

# ────────────────────────────────────────────── B. 依赖一致性
dep_used = collections.defaultdict(set)
for cname, info in crate_data.items():
    for f in info["files"]:
        for m in re.finditer(r'\b(core_[a-z_]+)::', strip_noise(rd(os.path.join(info["dir"], f + ".rs")))):
            dep_used[cname].add(m.group(1))

for cname, rel in list(CRATES.items()) + list(BIN_CRATES.items()):
    ct = rd(os.path.join(ROOT, rel.replace('/', os.sep), "Cargo.toml"))
    m = re.search(r'(?ms)^\[dependencies\](.*?)(?=^\[|\Z)', ct)
    block = m.group(1) if m else ""
    for mm in re.finditer(r'path\s*=\s*"([^"]+)"', block):
        p = os.path.normpath(os.path.join(ROOT, rel.replace('/', os.sep), mm.group(1)))
        if not os.path.isdir(p):
            P("ERROR", "B", "%s: path 依赖不存在 -> %s" % (cname, p))
    dec = {mm.group(1).replace('-', '_')
           for mm in re.finditer(r'(?m)^[ \t]*([A-Za-z_][\w-]*)[ \t]*=', block)}
    undecl = sorted((dep_used.get(cname, set()) & KNOWN) - dec)
    unused = sorted((dec & KNOWN) - dep_used.get(cname, set()))
    if undecl: P("ERROR", "B", "%s: 使用了但 Cargo.toml 未声明依赖 -> %s" % (cname, ", ".join(undecl)))
    if unused: P("WARN",  "B", "%s: 声明了依赖但源码未使用 -> %s" % (cname, ", ".join(unused)))

# ────────────────────────────────────────────── C. 可见符号表
pub_by_crate = {}
mod_pub = {}
for cname, info in crate_data.items():
    for f in info["files"]:
        mod_pub[(cname, f)] = module_level_pub_items(rd(os.path.join(info["dir"], f + ".rs")))
    lc = info["root_clean"]
    vis = set(mod_pub[(cname, info["rootstem"])])
    for m in re.finditer(r'(?ms)\bpub\s+use\s+([^;]+);', lc):
        for kind, segs in use_tree_paths(m.group(1)):
            if kind == 'glob':
                if segs and (cname, segs[-1]) in mod_pub:
                    vis |= mod_pub[(cname, segs[-1])]
            elif segs:
                vis.add(segs[-1])
        for part in split_top_level(m.group(1), ','):
            al = re.match(r'\s*\S+\s+as\s+([A-Za-z_]\w*)\s*$', part)
            if al: vis.add(al.group(1))
    pub_by_crate[cname] = vis

# ────────────────────────────────────────────── D/E/F/G
def resolve_path(cname, segs, where):
    if not segs: return None
    head = segs[0]
    if head in ('self', 'super'): return None
    if head == 'crate':
        base, rest = cname, segs[1:]
    elif head in KNOWN:
        base, rest = head, segs[1:]
    else:
        return None
    if not rest:
        P("ERROR", "D", "%s  `use %s::` 指向 crate 本身，不能作为项导入" % (where, head)); return False
    if len(rest) == 1:
        if rest[0] in pub_by_crate.get(base, set()) or (base, rest[0]) in mod_pub:
            return True
        P("ERROR", "D", "%s  `%s` 既不是 %s 的重导出项，也不是其 pub 模块"
          % (where, rest[0], base)); return False
    modname = rest[0]
    if (base, modname) not in mod_pub:
        P("ERROR", "D", "%s  `%s::%s` —— %s 中不存在模块 `%s`"
          % (where, base, modname, base, modname)); return False
    tail = rest[-1]
    if tail not in mod_pub[(base, modname)]:
        P("ERROR", "D", "%s  `%s::%s::%s` —— `%s` 不在 %s::%s 中"
          % (where, base, modname, tail, tail, base, modname)); return False
    return True

for cname, info in crate_data.items():
    for f in info["files"]:
        raw = rd(os.path.join(info["dir"], f + ".rs"))
        src = strip_noise(raw)
        for m in re.finditer(r'(?m)^[ \t]*(pub(?:\([^)]*\))?[ \t]+)?use[ \t]+([^;]*);', src):
            body = m.group(2)
            is_pub = bool(m.group(1))
            ln = src.count('\n', 0, m.start()) + 1
            where = "%s/%s.rs:%d" % (cname, f, ln)
            for kind, segs in use_tree_paths(body):
                if kind == 'glob': continue
                resolve_path(cname, segs, where)
            if is_pub:
                continue                      # 重导出：本文件不用很正常
            for part in split_top_level(body, ','):
                part = part.strip()
                if not part or '{' in part or part.endswith('::*'):
                    continue
                al = re.match(r'^(.+?)\s+as\s+([A-Za-z_]\w*)$', part)
                ident = al.group(2) if al else part.split('::')[-1].strip()
                if not re.fullmatch(r'[A-Za-z_]\w*', ident) or ident in ('self', '_'):
                    continue
                if ident in TRAIT_LIKE:
                    continue
                outside = src[:m.start()] + src[m.end():]
                if not re.search(r'\b%s\b' % re.escape(ident), outside):
                    P("WARN", "G", "%s  `%s` 在文件中未被使用（rustc: unused_imports）" % (where, ident))
        for m in re.finditer(r'\b(core_[a-z_]+)\s*::\s*([A-Za-z_]\w*)\s*::\s*([A-Za-z_]\w*)', src):
            tgt, m1, m2 = m.group(1), m.group(2), m.group(3)
            if (tgt, m1) in mod_pub and m2 not in mod_pub[(tgt, m1)]:
                P("ERROR", "E", "%s/%s.rs:%d  `%s::%s::%s` —— `%s` 不在 %s::%s 中"
                  % (cname, f, src.count('\n', 0, m.start()) + 1, tgt, m1, m2, m2, tgt, m1))
        for m in re.finditer(r'\bcrate\s*::\s*([A-Za-z_]\w*)\s*::\s*([A-Za-z_]\w*)', src):
            m1, m2 = m.group(1), m.group(2)
            if (cname, m1) in mod_pub and m2 not in mod_pub[(cname, m1)]:
                P("ERROR", "F", "%s/%s.rs:%d  `crate::%s::%s` —— `%s` 不在 %s::%s 中"
                  % (cname, f, src.count('\n', 0, m.start()) + 1, m1, m2, m2, cname, m1))

# ────────────────────────────────────────────── H. 结构体字面量的字段完整性
# 这是 rustc 的 E0063（缺字段）/ E0560（多字段），无法靠肉眼审完，必须机器查。
RESERVED = {
    'if', 'else', 'match', 'for', 'while', 'loop', 'impl', 'fn', 'struct', 'enum',
    'union', 'trait', 'unsafe', 'pub', 'mod', 'use', 'where', 'return', 'in', 'as',
    'move', 'ref', 'dyn', 'static', 'const', 'type', 'let', 'mut', 'crate', 'super',
    'self', 'Self', 'extern', 'async', 'await', 'box', 'break', 'continue',
}

def parse_struct_fields(clean):
    """具名字段结构体 -> {名: 字段集合}（仅模块层级）"""
    dep = build_depth_array(clean, clean)
    out = {}
    for m in re.finditer(r'\bstruct\s+([A-Za-z_]\w*)\s*(?:<[^>{;]*>)?\s*\{', clean):
        name = m.group(1)
        i = m.end() - 1
        d = 0; j = i
        while j < len(clean):
            if clean[j] == '{': d += 1
            elif clean[j] == '}':
                d -= 1
                if d == 0: break
            j += 1
        body = clean[i+1:j]
        fields = set()
        for part in split_top_level(body, ','):
            part = part.strip()
            fm = re.match(r'(?:pub(?:\([^)]*\))?\s+)?([A-Za-z_]\w*)\s*:', part)
            if fm: fields.add(fm.group(1))
        if fields:
            out[name] = fields
    return out

def literal_fields(body):
    """从字面量/模式的花括号体内取出字段名集合与是否有 `..rest`"""
    given = set(); has_rest = False
    for part in split_top_level(body, ','):
        part = part.strip()
        if not part: continue
        if part.startswith('..'):
            has_rest = True; continue
        part = re.sub(r'^ref\s+', '', part)
        part = re.sub(r'^mut\s+', '', part)
        if ':' in part:
            fm = re.match(r'([A-Za-z_]\w*)\s*:', part)
            if fm: given.add(fm.group(1))
        else:
            fm = re.match(r'([A-Za-z_]\w*)\s*(?::|@|$)', part)
            if fm and fm.group(1) not in RESERVED:
                given.add(fm.group(1))
    return given, has_rest

structs_by_crate = {}     # crate -> {结构体名: 字段集}
for cname, info in crate_data.items():
    table = {}
    for f in info["files"]:
        table.update(parse_struct_fields(strip_noise(rd(os.path.join(info["dir"], f + ".rs")))))
    structs_by_crate[cname] = table

for cname, info in crate_data.items():
    table = structs_by_crate[cname]
    if not table: continue
    known = "|".join(sorted(map(re.escape, table), key=len, reverse=True))
    for f in info["files"]:
        src = strip_noise(rd(os.path.join(info["dir"], f + ".rs")))
        # `impl X {` 上下文，用于把 `Self { ... }` 解析成 X
        impl_owner = {}
        for im in re.finditer(r'\bimpl(?:<[^>]*>)?\s+(?:([A-Za-z_]\w*)|(?:[A-Za-z_]\w*\s+for\s+([A-Za-z_]\w*)))[^;{]*\{', src):
            owner = im.group(1) or im.group(2)
            i = im.end() - 1
            d = 0; j = i
            while j < len(src):
                if src[j] == '{': d += 1
                elif src[j] == '}':
                    d -= 1
                    if d == 0: break
                j += 1
            impl_owner[im.start()] = (owner, j)
        def owner_at(pos):
            for start, (own, end) in impl_owner.items():
                if start < pos < end: return own
            return None
        for m in re.finditer(r'\b(%s|Self)\s*\{' % known, src):
            nm = m.group(1)
            start = m.start()
            # 排除若干「不是结构体字面量」的语法位置：
            #   ① `fn f() -> Rect {`   返回类型 + 函数体花括号
            #   ② `impl X {` / `struct X {` / `enum X {` / `impl T for X {`
            #   ③ `where T: X {`       约束子句
            before = src[:start]
            line_pre = before[before.rfind('\n') + 1:]
            if before.rstrip().endswith('->'):
                continue
            if re.search(r'\b(impl|struct|enum|union|trait)\b[^;{}]*$', line_pre):
                continue
            if re.search(r'\bwhere\b[^;{}]*$', line_pre):
                continue
            if re.search(r'\bfor\b[^;{}]*$', line_pre):
                continue
            real = owner_at(start) if nm == 'Self' else nm
            if real not in table:
                continue
            declared = table[real]
            i = m.end() - 1
            d = 0; j = i
            while j < len(src):
                if src[j] == '{': d += 1
                elif src[j] == '}':
                    d -= 1
                    if d == 0: break
                j += 1
            given, has_rest = literal_fields(src[i+1:j])
            ln = src.count('\n', 0, start) + 1
            extra = given - declared
            missing = declared - given
            if extra:
                P("ERROR", "H", "%s/%s.rs:%d  `%s { ... }` 出现未声明字段 -> %s"
                  % (cname, f, ln, real, ", ".join(sorted(extra))))
            elif missing and not has_rest and given:
                P("ERROR", "H", "%s/%s.rs:%d  `%s { ... }` 缺少字段（且无 ..rest）-> %s"
                  % (cname, f, ln, real, ", ".join(sorted(missing))))

# ────────────────────────────────────────────── I. 单一定义约定
# 有些函数/常量必须**全工程只有一处定义**：它们是两个前端共用的「语法」或
# 「口径」，各写一份就会慢慢分叉 —— 而这类分叉不会报错，只会让同一个动作在
# CLI 与界面上表现不一致（历史上已经出现过「路径双重替换」「落盘用合成路径」
# 两次事故，都是同一件事写了两遍）。
#
# 判据是**顶层定义**（行首无缩进）：`impl` 块里的同名方法不算重复，
# 因为那是"给会话再加一个便捷入口"，不是另写一套。
SINGLE_DEF = {
    # 人读写法 → 规则：GUI 的阈值输入框与 CLI 的 --rule 必须同一套语法
    "fn parse_size": "crates/core-monitor/src/spec.rs",
    "fn parse_ms": "crates/core-monitor/src/spec.rs",
    "fn parse_rule": "crates/core-monitor/src/spec.rs",
    # 「体积怎么说」：同一条变更在终端与界面上的数字必须一样
    "fn event_size_text": "crates/core-monitor/src/changelog.rs",
    # 合成路径拼接 / 事件 → 显示路径
    "fn join_path": "crates/core-scan/src/paths.rs",
    "fn event_path": "crates/core-scan/src/paths.rs",
    # FRN → 记录号的掩码（曾经在三个 crate 里各抄一份）
    "const RECNO_MASK": "crates/core-index/src/arena.rs",
}

_def_files = collections.defaultdict(list)
for cname, rel in list(CRATES.items()) + list(BIN_CRATES.items()):
    info = crate_data.get(cname)
    if not info:
        continue
    for f in info["files"]:
        relsrc = os.path.join(rel, "src", f + ".rs").replace(os.sep, "/")
        clean_f = strip_noise(rd(os.path.join(info["dir"], f + ".rs")))
        for what in SINGLE_DEF:
            kind, name = what.split(" ", 1)
            # 行首（可能有 pub / pub(crate)）即顶层定义；缩进的是方法或嵌套项
            pat = r'(?m)^(?:pub(?:\([^)]*\))?[ \t]+)?(?:const[ \t]+)?%s\b' % re.escape(name)
            if kind == "fn":
                pat = r'(?m)^(?:pub(?:\([^)]*\))?[ \t]+)?(?:async[ \t]+)?fn[ \t]+%s[ \t]*[<(]' % re.escape(name)
            if re.search(pat, clean_f):
                _def_files[what].append(relsrc)

for what, want in sorted(SINGLE_DEF.items()):
    got = _def_files.get(what, [])
    name = what.split(" ", 1)[1]
    if len(got) == 0:
        P("ERROR", "I", "`%s` 找不到定义（应在 %s）—— 检查器可能已过时" % (name, want))
    elif len(got) > 1:
        P("ERROR", "I", "`%s` 有 %d 处顶层定义 -> %s；这类共用口径只能有一处（另一处请改成 use 转出）"
          % (name, len(got), ", ".join(got)))
    elif got[0] != want:
        P("WARN", "I", "`%s` 定义在 %s，预期 %s" % (name, got[0], want))

# ────────────────────────────────────────────── 输出
order = {"ERROR": 0, "WARN": 1}
problems.sort(key=lambda x: (order[x[0]], x[1]))
errs = [p for p in problems if p[0] == "ERROR"]
warns = [p for p in problems if p[0] == "WARN"]
print("=" * 78)
print("跨 crate API 一致性检查   根目录: %s" % ROOT)
print("=" * 78)
for cname in list(CRATES) + list(BIN_CRATES):
    d = crate_data.get(cname, {})
    print("  %-14s 模块 %2d   跨 crate 可见符号 %3d   源文件 %d"
          % (cname, len(d.get("mods", [])), len(pub_by_crate.get(cname, set())), len(d.get("files", []))))
print()
if not problems:
    print("  ✅ 未发现问题")
else:
    for lv, chk, msg in problems:
        print("  [%s/%s] %s" % (lv, chk, msg))
print()
print("ERROR %d 项 / WARN %d 项" % (len(errs), len(warns)))
sys.exit(1 if errs else 0)
