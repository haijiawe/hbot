#Requires -Version 7.0
# 为什么要求 PowerShell 7：Windows PowerShell 5.1 的 Start-Process -RedirectStandardError <file>
# 会把子进程的 .ExitCode 丢成空值（本机实测，cmd /c exit 3 也一样），而本脚本要靠退出码判定；
# 另外 5.1 的 Get-Content 默认按 GBK 解 UTF-8 日志，断言会全变成乱码。
<#
.SYNOPSIS
    验证"关窗收进托盘"与"优雅退出"两条路径 —— 不点鼠标，用 WM_CLOSE 触发。

.DESCRIPTION
    三段验证，全是真的进程行为：

      A. **默认（closeToTray 缺省 = true）**：往主窗口发 `WM_CLOSE`，窗口必须**消失**
         （`IsWindowVisible` 变 false），但进程**仍然活着** —— 这就是"关窗到托盘、
         后台机器人继续跑"。
      B. **从托盘恢复**：走"单实例第二次启动"这条路（它和托盘菜单"显示主窗口"调用
         同一个 `shell::show_main_window`），窗口必须重新可见。
      C. **优雅退出**：把 `desktop.json` 写成 `closeToTray=false`，再发 `WM_CLOSE`，
         进程必须自行结束、退出码 0，且 stderr 里有 `桌面端开始优雅退出` 与
         `空库（没有机器人）` —— 证明走的是"停机器人 → 打写屏障 → exit"这条路径，
         而不是被直接杀掉。

    **没有验证的部分（别当成已验证）**：
      - 托盘图标是否真的出现在 Windows 通知区域（外壳行为，脚本断言不了）。
      - 点击托盘菜单项（"显示主窗口" / "开机自启" / "退出"）—— 需要真实鼠标点击。
        本脚本用同一条函数路径替代：B 段验了 `show_main_window`，C 段验了
        `request_quit`（托盘"退出"调的就是它）。

.PARAMETER ExePath
    可执行文件路径，默认 `$env:CARGO_TARGET_DIR\debug\hbot-desktop.exe`。

.EXAMPLE
    pwsh -File scripts/verify-tray-close.ps1
#>
[CmdletBinding()]
param(
    [string]$ExePath = '',
    [string]$WindowTitle = 'hbot'
)

$ErrorActionPreference = 'Stop'

$WM_CLOSE = 0x0010
# 主窗口标题来自 `tauri.conf.json` 的 `app.windows[0].title`。必须用**精确标题**找窗口：
# 进程里还有一个可见的隐藏辅助窗口（单实例插件的 `com.hbot.desktop-siw`），
# 按"第一个有标题的可见窗口"找会抓到它，往它发 WM_CLOSE 就把插件的事件窗口销毁了。
$script:MainWindowTitle = $WindowTitle

function Write-Step([string]$Text) {
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

function Write-Check([string]$Name, [bool]$Ok, [string]$Detail) {
    if ($Ok) {
        Write-Host ("  [PASS] {0}: {1}" -f $Name, $Detail) -ForegroundColor Green
    }
    else {
        Write-Host ("  [FAIL] {0}: {1}" -f $Name, $Detail) -ForegroundColor Red
    }
    return $Ok
}

# --- Win32 小工具 ------------------------------------------------------------
$script:HbotWin32Ready = $false
function Initialize-HbotWin32 {
    if ($script:HbotWin32Ready) { return }

    $source = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class HbotTrayWin32 {
    private delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumProc callback, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    private static extern bool PostMessageW(IntPtr hWnd, uint message, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int count);

    public static string WindowTitle(IntPtr handle) {
        if (handle == IntPtr.Zero) { return null; }
        StringBuilder buffer = new StringBuilder(512);
        int length = GetWindowTextW(handle, buffer, buffer.Capacity);
        if (length <= 0) { return null; }
        return buffer.ToString(0, length);
    }

    public static bool Visible(IntPtr handle) {
        return IsWindowVisible(handle);
    }

    public static bool Close(IntPtr handle) {
        return PostMessageW(handle, 0x0010, IntPtr.Zero, IntPtr.Zero);
    }

    public static IntPtr FindVisibleWindow(uint processId, string expectedTitle) {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate (IntPtr hWnd, IntPtr lParam) {
            uint owner;
            GetWindowThreadProcessId(hWnd, out owner);
            if (owner != processId) { return true; }
            if (!IsWindowVisible(hWnd)) { return true; }
            // 必须按**标题精确匹配**：单实例插件自己有一个可见的隐藏辅助窗口
            // （标题 `com.hbot.desktop-siw`），往它发 WM_CLOSE 会把它销毁，
            // 之后第二次启动就找不到目标窗口了（踩过）。
            string title = WindowTitle(hWnd);
            if (title == null) { return true; }
            if (!String.Equals(title, expectedTitle, StringComparison.Ordinal)) { return true; }
            found = hWnd;
            return false;
        }, IntPtr.Zero);
        return found;
    }
}
'@

    try {
        # 这台机器上全局 `LIB` 指向一个不存在的 VS 目录，PS 5.1 的 Add-Type 走 csc，
        # 会把那条无效搜索路径当成错误直接失败。清空它不影响 Add-Type 用的引用程序集。
        if ($env:LIB) { $env:LIB = '' }
        Add-Type -TypeDefinition $source -ErrorAction Stop
        $script:HbotWin32Ready = $true
    }
    catch {
        $script:HbotWin32Ready = $false
    }
}

