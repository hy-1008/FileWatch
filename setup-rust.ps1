# 一键搭建 FileWatch 的 Rust 构建环境（Windows）
#
# 为什么需要这个脚本：
#   本项目的目标构建机上**没有 Windows SDK 10**（即没有 kernel32.lib），
#   而 Rust 默认的 MSVC 工具链必须依赖它才能链接。因此我们必须使用
#   `x86_64-pc-windows-gnu` 工具链 —— rustup 的 GNU 变体自带 MinGW-w64
#   （含 gcc + 导入库），**不需要任何 Visual Studio 组件**即可完成链接。
#
#   同时国内直连 static.rust-lang.org 常常只有几 KB/s，脚本默认走清华 TUNA
#   镜像，可用 -Mirror 参数切换。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File .\setup-rust.ps1
#   powershell -ExecutionPolicy Bypass -File .\setup-rust.ps1 -Mirror ustc
#   powershell -ExecutionPolicy Bypass -File .\setup-rust.ps1 -SkipInstall
#
# 参数：
#   -Mirror <tuna|ustc|rsproxy|official>   下载源，默认 tuna
#   -SkipInstall                           只配置环境不安装 rustup
#   -Static                                额外开启 crt-static（生成完全自包含的 exe）
#   -Test                                  构建完成后跑一遍测试与自检

[CmdletBinding()]
param(
    [ValidateSet('tuna', 'ustc', 'rsproxy', 'official')]
    [string]$Mirror = 'tuna',
    [switch]$SkipInstall,
    [switch]$Static,
    [switch]$Test
)

$ErrorActionPreference = 'Stop'

