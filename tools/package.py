#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把已在 release 下构建好的产物收成一个「解压即用」的便携包（M6 的一部分）。

为什么需要这一步，而不是直接把 exe 拷给用户：

1. **GUI 不是单文件。** `filewatch-gui.exe` 会在启动时动态导入
   `WebView2Loader.dll`（`webview2-com-sys` 的构建脚本把它复制到 exe 旁边）。
   只拷 exe 的话，用户拿到的是「双击没反应/提示缺 DLL」——很难自己查到原因。
2. **名字要像产品。** 构建产物叫 `filewatch-gui.exe`，发给别人时叫 `FileWatch.exe`
   更清楚；CLI 则收进 `cli/` 子目录，不跟 GUI 抢同一个位置。
3. **把「怎么运行」写在包里。** 这个程序有降级模式和 UAC 申请这两件事，
   不看说明会以为它坏了。

用法：

    python tools/package.py            # 打包
    python tools/package.py --name FileWatch-optimized # 指定本次新包名称
    python tools/package.py --dir-only # 只铺目录，不压缩

产物：

    dist/FileWatch-<版本>-win64/        解压即用的目录
    dist/FileWatch-<版本>-win64.zip     分发用压缩包
"""

from __future__ import annotations

import argparse
import shutil
import re
import secrets
import sys
import time
import zipfile
from pathlib import Path

# ── 路径常量 ────────────────────────────────────────────────────────────────

ROOT = Path(__file__).resolve().parent.parent

# 目标三元组写死会过期（引擎已经固定成 x86_64-pc-windows-gnu），所以按候选顺序找。
GUI_CANDIDATES = [
    ROOT / "apps/gui/src-tauri/target/x86_64-pc-windows-gnu/release",
    ROOT / "apps/gui/src-tauri/target/release",
]
CLI_CANDIDATES = [
    ROOT / "target/x86_64-pc-windows-gnu/release",
    ROOT / "target/release",
]
DIST = ROOT / "dist"

GUI_EXE = "filewatch-gui.exe"
CLI_EXE = "filewatch.exe"
WEBVIEW2_DLL = "WebView2Loader.dll"


class PackageError(RuntimeError):
    """打包条件不满足。文案直接给用户看，所以要说清楚「该跑什么命令」。"""


# ── 小工具 ──────────────────────────────────────────────────────────────────


def workspace_version() -> str:
    """从根 `Cargo.toml` 的 `[workspace.package]` 里读版本号。

    不硬编码：版本号是发布信息的一部分，两处维护迟早不一致。
    只认 `version = "..."` 这一行，不做完整的 TOML 解析（零依赖）。
    """
    cargo = ROOT / "Cargo.toml"
    if not cargo.exists():
        return "0.1.1"
    in_pkg = False
    for line in cargo.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_pkg = stripped == "[workspace.package]"
            continue
        if in_pkg and stripped.startswith("version"):
            _, _, value = stripped.partition("=")
            return value.strip().strip('"').strip("'") or "0.1.1"
    return "0.1.0"


def find_in(candidates: list[Path], name: str) -> Path:
    """返回**包含** `name` 的那个目录（不是 `name` 本身）。

    返回值必须是目录：调用方要连着取同一目录下的其它文件（exe 与它旁边的 DLL）。
    一开始这里 `return p` 返回了文件，于是 `gui_dir / GUI_EXE` 拼成
    `...\\release\\filewatch-gui.exe\\filewatch-gui.exe`，报 WinError 3 ——
    错误信息只说「找不到路径」，看不出是这里拼错。
    """
    for d in candidates:
        if (d / name).exists():
            return d
    looked = "\n".join(f"  - {d / name}" for d in candidates)
    raise PackageError(
        f"找不到 {name}，已查找：\n{looked}\n"
        f"请先构建：\n"
        f"  cargo build --release\n"
        f"  cd apps/gui/src-tauri && cargo build --release\n"
        f"（gui 的目标目录会继承引擎根 .cargo/config.toml 里的目标三元组）"
    )


def human(n: int) -> str:
    v = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if v < 1024 or unit == "GB":
            return f"{v:.2f} {unit}" if unit != "B" else f"{int(v)} B"
        v /= 1024
    return f"{v:.2f} GB"


README_NAME = "使用说明.txt"


def readme_text(version: str, stamp: str) -> str:
    """包内说明。**刻意写短**：用户只在第一次打开时会读它。"""
    return f"""FileWatch {version}（Windows 64 位便携版）
