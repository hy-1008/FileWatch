# FileWatch GUI（Tauri 2 + Canvas）

引擎的可视化前端。**薄壳**：所有算法（扫描、聚合、布局、阈值）都在 `crates/**`，
这里只负责把引擎状态暴露成命令、把布局结果按二进制推给前端、把窗口开起来。

```
apps/gui/
├─ src-tauri/          Rust 侧（Tauri 应用）
│  ├─ src/main.rs      入口 + 命令注册 + `--check-elevation` 诊断
│  ├─ src/commands.rs  前端能调的命令（对应文档 F1~F12，另有 F0 权限）
│  ├─ src/state.rs     扫描结果的共享状态
│  ├─ src/wire.rs      Rust → 前端的二进制布局协议（含对拍测试）
│  ├─ src/elevate.rs   管理员权限：检测 + runas 重启（手写 FFI）
│  ├─ src/prefs.rs     GUI 偏好落盘（%APPDATA%\FileWatch\gui-prefs.json）
│  └─ icons/icon.ico   必需（见下）
├─ src/                前端（纯静态，无打包器、无 npm）
│  ├─ index.html
│  ├─ style.css
│  ├─ wire.js          二进制协议的解码端
│  ├─ admin-rule.js    权限弹窗的触发规则（纯函数，可单独跑真值表）
│  ├─ treemap.js       Canvas 渲染 + 命中测试 + 交互
│  └─ app.js           状态与各面板
└─ tools/
   ├─ make-icon.py     生成 icons/icon.ico
   ├─ check-wire.js    线格式校验（按文档偏移造字节流 ←→ wire.js 解码）
   └─ check-admin.js   权限触发规则真值表 + 与 Rust 侧字段对账
```

## 构建与运行

```bash
# 1) 图标（只需生成一次；改了图案再跑）
python apps/gui/tools/make-icon.py

# 2) 编译（首次要下几百个依赖，约 15~20 分钟；之后增量几秒）
cd apps/gui/src-tauri
cargo build --release

# 3) 运行
./target/x86_64-pc-windows-gnu/release/filewatch-gui.exe
```

**产物在 `target/x86_64-pc-windows-gnu/release/` 而不是 `target/release/`** ——
因为引擎根的 `.cargo/config.toml` 把目标固定成了 `x86_64-pc-windows-gnu`，而
cargo 的配置是**从当前目录逐级向上**读取的，`src-tauri` 会继承到那一条。
（这与 CLI 的产物布局一致，是有意的。）

前端是**静态文件**（`tauri.conf.json` 的 `frontendDist` 直接指向 `../src`），
所以没有 Node / npm / Vite 这一套 —— 改完 HTML/JS 重新构建一次即可，
不需要 `tauri dev`。这也意味着不用装任何 npm 包。

### 自检

```bash
# Rust 侧：引擎 → 布局 → 二进制 的端到端，以及参数映射、权限检测、偏好落盘
cd apps/gui/src-tauri && cargo test --release

# 前端侧：解码端与线格式文档是否一致（801 条断言）
node apps/gui/tools/check-wire.js

# 前端侧：权限弹窗的触发规则真值表（20 条断言）
node apps/gui/tools/check-admin.js

# 权限现状（不进界面、不开窗口，直接打 stdout）—— 排查 UAC 问题用
./target/x86_64-pc-windows-gnu/release/filewatch-gui.exe --check-elevation
```

界面点击没法自动化，所以「Rust 编码 / JS 解码」这条跨语言契约单独用
`check-wire.js` 钉住：它按**文档里的偏移**手工造字节流，再用 `wire.js` 解回来
逐字段对拍。加上 `wire.rs` 自己的往返测试，两侧都各自钉在同一份文档上。

`check-admin.js` 是同一思路用在权限弹窗上：那条触发规则只有三条分支，但其中
两条（"本次启动已尝试过提权"、"令牌未提权却仍算有权限"）**只在关了 UAC 的机器上
才走得到** —— 在这台开发机上永远点不出来。所以它把 3×2 的组合全跑一遍，
并顺带从 `commands.rs` 里抠出 `ElevationInfo` 的字段名做对账。

## 管理员权限（F0：进入软件时申请）

文档 R2 的原方案是嵌 `requireAdministrator` 清单自动提权。那个方案的问题不是
"做不到"，而是**代价**：每次双击都要过一遍 UAC（包括只想看一眼面积图的时候），
而且提权进程与资源管理器不在同一个完整性级别，拖放交互会静默失效。

这里改成**进入软件时问一次**：