# PowerShell 5.1 默认可能只启用 TLS 1.0/1.1，对国内 HTTPS 镜像会直接报
# "Could not create SSL/TLS secure channel"。必须显式打开 TLS 1.2（并保留 1.3）。
try {
    [Net.ServicePointManager]::SecurityProtocol = `
        [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls11
} catch {
    Write-Host '警告：无法设置 TLS 版本，下载可能失败' -ForegroundColor Yellow
}

# 下载助手：先试 Invoke-WebRequest，失败则退回系统自带的 curl.exe
# （Windows 10 1803+ 自带；curl 用自己的 TLS 栈，能绕开 PS 的 SSL 问题）
function Get-RemoteFile([string]$url, [string]$outFile) {
    try {
        Invoke-WebRequest -Uri $url -OutFile $outFile -UseBasicParsing -TimeoutSec 90
        if ((Test-Path $outFile) -and (Get-Item $outFile).Length -gt 0) { return $true }
    } catch { }
    try {
        $curl = Join-Path $env:SystemRoot 'System32\curl.exe'
        if (Test-Path $curl) {
            & $curl -fL --connect-timeout 30 --max-time 900 -o $outFile $url 2>$null
            if ($LASTEXITCODE -eq 0 -and (Test-Path $outFile) -and
                (Get-Item $outFile).Length -gt 0) { return $true }
        } else {
            & curl.exe -fL --connect-timeout 30 --max-time 900 -o $outFile $url 2>$null
            if ($LASTEXITCODE -eq 0 -and (Test-Path $outFile) -and
                (Get-Item $outFile).Length -gt 0) { return $true }
        }
    } catch { }
    return $false
}

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "    $msg" -ForegroundColor Green }
function Write-Warn2($msg){ Write-Host "    $msg" -ForegroundColor Yellow }

# ── 镜像地址 ────────────────────────────────────────────────────────────────
$mirrors = @{
    tuna     = @{ Dist = 'https://mirrors.tuna.tsinghua.edu.cn/rustup';              Update = 'https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup' }
    ustc     = @{ Dist = 'https://mirrors.ustc.edu.cn/rust-static';                  Update = 'https://mirrors.ustc.edu.cn/rust-static/rustup' }
    rsproxy  = @{ Dist = 'https://rsproxy.cn';                                       Update = 'https://rsproxy.cn/rustup' }
    official = @{ Dist = 'https://static.rust-lang.org';                             Update = 'https://static.rust-lang.org/rustup' }
}
$m = $mirrors[$Mirror]

# ── 0. 环境自检 ─────────────────────────────────────────────────────────────
Write-Step '检查当前环境'
Write-Ok ("PowerShell {0}" -f $PSVersionTable.PSVersion)

# 检测 Windows SDK 是否可用（决定是否需要 GNU 工具链）
$sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Lib'
$hasSdk = $false
if (Test-Path $sdkRoot) {
    $hasSdk = @(Get-ChildItem $sdkRoot -Directory -ErrorAction SilentlyContinue).Count -gt 0
}
if ($hasSdk) {
    Write-Ok '检测到 Windows SDK 10 —— MSVC 工具链理论上也可用'
    Write-Warn2 '但本脚本仍会安装 GNU 工具链：本项目已按 GNU 验证，且 GNU 不依赖 VS 组件'
} else {
    Write-Warn2 '未检测到 Windows SDK 10 —— MSVC 工具链会因缺少 kernel32.lib 而链接失败'
    Write-Warn2 '将使用 x86_64-pc-windows-gnu（rustup 自带 MinGW-w64，无需 VS）'
}

# ── 1. 安装 / 检测 rustup ───────────────────────────────────────────────────
$cargoExe = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargoExe -and -not $SkipInstall) {
    Write-Step "安装 rustup（镜像：$Mirror）"

    $env:RUSTUP_DIST_SERVER  = $m.Dist
    $env:RUSTUP_UPDATE_ROOT  = $m.Update

    # 下载 rustup-init.exe；逐个镜像回退，每个镜像内先 IWR 再 curl
    $initExe = Join-Path $env:TEMP 'rustup-init.exe'
    if (Test-Path $initExe) { Remove-Item $initExe -Force }

    $candidates = @(
        "$($m.Update)/dist/x86_64-pc-windows-msvc/rustup-init.exe",
        'https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
        'https://mirrors.ustc.edu.cn/rust-static/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
        'https://rsproxy.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
        'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
    ) | Select-Object -Unique

    $downloaded = $false
    foreach ($u in $candidates) {
        Write-Ok "尝试 $u"
        if (Get-RemoteFile $u $initExe) {
            Write-Ok ("  成功（{0:N0} 字节）" -f (Get-Item $initExe).Length)
            $downloaded = $true
            break
        }
        Write-Warn2 '  失败，换下一个'
    }
    if (-not $downloaded) {
        throw "所有镜像都无法下载 rustup-init.exe。请手动安装 Rust 后加 -SkipInstall 重跑。"
    }

    & $initExe -y --no-modify-path --default-host x86_64-pc-windows-gnu --default-toolchain none --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup-init.exe 返回 $LASTEXITCODE" }
    $cargoExe = Get-Command cargo -ErrorAction SilentlyContinue
} elseif ($SkipInstall) {
    Write-Step '按 -SkipInstall 跳过安装步骤'
}

# 让当前会话能找到 cargo/rustup
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo 仍不可用。请确认 `$env:USERPROFILE\.cargo\bin 已加入 PATH，然后重开一个终端再跑一次。"
}

# ── 2. 安装 GNU 工具链 ──────────────────────────────────────────────────────
Write-Step '安装 x86_64-pc-windows-gnu 工具链'

# 依次尝试各镜像；任一成功即停。默认先用用户指定的镜像。
$tryMirrors = @($Mirror) + @('tuna', 'ustc', 'rsproxy', 'official') |
              Select-Object -Unique