打包时间：{stamp}

────────────────────────────────────────────────
一、怎么打开
────────────────────────────────────────────────
双击本目录下的  FileWatch.exe  即可，不需要安装。

  不要只把 FileWatch.exe 单独拷到别处 —— 它启动时要读取同目录的
  WebView2Loader.dll。两个文件必须放在一起。

第一次启动会弹一个「是否以管理员身份运行」的窗口：

  选「以管理员身份重启」→ Windows 会再弹一个确认框 → 点「是」就行。
     直读 NTFS 的 $MFT，百万文件的盘通常 1~3 秒扫完，并能做真正的实时增量监控。

  选「以降级模式继续」→ 直接进界面。
     靠目录遍历建立索引，增量由 Windows 目录变更通知提供。

两个选择随时可以改：界面右下角的「权限」按钮里可以重新申请。
也可以在窗内勾「下次进入时不再自动询问」。

扫描入口只有两处：点左侧的盘符卡片，或「文件 → 选择文件夹…」（选完即扫）。
「界面」菜单：统一调整主题、密度、布局、着色、统计口径与细节。
「模式」菜单：运行模式 + 开机自启动 + 关闭软件。
其中外观与模式选项会自动保存：
  主题：跟随系统 / 浅色 / 深色。
  密度：舒适 / 紧凑。
  布局：完整布局 / 专注分析（收起右侧统计栏）。
  运行模式：标准 / 轻量（省内存，看得粗）/ 性能（后台周期刷新，会吃一点 IO）。
  开机自启动：勾选后登录 Windows 时自动启动（写在 HKCU 的 Run 键里，不需要管理员）。
  关闭软件：最小化到托盘（监控继续跑）／关闭所有进程（默认，直接退出）。
    选了托盘之后，退出走**托盘图标**的右键菜单 —— 那条路一直在。

实时监控可选「联动面积图」或「独立监控 · 固定路径」。联动模式的目标须是已扫描
的目录；若提示事件丢弃或监视中断，请重新扫描后继续监控，以校准统计。
独立模式最多管理 32 个文件/文件夹，支持新增、修改、暂停/启用和移除；无需先扫描，
可与盘符扫描同时运行，不占用扫描会话。只监听通知和读取元数据，不读取文件内容、
不锁定写入。文件夹需要建立独立索引，大目录会占用更多时间与内存。
配置自动保存，下次启动恢复已启用目标；移除监控不会删除文件，内存流水退出后清空。
流水与预警显示当地完整日期时间。阈值规则旁的「?」可查看说明：规则之间是「或」，
严格超过阈值触发，默认冷却 10 分钟，且需回落到阈值 90% 以下才可再次触发。
规则旁边有「规则试算」：拿当前索引跑一遍这套规则，立刻知道会不会一上来就刷屏、
离阈值还差多少。绝对阈值当场就能判；增量与速率规则要监控跑起来才有意义，
试算会直接标成"要跑满一个窗口才能判断"，不会假装给出一个数。
阈值命中时会提醒你，窗口收起来也看得见：
  * 系统通知（Windows 通知中心，显示在右下角/通知列表里）；
  * 托盘图标闪动（右上角出现红点，闪 6 次后恢复）；
  * 托盘图标右键菜单里会列出最近 5 条预警，点一条就打开窗口并切到监控页。
    系统设置里关掉了通知的话，后两条仍然有效。
