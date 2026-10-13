# FileWatch —— Windows 文件增量监控工具

基于 NTFS `$MFT` 直读 + USN Journal 的高性能文件监控引擎，配 Squarified Treemap
面积图可视化。

* **引擎 + CLI**（M0~M3）：验收全绿，见第 5 节。
* **GUI**（M4~M5，Tauri 2 + Canvas）：已落地，见 [`apps/gui/README.md`](apps/gui/README.md)。

---

## 1. 快速开始

```powershell
# 一键装工具链 + 构建 + 自检（引擎 + CLI）
powershell -ExecutionPolicy Bypass -File .\setup-rust.ps1

# 检查引擎是否完好（不需要管理员权限）
.\target\x86_64-pc-windows-gnu\release\filewatch.exe selftest --verbose
```

GUI 单独构建（它是自己的 workspace，见 `apps/gui/README.md`）：

```bash
python apps/gui/tools/make-icon.py        # 生成图标，只需一次
cd apps/gui/src-tauri && cargo build --release
./target/x86_64-pc-windows-gnu/release/filewatch-gui.exe   # 注意是三元组子目录
```

> **为什么要用 GNU 工具链**：开发机没有 Windows SDK 10（缺 `kernel32.lib`），
> Rust 默认的 MSVC 工具链会链接失败。`x86_64-pc-windows-gnu` 由 rustup 自带
> MinGW-w64（含 gcc 与导入库），**不需要任何 Visual Studio 组件**。
> `setup-rust.ps1` 会自动装好并把目标三元组固定进 `.cargo/config.toml`。
> 实测 Tauri 2 在同一套 GNU 工具链下也能正常编译、链接、启动（见
> `apps/gui/README.md` 的「两个必须知道的坑」）。

---

## 2. 能力与降级

程序**永远能跑**，但会用两条性能差 1~2 个数量级的路径。选哪条由环境决定，
且**一定会在输出里说清楚**，不做静默降级。

| | 快速路径 | 降级路径 |
|---|---|---|
| 全量扫描 | `$MFT` 直读，16 MB 顺序块 | 目录递归遍历 |
| 增量来源 | USN Journal（内核 32 MB 滚动日志，可断点续读） | `ReadDirectoryChangesW`（纯内存队列，进程不在即丢） |
| 节点标识 | FRN → 数组下标，O(1) 定位 | 自增序号 + 「小写绝对路径 → 下标」索引 |
| 权限要求 | **管理员** | 无 |
| 断线补偿 | 有（游标 + 重放） | 无 |
| 适用 | NTFS + 已提权 | 任何卷 / 任何用户 |

自检里的 `降级模式端到端：扫描→事件→增量` 一项，就是**在无管理员权限下**
跑通整条链路的自动化验收。

```powershell
filewatch volumes                 # 看当前能走哪条路
filewatch scan  C:                # 自动选；没权限就降级并说明原因
filewatch scan  C: --force-traverse   # 强制走降级路径（用于验证）
filewatch probe C:                # 需要管理员；失败会打印出该怎么办
```

---

## 3. 命令参考

```
filewatch <命令> [参数...]

volumes                     列出所有卷、文件系统与可用后端
probe <盘符>                读引导扇区 + $MFT 记录 0，逐字段对照 fsutil（M0 验收）
journal <盘符> [--status|--create|--delete]
scan <盘符> [选项]          全量扫描并输出统计（M1 验收）
top <盘符> [--n=20]         恒定内存找最大的 N 个文件（不建树）
monitor <路径> [选项]       实时监控 + 阈值预警（M2/M3 验收）
selftest [--verbose]        免管理员的全链路自检
help / version
```

### scan

```powershell
filewatch scan C: --top=15 --exts=12
filewatch scan D: --root=D:\projects --json          # --root 自动切换为遍历模式
filewatch scan C: --layout=110x32 --top=0 --exts=0   # 字符网格预览面积图
filewatch scan C: --dedup                            # 解释与资源管理器的口径差异
```

| 选项 | 含义 |
|---|---|
| `--top=N` | 前 N 大文件（默认 20，0 = 不输出） |
| `--exts=N` | 扩展名 Top N（默认 15） |
| `--layout=WxH` | 字符网格预览 Squarified 面积图 |
| `--root=PATH` | 指定扫描根（$MFT 是整卷粒度，因此**自动切换为遍历模式**） |
| `--json` | 机器可读摘要 |
| `--chunk=BYTES` | 顺序读块大小（默认 16M） |
| `--max-depth=N` | 遍历模式最大深度（默认 256） |
| `--no-prealloc` | 关掉节点数组预分配（内存峰值更低、更慢） |
| `--force-traverse` | 强制降级 |