function Get-VisibleWindowHandle([int]$ProcessId) {
    if (-not $script:HbotWin32Ready) { return [IntPtr]::Zero }
    return [HbotTrayWin32]::FindVisibleWindow([uint32]$ProcessId, $script:MainWindowTitle)
}

function Test-WindowVisible([IntPtr]$Handle) {
    if (-not $script:HbotWin32Ready) { return $null }
    return [HbotTrayWin32]::Visible($Handle)
}

function Invoke-WindowClose([IntPtr]$Handle) {
    if (-not $script:HbotWin32Ready) { throw 'Add-Type 不可用，无法发 WM_CLOSE' }
    return [HbotTrayWin32]::Close($Handle)
}

function Wait-Until([scriptblock]$Condition, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 250
    }
    return (& $Condition)
}

function Get-LogText([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    # 子进程还在写这个文件时，`[System.IO.File]::ReadAllText` 默认的 FileShare.Read 会撞
    # 共享冲突（拿不到写句柄的许可），返回空串；`Get-Content` 在 PS 5.1 又会按 GBK 解。
    # 所以自己开流：FileShare.ReadWrite + 显式 UTF-8。
    try {
        $stream = [System.IO.FileStream]::new($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $reader = [System.IO.StreamReader]::new($stream, [System.Text.UTF8Encoding]::new($false))
            try {
                return $reader.ReadToEnd()
            }
            finally { $reader.Dispose() }
        }
        finally { $stream.Dispose() }
    }
    catch {
        return ''
    }
}

function Wait-VisibleWindow([int]$ProcessId, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $handle = Get-VisibleWindowHandle $ProcessId
        if ($handle -ne [IntPtr]::Zero) { return $handle }
        Start-Sleep -Milliseconds 250
    }
    return [IntPtr]::Zero
}