左侧「文件夹树 / 实时监控」页签可以切换；切到监控后，右侧同时显示变更流水
和阈值预警。切回文件夹树则恢复文件类型与大文件统计。
各列标题栏统一高度，右栏切换保持等宽。面积图保留旧画面等待新布局与文字准备，
然后柔和过渡；系统开启减少动态效果时不播放渐变。
文件夹树顶部可输入已扫描目录的完整路径，按 Enter 或「→」前往；无匹配会提示，
不会退回上级目录。方块数移至面积图路径栏右侧。双击大文件可前往其所在目录。
文件夹树、面积图方块、大文件、面包屑与详情支持右键：查看详情、目录导航、
**打开方式…**、用默认程序打开（仅文件）、在文件资源管理器打开、资源管理器定位、
复制路径/名称/大小、加入独立监控等。
盘符可右键扫描或复制容量；文件类型可右键高亮；监控目标可编辑、启停或移除配置；
流水、预警与快照对比的差异行可复制完整记录或本行变化。
菜单支持 Shift+F10/菜单键与方向键、Enter、Esc。输入框保留原有文字编辑菜单。

「打开方式…」弹出 Windows 自己的"你要如何打开这个文件？"对话框，挑哪个程序
由你在那个对话框里决定；「用默认程序打开」等于双击。可执行文件（exe、bat、
cmd、ps1、vbs、lnk、reg 等）只给「打开方式…」，不给一键执行。文件夹没有这两项。

「在文件资源管理器打开」与「在资源管理器中定位」不是一回事：
前者打开这个文件夹（目标是文件时打开它所在的目录），后者打开上级目录并**选中**
目标。两者都不会执行文件；目标不存在时退到仍存在的上级目录并提示。

扫完之后，界面中间区顶部有六个页签：

  面积图    —— 每个方块是一个文件夹或文件，面积就是它自己（目录是子孙合计）的大小。
              滚轮下钻、Esc 返回。扫描过程中就会「扫到多少画多少」：
              半成品只画第一层，扫完自动换成完整面积图。
  重复文件  —— 内容完全相同的文件归成一组，给出"删到只剩一份能回收多少"。默认只比
               ≥ 1 MB 的文件。它**只查找、不删除**，删不删由你自己决定。
  快照对比  —— 先把现在存成一份快照，过一段时间再回来跟当前结果对比，
              看谁新增了、谁变大了。回答"这一周到底是什么把盘吃满了"。
              结果表有 类型 / 变化 / 文件名 / 修改时间 / 创建时间 / 所在目录 六列，
              文件名过长时从中间省略（扩展名一直看得见）。
              上面还有"增长归因"三栏：增长最多的目录（聚合到前三层）、变化幅度最大的
              类型、变化最集中的时段。明细答"哪个文件变了"，这三栏答"哪块地方在长"。
              这一块可以用标题行右侧的箭头折叠（收起后标题行会留一句摘要）；
              在任意一行上点右键，可以定位目录、复制路径或复制这一行的归因。
              「文件 → 导出 HTML 报告…」可以把当前扫描结果导成一个 HTML 文件：
              概览 + 目录面积图 + 最大目录/文件/类型三张榜。断网也能打开，可直接转发。
  风险清单  —— 按文件名与扩展名列出可能不该躺在这台机器上的东西：SSH/签名私钥、
              密码库、钱包、云与代码托管凭据、.env 这类密钥配置、包管理器令牌、证件扫描件。
              按 高/中 分级。它**只看文件名，不读文件内容**，所以"没找到"不等于"没有风险"；
              命中也不等于"有问题"，它只是指出值得看一眼的位置。全程只读，不删不改。
  增长趋势  —— 每天留一个很小的"目录级"样本（不是文件清单），画出总量曲线，
              并用前 7 天的日增量当基线，回答"今天哪个目录长得不正常"。
              基线不足 3 天时只写"还差 N 天"，不会给一个看起来很像结论的 z 值。
              性能模式下每次扫描后自动记一次（同一目标 30 分钟最多一次）；
              其它模式点「记录一次」才写，标准模式不该偷偷往你盘上写东西。
  文件搜索  —— 在**已扫描的索引**里搜文件名，不重新遍历磁盘，所以很快。
              方式：包含（子串）/ 模糊（按顺序的字符，rpt 命中 report）/ 正则
              （支持 . * + ? [abc] [a-z] [^…] ^ $ 与反斜杠转义）。
              范围：文件 / 文件夹 / 两者。回车即搜，双击前往所在目录，右键有更多操作。
              「内容」那一行是在文件**里面**找（纯文本类：txt / 日志 / CSV / MD / 源码…，
              含 UTF-16 的 txt）。只搜文本：Office 文档（docx/xlsx/pptx）本身是压缩包，
              本程序不引第三方库解析，暂不支持。单个文件最多读 4 MiB、
              最多考虑 2 万个文件（读盘要几秒到几十秒，所以有进度条）。
              第三行是**附加条件**，名称搜索与内容搜索用的是同一套：
                体积 ≥ / ≤  —— 写法 10M / 512K / 1073741824（1024 进制）
                修改于 / 至  —— 写法 7d（七天内）/ 12h / 30m / 2w，
                               或 2026-01-01、2026-01-01 12:30（按本地时间认）
                扩展名      —— mp4,mkv,avi（逗号或分号分隔，大小写随意）
              留空就是不限；看不懂的写法会当场报错，不会悄悄当成"没填"。
              生效的条件会写在结果摘要里；扩展名会与内容搜索的纯文本白名单取交集。
              结果每行前面有勾选框，列头那个是全选（只作用于列出来的这些）。
              勾上之后会出现一条动作条：复制路径（每行一个完整路径）、
              导出 CSV（列与命令行的 search --csv 完全一致，时间是本地时间）、清空选择。
              换一批搜索结果会自动清掉旧勾选 —— 免得导出的是上一批的路径。