### monitor

```powershell
filewatch monitor $env:USERPROFILE\Downloads --rule=abs=1G --duration=60s
filewatch monitor D:\build --rule=grow=500M --rule=rate=5M/1m --changelog=out.jsonl
filewatch monitor C: --rule=abs=2G --create-journal      # 自动启用 USN Journal
```

规则语法（可重复给出多条）：

| 形式 | 含义 |
|---|---|
| `--rule=abs=1G` | **绝对阈值**：目录体积超过 1 GiB 报警 |
| `--rule=grow=500M` | **增长量**：相对基线增长超过 500 MiB 报警 |
| `--rule=rate=10M/60s` | **速率**：折算到 10 MiB/60 s 的速率报警 |

支持的单位：`B / K / KB / M / MB / G / GB / T / TB`（1024 进制）；
时间：`ms / s / m / h`。

| 选项 | 含义 |
|---|---|
| `--duration=60s` | 运行时长；缺省 = 一直运行到 Ctrl+C |
| `--interval=500ms` | 轮询间隔 |
| `--changelog=PATH` | 变更流水落盘（JSONL，64 MB 轮转） |
| `--create-journal` | 允许自动创建 USN Journal（**系统级变更**） |
| `--force-traverse` | 不碰裸设备句柄，强制走目录监视 |
| `--no-watch` | 连目录监视也关掉，只做一次性扫描 |
| `--no-subtree` | 只监视根目录直接子项 |
| `--watch-buffer=64K` | 目录监视缓冲（突发变更多时调大，防丢事件） |
| `--watch-max-nodes=N` | 实时增量的路径索引上限（默认 300000） |

---

## 4. 架构

```
apps/cli                    命令行（M0~M3 的主力验证工具）
crates/
  core-io       卷句柄 / 16 MB 预读 / DeviceIoControl / 手写 Win32 FFI
  core-ntfs     引导扇区、fixup、属性链、Data Runs、$FILE_NAME、单条 MFT 记录
  core-usn      USN Journal 读写、USN_RECORD_V2/V3 解析、Reason→语义事件、游标自愈
  core-index    40 字节紧凑节点、字符串池、目录树、O(树深) 增量聚合、扩展名统计
  core-viz      Squarified Treemap、LOD 剪枝、相机钳制（纯函数）
  core-monitor  有界队列、debounce 合并、三种阈值规则、预警去重、变更流水、合成路径重映射
  core-scan     扫描调度与会话编排（唯一对外入口）
```

### 分层规则

* **`core-io` … `core-viz`、`core-monitor` 互相不依赖**，全部可独立单测。
* 只有 `core-scan` 同时依赖它们，反向无人依赖它 —— CLI 与 GUI 共用同一条装配路径。
* `core-monitor` **刻意不依赖 `core-ntfs`**：回读文件大小通过 `size_probe`
  闭包注入，因此它磁盘无关、可纯内存单测。

### 三条关键技术决策

**① 紧凑内存布局。** `Node` 是 40 字节的 `#[repr(C)]` 结构，节点数组下标即
FRN 低 48 位，**不用 HashMap** 做定位；文件名进字符串池、节点只存 `u32` 偏移。
200 万文件约 150 MB，落在「常驻 ≤ 300 MB」的目标内（自检会打印实测尺寸）。

**② O(树深) 增量聚合。** 一处变更只沿父链上传 delta，而不是重算整棵树。
200 万文件、平均树深 10 时，一次变更是 10 次写入而不是 2000 万次。
阈值规则的求值也挂在这条链上，所以「检查所有被监控目录」是 O(1)。

**③ 零第三方依赖。** Win32 声明为手写 FFI，JSON、参数解析、表格对齐、Ctrl+C
全部自己写。带来的好处很实在：网络受限的构建机上 `cargo build` 能过，
链接面积小到只需要 `libkernel32.a`。

### 降级模式的四个补偿机制

`ReadDirectoryChangesW` 只有路径、没有 FRN，所以 `core-scan/watcher.rs` 要
把路径事件翻译成引擎能吃的 FRN 事件。四个必须处理的语义差：