$installed = $false
foreach ($mk in $tryMirrors) {
    $env:RUSTUP_DIST_SERVER = $mirrors[$mk].Dist
    $env:RUSTUP_UPDATE_ROOT = $mirrors[$mk].Update
    Write-Ok "尝试镜像 $mk ($($mirrors[$mk].Dist))"
    rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal -c rustfmt -c clippy
    if ($LASTEXITCODE -eq 0) { $installed = $true; break }
    Write-Warn2 "  镜像 $mk 失败，换下一个"
    rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
    if ($LASTEXITCODE -eq 0) {
        Write-Warn2 '  仅核心组件装成功（rustfmt/clippy 不可用，不影响构建）'
        $installed = $true; break
    }
}
if (-not $installed) { throw '所有镜像都装不上 stable-x86_64-pc-windows-gnu 工具链。' }

# rustup-init 用 --default-toolchain none 装的环境没有默认工具链，
# 不显式设默认的话 `cargo build` 会直接报 "no default toolchain configured"。
Write-Step '设为默认工具链'
rustup default stable-x86_64-pc-windows-gnu
if ($LASTEXITCODE -ne 0) { throw '设置默认工具链失败' }

Write-Step '校验工具链'
rustup target list --installed
rustc --version
cargo --version

# 让 GNU 成为本目录下的默认目标
Write-Step '写入 .cargo\config.toml（固定目标三元组）'
$cargoDir = Join-Path $PSScriptRoot '.cargo'
if (-not (Test-Path $cargoDir)) { New-Item -ItemType Directory -Path $cargoDir | Out-Null }
$configPath = Join-Path $cargoDir 'config.toml'

$lines = @(
    '# 由 setup-rust.ps1 生成。',
    '#',
    '# 为什么固定 GNU 目标：本机没有 Windows SDK 10（缺 kernel32.lib），',
    '# MSVC 工具链链接会失败。x86_64-pc-windows-gnu 由 rustup 自带 MinGW-w64，',
    '# 无需任何 Visual Studio 组件。',
    '[build]',
    'target = "x86_64-pc-windows-gnu"'
)
if ($Static) {
    $lines += @(
        '',
        '# crt-static：把 C 运行库静态链进去，产出的 exe 完全自包含（便于拷给别人用）',
        '[target.x86_64-pc-windows-gnu]',
        'rustflags = ["-C", "target-feature=+crt-static"]'
    )
}
# 用 UTF8 无 BOM 写出，避免 cargo 解析失败
[System.IO.File]::WriteAllLines($configPath, $lines, (New-Object System.Text.UTF8Encoding($false)))
Write-Ok "已写入 $configPath"

# ── 3. 构建 ─────────────────────────────────────────────────────────────────
Write-Step '构建（release）'
Push-Location $PSScriptRoot
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

    $exe = Join-Path $PSScriptRoot 'target\x86_64-pc-windows-gnu\release\filewatch.exe'
    if (-not (Test-Path $exe)) {
        $exe = Join-Path $PSScriptRoot 'target\release\filewatch.exe'
    }
    Write-Ok "构建产物：$exe"

    if ($Test) {
        Write-Step '运行单元测试与集成测试'
        cargo test --release
        Write-Step '运行内置自检'
        & $exe selftest --verbose
    } else {
        Write-Step '运行内置自检（不需要管理员权限）'
        & $exe selftest
    }

    Write-Host ''
    Write-Host '完成。常用命令：' -ForegroundColor Cyan
    Write-Host "  $exe volumes"
    Write-Host "  $exe scan C: --top=10 --exts=10 --layout=110x32"
    Write-Host "  $exe monitor `$env:USERPROFILE\Downloads --rule=abs=1G --duration=60s"
    Write-Host ''
    Write-Host '提示：想看 MFT 直读 / USN Journal 那两条快速路径，需要**以管理员身份**运行终端。' -ForegroundColor Yellow
    Write-Host '      不提权也能用 —— 会自动降级为目录遍历 + ReadDirectoryChangesW。' -ForegroundColor Yellow
}
finally {
    Pop-Location
}