快捷键：Ctrl+F 进文件搜索（光标直接落在输入框）；F5 重扫当前根
（默认的 F5 在网页里是刷新页面，会把整份索引丢掉，这里已经挡掉了）；
Esc 收起打开的一层（先菜单、再弹窗），连按才逐层退。

────────────────────────────────────────────────
二、系统要求
────────────────────────────────────────────────
* Windows 10 1809 及以上 / Windows 11，64 位。
* 需要 Microsoft Edge WebView2 运行时。Win11 与近几年的 Win10 已随系统预装；
  如果双击后提示缺少 WebView2，去微软官网装「Evergreen Runtime」即可。

────────────────────────────────────────────────
三、这个程序做了什么、没做什么
────────────────────────────────────────────────
扫描与监控读取目录项、文件元数据；重复文件分析会读取文件内容。
程序不修改、移动或删除源文件；日志、快照、CSV 和偏好会写入对应保存位置。
也不会擅自创建 USN Journal
（那是系统级变更，需要你在界面里明确勾选）。

────────────────────────────────────────────────
四、命令行版本
────────────────────────────────────────────────
cli/ 子目录里是同一个引擎的命令行前端，适合脚本化：

  cli\\filewatch.exe volumes            列出所有卷与可用后端
  cli\\filewatch.exe scan C: --top=20   全量扫描并输出统计
  cli\\filewatch.exe monitor D:\\projects --rule=abs=1G
  cli\\filewatch.exe selftest           免管理员的全链路自检
  cli\\filewatch.exe dupes D:\\photos   找出重复文件
  cli\\filewatch.exe snapshot D: --out=snap.tsv
  cli\\filewatch.exe diff old.tsv new.tsv   （--attrib 多打三个归因榜）
  cli\\filewatch.exe report D:\\projects --out=r.html   单文件离线报告
  cli\\filewatch.exe sensitive C:                     按文件名盘点敏感文件
  cli\\filewatch.exe history C: --record              记一次目录历史（趋势 / 基线异常）
  cli\\filewatch.exe search D:\\photos --text=mp4 --min-size=100M --exts=mp4,mkv
  cli\\filewatch.exe content D:\\src --text=TODO --exts=rs,ts      纯文本内容搜索
  cli\\filewatch.exe rules D:\\data --rule=abs=1G --rule=rate=10M/60s   规则试算

search / content 的条件语法（界面与 CLI 同一套）：
  体积   --min-size=10M / --max-size=1G   （1024 进制，也接受纯字节）
  时间   --after=7d / --before=2026-06-30 （相对 7d/12h/30m/2w 或绝对日期，本地时区）
  扩展名 --exts=mp4,mkv                   （大小写随意，带不带点都行）
  看不懂的写法当场报错退出，不会静默当成「不设这个条件」。