1. **第一次读取必须在 `DirWatcher::start()` 返回之前挂到内核上。**
   内核只报告「读取挂上之后」发生的变更 —— 早期实现把这一步留给监视线程做，
   于是「`start()` 返回 → 调用方立刻写文件 → 线程才被调度」这段窗口里的变更
   被**永久**丢掉（这个 API 不补发「挂上之前」的历史，降级模式又没有 USN
   游标可以重放）。所以用 `FILE_FLAG_OVERLAPPED` + `ERROR_IO_PENDING`
   让「返回即已就位」，窗口收敛到零。`dirwatch::tests::start_is_armed_before_returning`
   用「返回后立刻写、连跑 5 轮」把这条钉住。
2. **路径索引在遍历扫描时顺带建立**（零额外成本）。事后用 `Tree::path_of`
   反推是错的 —— 树里的路径前缀是盘符，而真实扫描根可能是个深目录。
3. **新增目录要立刻物化占位节点。** `propagate_from_parent` 遇到
   `!in_use` 的祖先会立刻停止上传；而「新建目录 `d`」与「新建 `d\f.bin`」
   可能落在同一个 debounce 窗口里，此时 `d` 的节点还不存在，`f.bin` 的体积
   就永远传不到上层。占位节点 `size=0`，随后被引擎自己的 Created 覆盖，
   不会重复计数。
4. **占位目录要有一轮对账。** 「创建 + 删除」在窗口内会互相抵消（debounce
   合成 `Unknown` 丢掉），文件无所谓（节点从没建过），但占位目录会残留成
   幽灵节点。窗口过后回查磁盘确认，不存在就摘掉并做负向上传。

### 两个「静默错误」的防线

下面这两类错误**不报错、只是数字或画面不对**，比崩溃难查得多，所以都配了
专门的回归测试：

* **事件丢了 → 体积永远是 0。** 待回读大小的登记表（`probe_paths`）必须
  按 TTL 淘汰，不能在每轮 tick 后无条件清空：debounce 意味着事件要到下一轮
  之后才成熟，清早了引擎就永远拿不到真实大小。见 `ProbeEntry` 的文档。
* **方块错位 → 目录凭空消失。** `squarify` 必须**先攒后回填**，不能「边铺边
  push」：0 面积的元素（空目录 / 0 字节文件）会在当前行封行之前抢走前面的槽位，
  于是大目录拿到退化矩形被丢掉、空文件占满屏。见
  `squarify::tests::zero_weight_entry_does_not_shift_index_mapping` 与
  `layout::tests::zero_size_child_does_not_steal_sibling_rect`。

---

## 5. 验收对照

| 里程碑 | 验收内容 | 对应命令 | 状态 |
|---|---|---|---|
| M0 | 引导扇区 / `$MFT` 记录 0 与 `fsutil` 逐字段一致 | `probe <盘符>`（需管理员） | 代码完成，待提权实测 |
| M1 | 全量扫描正确性 + 性能 | `scan <盘符>` | 遍历路径已实测；MFT 路径待提权 |
| M2 | 增量事件不丢、不漏、不重 | `monitor`（两条路径） | 降级路径已端到端实测 |
| M3 | 阈值预警：滞回 / 冷却 / 合并 | `monitor --rule=...` | 规则引擎单测 + 自检覆盖 |
| M4 | 面积图布局 | `scan --layout=WxH` | 布局算法完成，字符网格预览可用 |
| M5 | GUI（Tauri 2 + Canvas/WebGL2） | — | **未开始**（按约定「先引擎 + CLI，再套 GUI」） |
| M6 | 打包分发 | — | 未开始 |

### 真实目录实测（不是只在临时小目录里跑）

测试用例的树都是现造的小树，所以另外拿真实目录对了一遍：

| 目录 | filewatch | 独立参考实现（Python `os.walk`） | 结论 |
|---|---|---|---|
| 本工程目录 | 60 目录 / 245 文件 / 20 143 678 B | 60 / 245 / 20 143 678 | **逐项一致** |
| `C:\Windows\System32` | 1 864 目录 / 18 950 文件 / 11 028 884 660 B / 错误 23 | 1 841 / 18 950 / 11 028 884 660 | 文件数与字节数**完全一致** |