function Start-IsolatedInstance([string]$ExePath, [string]$StdOut, [string]$StdErr) {
    return Start-Process -FilePath $ExePath -PassThru `
        -RedirectStandardOutput $StdOut -RedirectStandardError $StdErr
}

# --- 定位可执行文件 -----------------------------------------------------------
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $targetDir = $env:CARGO_TARGET_DIR
    if ([string]::IsNullOrWhiteSpace($targetDir)) { $targetDir = Join-Path $repoRoot 'target' }
    $ExePath = Join-Path $targetDir 'debug\hbot-desktop.exe'
}
if (-not (Test-Path -LiteralPath $ExePath)) {
    Write-Host "找不到可执行文件：$ExePath" -ForegroundColor Red
    Write-Host '先跑：cargo build -p hbot-desktop -j 2' -ForegroundColor Yellow
    exit 2
}
$ExePath = (Resolve-Path -LiteralPath $ExePath).Path

Initialize-HbotWin32
if (-not $script:HbotWin32Ready) {
    Write-Host 'Add-Type 不可用：这个脚本的核心手段就是 Win32 调用，无法继续。' -ForegroundColor Red
    exit 2
}

$existing = @(Get-Process -Name 'hbot-desktop' -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
    Write-Host ("已有 {0} 个 hbot-desktop 在跑（PID: {1}）；单实例锁是全局的，请先关掉。" -f $existing.Count, (($existing | ForEach-Object { $_.Id }) -join ',')) -ForegroundColor Yellow
    exit 2
}

$workRoot = Join-Path $env:TEMP ("hbot-tray-close-{0}-{1}" -f $PID, (Get-Date -Format 'HHmmss'))
$appDataDir = Join-Path $workRoot 'appdata'
$userProfileDir = Join-Path $workRoot 'profile'
$dataDir = Join-Path $appDataDir 'hbot'
New-Item -ItemType Directory -Force -Path $dataDir, $userProfileDir | Out-Null
$settingsPath = Join-Path $dataDir 'desktop.json'

Write-Step '0. 环境'
Write-Host "  可执行文件: $ExePath"
Write-Host "  临时工作目录: $workRoot"
Write-Host "  隔离后的数据目录: $dataDir"

$savedAppData = $env:APPDATA
$savedUserProfile = $env:USERPROFILE
$env:APPDATA = $appDataDir
$env:USERPROFILE = $userProfileDir

$problems = @()
$running = @()

try {
    # =====================================================================
    # A. 默认行为：关窗 → 收进托盘（进程继续跑）
    # =====================================================================
    Write-Step 'A. 默认（closeToTray=true）：发 WM_CLOSE，窗口应消失、进程应仍在'
    $firstOut = Join-Path $workRoot 'a.out.log'
    $firstErr = Join-Path $workRoot 'a.err.log'
    $app = Start-IsolatedInstance $ExePath $firstOut $firstErr
    $running += $app
    Write-Host ("  PID: {0}" -f $app.Id)

    $handle = Wait-VisibleWindow $app.Id 60
    if (-not (Write-Check '主窗口出现' ($handle -ne [IntPtr]::Zero) ("hwnd=0x{0:X}" -f [int64]$handle))) {
        $problems += 'a-no-window'
        throw '窗口都没出来，后面没法测'
    }

    # 托盘已经建出来了（配置驱动，日志里 source="config"）。
    $null = Wait-Until { (Get-LogText $firstErr).Contains('托盘已就绪') } 20
    $aLog = Get-LogText $firstErr
    if (-not (Write-Check '日志确认托盘已建出并挂上菜单' ($aLog.Contains('托盘已就绪') -and $aLog.Contains('source="config"')) 'stderr 里找 托盘已就绪 与 source=config')) {
        $problems += 'a-no-tray-log'
    }

    $closed = Invoke-WindowClose $handle
    if (-not (Write-Check 'WM_CLOSE 投递成功' $closed 'PostMessage 返回值')) { $problems += 'a-postmessage' }

    $hidden = Wait-Until { (Test-WindowVisible $handle) -eq $false } 15
    if (-not (Write-Check '窗口被隐藏（IsWindowVisible=false）' $hidden ("Visible=" + (Test-WindowVisible $handle)))) {
        $problems += 'a-not-hidden'
    }
    $app.Refresh()
    if (-not (Write-Check '进程仍然存活（后台继续跑）' (-not $app.HasExited) ("HasExited=" + $app.HasExited))) {
        $problems += 'a-exited'
    }

    # =====================================================================
    # B. 从托盘恢复：走单实例第二次启动（与托盘"显示主窗口"同一函数）
    # =====================================================================
    Write-Step 'B. 第二个实例应把窗口重新显示出来（= 托盘"显示主窗口"同一条路径）'
    $bOut = Join-Path $workRoot 'b.out.log'
    $bErr = Join-Path $workRoot 'b.err.log'
    $second = Start-IsolatedInstance $ExePath $bOut $bErr
    $running += $second
    $null = $second.WaitForExit(30000)
    $second.Refresh()
    if (-not (Write-Check '第二个实例退出码 = 0' ($second.HasExited -and $second.ExitCode -eq 0) ("HasExited=" + $second.HasExited + " ExitCode=" + $second.ExitCode))) {
        $problems += 'b-second-exit'
    }
    $shown = Wait-Until { (Test-WindowVisible $handle) -eq $true } 15
    if (-not (Write-Check '窗口重新可见（被带到前台）' $shown ("Visible=" + (Test-WindowVisible $handle)))) {
        $problems += 'b-not-shown'
    }

    # =====================================================================
    # C. closeToTray=false：关窗 → 优雅退出（退出码 0，走 shutdown 路径）
    # =====================================================================
    Write-Step 'C. closeToTray=false：关窗应优雅退出（停 bot → 写屏障 → exit 0）'
    & taskkill.exe /PID $app.Id /T /F 2>&1 | Out-Null
    $gone = Wait-Until { $null -eq (Get-Process -Id $app.Id -ErrorAction SilentlyContinue) } 20
    if (-not $gone) { $problems += 'c-kill-timeout' }

    # serde_json 拒收 BOM：用 UTF8Encoding($false)。
    $json = "{`n  `"autostart`": false,`n  `"closeToTray`": false`n}`n"
    [System.IO.File]::WriteAllText($settingsPath, $json, (New-Object System.Text.UTF8Encoding $false))
    Write-Host "  写入 $settingsPath"

    $cOut = Join-Path $workRoot 'c.out.log'
    $cErr = Join-Path $workRoot 'c.err.log'
    $third = Start-IsolatedInstance $ExePath $cOut $cErr
    $running += $third
    Write-Host ("  PID: {0}" -f $third.Id)

    $handle3 = Wait-VisibleWindow $third.Id 60
    if (-not (Write-Check '第三个实例的主窗口出现' ($handle3 -ne [IntPtr]::Zero) ("hwnd=0x{0:X}" -f [int64]$handle3))) {
        $problems += 'c-no-window'
    }
    else {
        # 必须先等 `shell::install` 跑完（判据：托盘已挂上菜单）再发 WM_CLOSE。
        # 窗口是 `Builder::build` 里创建的，`setup` 里的关窗拦截器要晚几百毫秒；
        # 抢在前面发 WM_CLOSE 会走 Tauri 默认关窗（最后一个窗口没了就直接退出），
        # 那样测的就不是优雅退出路径了（踩过）。
        $null = Wait-Until { (Get-LogText $cErr).Contains('托盘已就绪') } 20
        $closed3 = Invoke-WindowClose $handle3
        if (-not (Write-Check 'WM_CLOSE 投递成功' $closed3 'PostMessage 返回值')) { $problems += 'c-postmessage' }

        $exited = $third.WaitForExit(30000)
        $third.Refresh()
        if (-not (Write-Check '进程自行退出（不是被杀的）' $exited ("HasExited=" + $third.HasExited))) {
            $problems += 'c-hung'
        }
        else {
            if (-not (Write-Check '退出码 = 0' ($third.ExitCode -eq 0) ("ExitCode=" + $third.ExitCode))) {
                $problems += 'c-exit-code'
            }
        }

        $cLog = Get-LogText $cErr
        if (-not (Write-Check '日志证明走了优雅退出路径' ($cLog.Contains('桌面端开始优雅退出') -and $cLog.Contains('空库（没有机器人）')) 'stderr 里找 "桌面端开始优雅退出" 与 "空库（没有机器人）"')) {
            $problems += 'c-graceful-log'
        }
        Write-Host '  第三个实例 stderr 全文：'
        ($cLog -split "`r?`n") | Where-Object { $_.Trim().Length -gt 0 } | ForEach-Object { Write-Host "    $_" }
    }
}
finally {
    $env:APPDATA = $savedAppData
    $env:USERPROFILE = $savedUserProfile

    Write-Step '清理：强杀残留进程'
    foreach ($proc in $running) {
        if ($null -ne $proc) {
            try { $proc.Refresh() } catch { }
            if (-not $proc.HasExited) {
                & taskkill.exe /PID $proc.Id /T /F 2>&1 | ForEach-Object { Write-Host "  $_" }
            }
        }
    }
}

Write-Step '结论'
if ($problems.Count -eq 0) {
    Write-Host 'A/B/C 全通过：关窗只隐藏进程不退、第二次启动能把窗口重新显示、closeToTray=false 时关窗优雅退出 0。' -ForegroundColor Green
    Write-Host "日志留在：$workRoot"
    exit 0
}
else {
    Write-Host ("失败项: " + ($problems -join ', ')) -ForegroundColor Red
    Write-Host "日志留在：$workRoot"
    exit 1
}