位置参数怎么解释：只有 c / C: / C:\\ 三种写法算「盘符」→ 扫整卷；
其它一律当目录（相对路径按当前目录算），而且必须存在：
  cli\\filewatch.exe search crates --text=main   只扫 crates 这一个目录
  cli\\filewatch.exe report . --out=r.html       只扫当前目录
写错名字会直接报错退出，不会悄悄变成"扫整卷"。

详细说明见项目仓库的 README.md。
"""


# ── 打包主体 ────────────────────────────────────────────────────────────────


def reserve_output(version: str, name: str | None = None) -> Path:
    """Claim a new output directory; never move, erase, or reuse an old package."""
    base = name or f"FileWatch-{version}-win64"
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,100}", base):
        raise PackageError("Package name must be a single filename using letters, digits, '.', '-' or '_'.")
    DIST.mkdir(parents=True, exist_ok=True)
    for attempt in range(100):
        suffix = "" if attempt == 0 else "-" + time.strftime("%Y%m%d-%H%M%S") + "-" + secrets.token_hex(3)
        out = DIST / (base + suffix)
        if out.with_name(out.name + ".zip").exists():
            if name:
                raise PackageError(f"Package archive already exists; choose a new --name: {out.name}.zip")
            continue
        try:
            out.mkdir()  # Atomic claim; an existing directory is never reused.
        except FileExistsError:
            if name:
                raise PackageError(f"Output already exists; choose a new --name: {out}")
            continue
        return out
    raise PackageError("Could not claim a new output directory.")


def build_dir(name: str | None = None) -> Path:
    version = workspace_version()
    gui_dir = find_in(GUI_CANDIDATES, GUI_EXE)
    cli_dir = find_in(CLI_CANDIDATES, CLI_EXE)
    # 找 DLL 也走「哪个目录里有它」——它与 GUI 的 exe 同目录，两个文件必须一起发。
    dll_dir = find_in(GUI_CANDIDATES, WEBVIEW2_DLL)

    out = reserve_output(version, name)
    (out / "cli").mkdir()

    shutil.copy2(gui_dir / GUI_EXE, out / "FileWatch.exe")
    shutil.copy2(dll_dir / WEBVIEW2_DLL, out / WEBVIEW2_DLL)
    shutil.copy2(cli_dir / CLI_EXE, out / "cli" / CLI_EXE)

    stamp = time.strftime("%Y-%m-%d %H:%M:%S")
    # UTF-8 BOM：老版本的记事本没有 BOM 会把中文读成乱码
    (out / README_NAME).write_text(readme_text(version, stamp), encoding="utf-8-sig")
    return out


def make_zip(folder: Path) -> Path:
    # 不能用 `with_suffix(".zip")`：目录名里有版本号，最后一个点会被当成后缀，
    # `FileWatch-0.1.0-win64` 会被截成 `FileWatch-0.1.zip` —— 名字里
    # 少了 win64 与修订号，用户根本分不清是哪个包。直接拼字符串。
    zip_path = folder.parent / (folder.name + ".zip")
    # 压缩包内多一层同名目录：解压时不会把文件撒得满地都是
    with zipfile.ZipFile(zip_path, "x", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for p in sorted(folder.rglob("*")):
            if p.is_file():
                z.write(p, Path(folder.name) / p.relative_to(folder))
    return zip_path


def main() -> int:
    ap = argparse.ArgumentParser(description="打包 FileWatch 便携版")
    ap.add_argument("--dir-only", action="store_true", help="只铺目录，不生成 zip")
    ap.add_argument("--name", help="本次新包的目录名；同名目录或压缩包已存在时拒绝覆盖")
    opts = ap.parse_args()

    try:
        folder = build_dir(opts.name)
    except PackageError as e:
        print(f"打包失败：{e}", file=sys.stderr)
        return 1

    print(f"目录：{folder}")
    total = 0
    for p in sorted(folder.rglob("*")):
        if p.is_file():
            size = p.stat().st_size
            total += size
            print(f"  {human(size):>10}  {p.relative_to(folder)}")
    print(f"  {'─' * 10}")
    print(f"  共 {human(total)}")

    if not opts.dir_only:
        zp = make_zip(folder)
        print(f"\n压缩包：{zp}（{human(zp.stat().st_size)}）")
        print("解压后双击 FileWatch.exe 即可运行。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