| 情形 | 行为 |
|---|---|
| 已有管理员权限 | 直接进界面，状态栏显示绿色「权限：管理员」 |
| 在管理员组、被 UAC 过滤 | 弹窗 → 点「以管理员身份重启」→ Windows 弹确认框，**点「是」即可，不用输密码** |
| 不在管理员组 | 弹窗，但文案说明需要输入管理员账户的密码 |
| 用户点「以降级模式继续」 | 照常进界面，橙色「权限：降级模式」；点它可随时重新申请 |

实现要点（都在 `src-tauri/src/elevate.rs`）：清单保持 `asInvoker`，用户同意后用
`ShellExecuteW(verb="runas")` 重启自己；`ShellExecuteW` 会阻塞到用户在 UAC 框上
做出选择，所以整个动作放在**独立线程**里跑，结果用 `admin:relaunch` 事件回传，
界面在此期间不冻结。

三个容易写错、已经钉住的地方：

1. **不能只看令牌。** 系统关掉 UAC 时 `TokenElevationType` 是 `Default`、
   `TokenIsElevated` 为 0，但进程其实拿着完整令牌、卷照样打得开。所以判据是
   **令牌 ∨ `\\.\C:` 能不能打开**。只信令牌会在这类机器上每次启动都追问一遍。
2. **必须防死循环。** 重启时给新进程塞 `--elevation-attempted`；新进程见到它就
   不再自动弹窗（只提示一次"提权未生效"）。否则在"提权无效"的机器上会
   「弹窗 → 重启 → 又弹窗」转圈。
3. **命令行参数要正确加引号。** `--root=C:\Program Files\x` 直接拼会被拆成两个
   参数，新进程拿到错路径 —— 而且错得很安静。`elevate::quote_arg` 按
   `CommandLineToArgvW` 的规则处理（含"结尾连续反斜杠要翻倍"这条），单测覆盖。

「不再自动询问」落在 `%APPDATA%\FileWatch\gui-prefs.json`。读取一律容错
（文件缺失/截断/被手改成非法 JSON 都退回默认值）—— 一个偏好文件不该有能力让
程序起不来。**"不再自动问"不等于"没地方问"**：状态栏的按钮任何时候都能把窗调
回来，所以用户不会被自己的选择锁死。

### 依赖从哪来（本机的网络情况）

引擎层零依赖，所以 CLI 从不需要联网；**GUI 是第一个真的需要 crates.io 的地方**。
本机实测：

| 目标 | 结果 |
|---|---|
| `index.crates.io`（索引） | 200 |
| `static.crates.io`（真正下载 `.crate`） | **不可达** |
| `registry.npmjs.org` | **不可达** |
| `mirrors.ustc.edu.cn/crates.io`（索引 + 下载） | 200 ✅ |
| `registry.npmmirror.com` | 200 ✅ |

所以配置了两处镜像，都已写好、不需要再手工设置：

* `~/.cargo/config.toml` —— 把 `crates-io` 换成中科大（sparse 索引 + 同一站点的
  下载端点，实测 `.../crates/serde/1.0.210/download` 能拉到 78 KB 的真包）。
  想还原官方源就删掉这个文件，它只做了这一件事。
* 前端**不用 npm**（纯静态文件、无打包器），所以 npmmirror 只是备着没用上。

还有一个**只在沙箱/CI 里才会遇到的坑**：如果 cargo 报
`Failed to connect to ... over proxy 127.0.0.1`，说明它在走一个不可达的代理。
环境的 `https_proxy` 是对的但 cargo 没吃进去（curl 出于 httpoxy 防护会忽略大写的
`HTTP_PROXY`，而沙箱同时设了大小写两套变量），显式喂给它即可：

```bash
CARGO_HTTP_PROXY="$https_proxy" cargo build --release
```

普通终端里不需要这一层。

### 两个必须知道的坑

1. **`icons/icon.ico` 不能少。** `tauri-build` 要拿它生成 Windows 资源文件，
   缺了会直接构建失败（报 `required for generating a Windows Resource file`）。
   这个和工具链无关，GNU/MSVC 都一样。
2. **分发要带上 `WebView2Loader.dll`。** 产物是动态导入它的
   （`objdump -p` 能看到这条 import），`webview2-com-sys` 的构建脚本会自动把它
   复制到 exe 旁边。只拷 exe 会启动即报缺 DLL。系统还需要 **WebView2 运行时**
   （Win11 自带；Win10 通常随 Edge 装机已有，本机实测 145.0.3800.82）。

## 为什么 GUI 不在引擎的 workspace 里

引擎层（`crates/**`）的硬约束是**零第三方依赖**，而 GUI 必然要用 Tauri。
所以 `src-tauri` 自己就是一个 workspace 根，通过 path 依赖引用引擎；
引擎根的 `Cargo.toml` 用 `exclude` 把它挡在外面。效果：