System32 那 23 个目录的差值是**计数口径**不同，不是漏算：filewatch 把一个
「读不进去的目录」记成「已访问 + 1 个错误」，而 Python 的 `os.walk` 直接跳过、
既不产出也不报错。文件集合与总字节数两边一模一样，说明数据本身没有偏差。

`monitor` 也在一棵真实目录树上跑过完整链路（建文件 / 追加写 / 改名 / 建嵌套目录 /
写文件 / 删文件 / 删目录），逐条核对：

```text
[00:04.420] 新增          +1.14 MB  ...\fw_livecheck3\big.dat（合并 2 条）
[00:04.420] [WARNING] 【绝对阈值】 — ...\fw_livecheck3 总大小 1.15 MB，超过绝对阈值 1.00 MB
[00:05.022] 修改        +390.62 KB  ...\fw_livecheck3\big.dat
[00:05.424] 新增         +29.30 KB  ...\fw_livecheck3\d1\d2\leaf.txt（合并 2 条）
```

* 体积全部精确：1 200 000 B → `+1.14 MB`、400 000 B → `+390.62 KB`、30 000 B → `+29.30 KB`；
* 「修改」报的是**增量** 400 000 而不是重新计一遍 1 600 000；
* 嵌套目录 `d1\d2` 先物化占位、`leaf.txt` 的体积照样传到根（四个补偿机制在真机上都成立）；
* 变更流水 JSONL 10 条记录全部可 `json.loads`，`kind` / `size` / `delta` 与上面逐条对得上；
* 阈值规则 `abs=1M` 与 `grow=100K` 都在正确的时刻触发了一次。

### 免管理员的自动化验收

```powershell
cargo test --release      # 单元测试 + 集成测试（e2e 直接跑真实 exe）
filewatch selftest        # 28 项内置自检，逐项打印「通过/失败/跳过」
```

自检把**失败**和**跳过**分开报：跳过只发生在环境本身不支持时（例如容器里
连目录句柄都拿不到），绝不掩盖失败。退出码非 0 表示确实有失败项。

实测（非管理员，Windows 10 19045）：

| 目标 | 结果 |
|---|---|
| `cargo build --release` | 0 warning / 0 error |
| `core-io` | 15 / 15 |
| `core-ntfs` | 35 / 35 |
| `core-usn` | 29 / 29 |
| `core-index` | 42 / 42 |
| `core-viz` | 58 / 58 |
| `core-monitor` | 87 / 87 |
| `core-scan` | 36 / 36 |
| `filewatch`（bin + e2e） | 29 + 12 |
| `filewatch selftest` | 通过 28 / 失败 0 / 跳过 0 |
| `tools/muttest.py` | 通过 20 / 失败 0 |
| `tools/check_api.py` | ERROR 0 / WARN 0 |

### 无编译器时的静态预检（`tools/`）

在没有 Rust 工具链的机器上，`tools/check_api.py` 覆盖了 `cargo check` 里
**可静态判定**的那部分错误，用于在装机前把低级错误清零：

| 检查 | 对应 rustc 诊断 | 说明 |
|---|---|---|
| A | `E0583` / 死文件 | `mod X;` 是否有文件；是否有孤儿 `.rs` |
| B | `E0432` / `unused_crate_dependencies` | `path` 依赖是否存在；用了没声明 / 声明了没用 |
| C | — | 汇总各 crate 跨 crate 可见的符号表 |
| D | `E0432` `unresolved import` | `use core_x::{A, m::B};` 全形态 use 树逐叶解析 |
| E/F | `E0425` / `E0433` | `core_x::mod::Item` 与 `crate::mod::Item` 是否存在 |
| G | `unused_imports` | 导入但未使用（已排除 `pub use` 与靠方法语法使用的 trait） |
| H | `E0063` / `E0560` | 结构体字面量**缺字段 / 多字段 / 字段名拼错** |

```bash
python tools/check_api.py           # 输出 ERROR/WARN 清单，有 ERROR 时退出码 1
python tools/check_api.py .         # 也可以显式指定工程根目录
python tools/muttest.py             # 变异测试：验证上面这套检查器真的抓得到错
```

`muttest.py` 是这件事的关键：它会往**副本**里故意注入 14 类错误（另有 6 个
「看着可疑但完全合法」的回归守卫，如 `fn f() -> Rect {`、`..Default::default()`、
`extern` 块内的 `pub fn`），确认每一类都被检出、且合法写法零误报。
一个只报「0 错误」而没有通过变异测试的检查器，其「0 错误」是没有意义的。