* 在 `filewatch/` 下 `cargo build` 永远不会去编译 Tauri —— 引擎的依赖图仍然是空的；
* 两边可以独立升级、独立构建；
* **只有 `apps/gui/src-tauri` 这一层被允许加依赖**，加之前请确认这一点。

## 交互模型（一个刻意的取舍）

布局是**屏幕空间递归**的：每一层都用当前画布的像素重跑一次，所以目录标题栏恒定
16 px、间距恒定 1~2 px，缩放时装饰不会变形（文档 §5.6）。这带来一个后果 ——
**自由拖拽缩放和它天然冲突**：把整张图当作大位图去缩放，那些「恒定像素」的装饰
就会跟着放大，屏幕空间递归就白做了。

所以这里的「缩放」表达为**层级导航**：

| 操作 | 效果 |
|---|---|
| 滚轮向上 / 双击方块 | 下钻到光标下最深的那个方块（等价于放大） |
| 滚轮向下 / Esc | 回到上一层（等价于缩小） |
| 点击方块 / 点击右侧类型 | 选中并显示详情（不改变层级） |
| 点击类型行 | 高亮面积图里同一扩展名的所有方块，其余降透明度 |
| 上方面包屑 | 任意层级来回跳 |
| 顶栏「细节」滑块 | 调整 LOD 展开阈值（`min_side`）：左=精简，右=展开更深 |

连按滚轮就能一路钻到最深处，这正是 WizTree / SpaceSniffer 的模型，也是这类
「每层铺满视口」的面积图唯一自洽的缩放方式。文档 §5.6 里那套 `(center, zoom)`
相机模型是给「把整棵树当一张无限大画布」的渲染方式准备的，与屏幕空间递归二者
只能选一个 —— 这里选了后者，因为它才是「缩放不变形」的前提。

## 二进制布局协议

Rust → 前端**不走 JSON**（文档 §5.6）：200 万节点的 JSON 会卡死主线程、体积也
膨胀数倍。协议在 `src-tauri/src/wire.rs`，前端解码端在 `src/wire.js`，两边必须
同步改：

```
头部 16 字节：magic "FWLY" | version | count | root
每项 56 字节：node u32 | depth u8 | flags u8 | slot u8 | pad u8
              ext_id u32 | shade_t f32 | rect 4×f32 | header 4×f32 | size u64
```

前端 `decode` 的第一件事就是校验 magic 与 version，不匹配**直接抛错**而不是按旧
偏移读出一堆「看起来差不多」的错数据。`wire.rs` 里有一个把每个字段都解回来的
往返测试（`round_trip_preserves_every_field`），它就是这份文档的可执行版本。

## 当前完成度

| 编号 | 功能 | 状态 |
|---|---|---|
| F1 | 盘符选择（容量/可用/文件系统/是否可 MFT） | ✅ |
| F2 | 全量扫描 + 流式进度（**节流**到 60 ms 一条事件） | ✅ 进度是计数式；面积图在扫描完成后一次性绘制 |
| F3 | 文件夹树（按大小降序，分页 200 条） | ✅ 以「面包屑层级 + 当前层子项」的形式呈现 |
| F4 | 面积图：Squarified + LOD + 下钻 + 面包屑 + 类型高亮 | ✅ |
| F5 | 文件类型列表 + 点击联动高亮 | ✅ |
| F12 | Top-N 大文件（部分） | ✅ |
| F6 | 实时增量监控面板（新增/删除/改名/大小变化） | ⬜ 引擎已具备（`Session::poll`），界面未接 |
| F7 | 阈值预警面板 | ⬜ 引擎已具备，界面未接 |
| F8 | 变更流水表 + 导出 CSV | ⬜ 引擎已具备，界面未接 |
| F9 | 非 NTFS 降级 | ✅ 自动（`supports_mft` + 无管理员都走遍历，界面明确标注） |
| F0 | 管理员权限申请（文档 R2） | ✅ 进入软件时弹窗申请 + `runas` 重启；可勾「不再自动询问」，状态栏随时重新申请 |

**F2 的「流式渐进渲染」**目前只做到「进度条 + 实时计数，不白屏」，面积图是在扫描
结束后一次画出的。真正的「扫到多少画多少」需要引擎在扫描过程中暴露部分树快照，
属于下一步。

## 与 CLI 的关系

同一个引擎、同一条装配路径（`core_scan::Session`）。所以：

* 同一个目录在 CLI 与界面里显示的大小**必须**一致 —— 不一致就是 bug，不是"口径不同"；
* 界面里的任何新算法都应该先落到 `core-*` 里，两个前端共享；
* 反向也成立：GUI 这一层不允许出现「顺手在这里算一下」的逻辑。