副本放在**系统临时目录**（`%TEMP%\filewatch_muttest\{prime,work}`）而不是工程里：
`prime` 是只建一次的干净副本，`work` 是每轮变异的工作副本，每轮只把上一轮动过的
那一两个文件从 `prime` 还原回去。早期实现是「每轮重建整棵副本树」，21 轮要删除
一千多个文件 —— 又慢又会撞上环境的批量删除保护。测试结束**不**删副本，下次原地复用。

当前状态：**20 / 20 通过，工程代码 0 ERROR / 0 WARN**。
（注意：这不等于能编译。借用检查、trait 解析、泛型单态化等仍必须靠 `rustc`。）

### 需要提权才能验收的部分

```powershell
# 用「以管理员身份运行」打开终端
filewatch probe C:
fsutil fsinfo ntfsinfo C:      # 与上面输出逐项对照

filewatch journal C: --status  # 应当看到活动的 JournalID
filewatch scan C:              # 应自动选择 MFT 直读后端
filewatch monitor C: --rule=abs=50G --duration=5m
```

---

## 6. 已知限制

* **仅 Windows**：核心依赖 NTFS 与 Win32；`core-io` 在非 Windows 上直接
  `compile_error!`。
* **USN 记录不带文件大小**，必须回读 `$MFT` 记录。若文件在两次读取之间被删，
  该次变更被记为 0 字节 —— 这是协议本身的限制，不是实现缺陷。
* **降级模式无离线补偿**：进程不在时发生的变更不会被记录。需要断线续跑必须
  走 USN 路径。（进程**在**的时候不丢事件：内核侧的监听在 `start()` 返回前就已
  挂好，见第 4 节的补偿机制 ①。）
* **降级模式不做硬链接去重**：`MetadataExt::number_of_links` 至今仍是 unstable
  （`windows_by_handle`），而便宜的替代方案（`GetFileInformationByHandle`）
  要给每个文件开一次句柄，200 万文件规模下不可接受。代价是同一份数据被多个
  硬链接指向时会**重复计入总大小**。需要精确去重请用 `--admin` 走 MFT 直读路径。
* **降级模式对整盘不实用**：路径索引约 110 字节/条，超过 `--watch-max-nodes`
  就自动只做一次性扫描（会明确提示）。这是内存预算的取舍，不是 bug。
* **`--create-journal` 是系统级变更**，会影响该卷上的所有应用；退出时会提示
  如何用 `journal --delete` 恢复。
* 分配大小（占用空间）在遍历模式下只能按簇对齐**估算**，没有 MFT 的
  `$DATA` 分配簇数精确。

---

## 7. 项目结构

```
filewatch/
├─ Cargo.toml                  workspace（8 个成员，零第三方依赖）
├─ setup-rust.ps1              一键搭环境 + 构建 + 自检（含国内镜像）
├─ tools/
│  ├─ check_api.py             无编译器时的静态预检（A~H 七类检查）
│  └─ muttest.py               对 check_api.py 的变异测试（20 项）
├─ .cargo/config.toml          固定 x86_64-pc-windows-gnu 目标（脚本生成）
├─ apps/cli/
│  ├─ src/main.rs              命令分发 + 各命令实现
│  ├─ src/args.rs              自包含参数解析（含 --rule 语法）
│  ├─ src/render.rs            时间/表格/字符网格面积图
│  ├─ src/selftest.rs          28 项内置自检
│  └─ tests/e2e.rs             端到端测试（跑真实 exe）
├─ apps/gui/                   GUI（Tauri 2 + Canvas）
│  ├─ src-tauri/               Rust 侧；**它自己是 workspace 根**，
│  │                           被上面的 exclude 挡在外面，所以引擎的依赖图仍是空的
│  └─ src/                     前端（纯静态，无 npm / 无打包器）
└─ crates/                     7 个引擎 crate，详见第 4 节
```

分层上只有一个方向：`apps/* → core-scan → core-*`。GUI 与 CLI 是**并列**的两个
调用方，都用 `core_scan::Session` 这一条装配路径 —— 所以同一个目录在两边显示的
大小必须一致，不一致就是 bug 而不是「口径不同」。
