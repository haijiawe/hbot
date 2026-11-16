#Requires -Version 7.0
# 为什么要求 PowerShell 7：Windows PowerShell 5.1 的 Start-Process -RedirectStandardError <file>
# 会把子进程的 .ExitCode 丢成空值（本机实测，cmd /c exit 3 也一样），而本脚本要靠退出码判定；
# 另外 5.1 的 Get-Content 默认按 GBK 解 UTF-8 日志，断言会全变成乱码。
<#
.SYNOPSIS
    验证托盘菜单三个 id 的**派发**（不是鼠标点击）：show / autostart / quit。

.DESCRIPTION
    真实鼠标点击 Windows 通知区域里的托盘菜单不可靠，所以这里换一条**可重复**的路：
    第二个进程带隐藏参数 `--tray-action <菜单 id>` 启动 → 单实例插件把 argv 交给已运行
    的实例 → 那个实例走与真实菜单事件**完全相同**的 `shell::handle_menu` +
    `shell::dispatch_menu_action`。脚本只对结果做断言：

      A. `--tray-action show`：先发 WM_CLOSE 把主窗口藏起来（这是已经验证过的关窗路径），
         再派发 show，断言窗口重新可见（IsWindowVisible=true）。
      B. `--tray-action autostart`：连派发两次，断言第一次 desktop.json 的 autostart
         变 true 且 HKCU Run 下出现 `hbot` 值（带引号的 exe 路径），第二次变回 false
         且注册表值被删掉 —— 两个方向都验到"翻转"。
      C. `--tray-action quit`：断言第一个实例自行退出、退出码 0，stderr 里有
         `桌面端开始优雅退出` 与 `空库（没有机器人）`（证明走的是 shutdown 写屏障）。

    每条动作还会断言第一个实例的 stderr 里有 `source="cli"` + 对应 `action=` 的日志，
    证明动作确实由隐藏入口触发，而不是别的路径碰巧产生了同样结果。

    **没有验证、也不打算在这份脚本里验证的部分**：
      - 真实鼠标点中系统托盘图标 / 菜单项。本脚本验证的是「菜单 id → 动作」的派发，
        事件来源是单实例转交的 argv，不是鼠标。
      - 托盘图标在通知区域的外观。
      - Linux / macOS 的托盘与自启。
      - 重启后真的自启（只验证注册表写入/删除）。

.PARAMETER ExePath
    可执行文件路径，默认 `$env:CARGO_TARGET_DIR\debug\hbot-desktop.exe`。

.EXAMPLE
    pwsh -File scripts/verify-tray-actions.ps1
#>
[CmdletBinding()]
param(
    [string]$ExePath = '',
    [string]$WindowTitle = 'hbot'
)

$ErrorActionPreference = 'Stop'

$WM_CLOSE = 0x0010
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$ValueName = 'hbot'
# 主窗口标题来自 `tauri.conf.json` 的 `app.windows[0].title`。必须精确匹配：同进程里
# 单实例插件还有一个可见的辅助窗口（标题 `com.hbot.desktop-siw`），抓错窗口就废了。
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

# --- Win32 小工具（与 verify-tray-close.ps1 同一套） --------------------------
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

function Start-IsolatedInstance([string]$ExePath, [string]$StdOut, [string]$StdErr, [string[]]$AppArgs = @()) {
    $startArgs = @{
        FilePath               = $ExePath
        PassThru               = $true
        RedirectStandardOutput = $StdOut
        RedirectStandardError  = $StdErr
    }
    if ($AppArgs.Count -gt 0) { $startArgs.ArgumentList = $AppArgs }
    return Start-Process @startArgs
}

function Get-RunValue {
    $item = Get-ItemProperty -LiteralPath $RunKey -Name $ValueName -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return [string]$item.$ValueName
}

function Remove-RunValue {
    Remove-ItemProperty -LiteralPath $RunKey -Name $ValueName -ErrorAction SilentlyContinue
}

function Get-AutostartSetting([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $json = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    return [bool]$json.autostart
}

function Write-DesktopSettings([string]$Path, [bool]$Autostart, [bool]$CloseToTray) {
    $json = "{`n  `"autostart`": $($Autostart.ToString().ToLowerInvariant()),`n  `"closeToTray`": $($CloseToTray.ToString().ToLowerInvariant())`n}`n"
    [System.IO.File]::WriteAllText($Path, $json, (New-Object System.Text.UTF8Encoding $false))
}

function Start-TrayAction([string]$ExePath, [string]$Action, [string]$StdOut, [string]$StdErr) {
    return Start-IsolatedInstance $ExePath $StdOut $StdErr @('--tray-action', $Action)
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

$workRoot = Join-Path $env:TEMP ("hbot-tray-actions-{0}-{1}" -f $PID, (Get-Date -Format 'HHmmss'))
$appDataDir = Join-Path $workRoot 'appdata'
$userProfileDir = Join-Path $workRoot 'profile'
$dataDir = Join-Path $appDataDir 'hbot'
New-Item -ItemType Directory -Force -Path $dataDir, $userProfileDir | Out-Null
$settingsPath = Join-Path $dataDir 'desktop.json'
Write-DesktopSettings $settingsPath $false $true

Write-Step '0. 环境'
Write-Host "  可执行文件: $ExePath"
Write-Host "  临时工作目录: $workRoot"
Write-Host "  隔离后的数据目录: $dataDir"

$savedAppData = $env:APPDATA
$savedUserProfile = $env:USERPROFILE
$savedRunValue = Get-RunValue
$hadRunValue = $null -ne $savedRunValue
# 从干净状态开始：否则 setup 的 reconcile 会拿真实注册表值改掉 desktop.json。
Remove-RunValue

$env:APPDATA = $appDataDir
$env:USERPROFILE = $userProfileDir

$problems = @()
$running = @()

try {
    # =====================================================================
    # A. show：藏起来 → --tray-action show → 必须重新可见
    # =====================================================================
    Write-Step 'A. 托盘动作 show：主窗口隐藏后应被隐藏入口重新显示'
    $aOut = Join-Path $workRoot 'a.out.log'
    $aErr = Join-Path $workRoot 'a.err.log'
    $app = Start-IsolatedInstance $ExePath $aOut $aErr
    $running += $app
    Write-Host ("  第一个实例 PID: {0}" -f $app.Id)

    $handle = Wait-VisibleWindow $app.Id 60
    if (-not (Write-Check '主窗口出现' ($handle -ne [IntPtr]::Zero) ("hwnd=0x{0:X}" -f [int64]$handle))) {
        $problems += 'a-no-window'
        throw '窗口都没出来，后面没法测'
    }
    $null = Wait-Until { (Get-LogText $aErr).Contains('托盘已就绪') } 20

    # 用已经验证过的关窗路径把窗口藏起来（不是鼠标点托盘）。
    $closed = Invoke-WindowClose $handle
    if (-not (Write-Check 'WM_CLOSE 投递成功（先制造"窗口隐藏"）' $closed 'PostMessage 返回值')) { $problems += 'a-postmessage' }
    $hidden = Wait-Until { (Test-WindowVisible $handle) -eq $false } 15
    if (-not (Write-Check '前置条件：窗口已隐藏' $hidden ("Visible=" + (Test-WindowVisible $handle)))) { $problems += 'a-not-hidden' }

    $showOut = Join-Path $workRoot 'a-show.out.log'
    $showErr = Join-Path $workRoot 'a-show.err.log'
    $showProc = Start-TrayAction $ExePath 'show' $showOut $showErr
    $running += $showProc
    $null = $showProc.WaitForExit(30000)
    $showProc.Refresh()
    if (-not (Write-Check 'show 转发进程退出码 = 0' ($showProc.HasExited -and $showProc.ExitCode -eq 0) ("HasExited=" + $showProc.HasExited + " ExitCode=" + $showProc.ExitCode))) {
        $problems += 'a-show-forward-exit'
    }
    $shown = Wait-Until { (Test-WindowVisible $handle) -eq $true } 15
    if (-not (Write-Check '派发 show 后窗口重新可见（IsWindowVisible=true）' $shown ("Visible=" + (Test-WindowVisible $handle)))) {
        $problems += 'a-show-visible'
    }
    $aLog = Get-LogText $aErr
    $showLogged = $aLog.Contains('source="cli"') -and $aLog.Contains('menu_id="desktop/tray/show"') -and $aLog.Contains('action="show"')
    if (-not (Write-Check '日志证明动作来自隐藏入口（source=cli / menu_id / action=show）' $showLogged 'stderr 找 source="cli" menu_id="desktop/tray/show" action="show"')) {
        $problems += 'a-show-log'
    }

    # =====================================================================
    # B. autostart：连派发两次 = 开 → 关，注册表与 desktop.json 都要翻转
    # =====================================================================
    Write-Step 'B. 托盘动作 autostart：第一次应开（写注册表），第二次应关（删注册表）'
    if (-not (Write-Check '前置条件：desktop.json autostart=false' ((Get-AutostartSetting $settingsPath) -eq $false) ("autostart=" + (Get-AutostartSetting $settingsPath)))) {
        $problems += 'b-precondition-json'
    }
    if (-not (Write-Check '前置条件：注册表没有 hbot 值' ($null -eq (Get-RunValue)) ("值为: " + (Get-RunValue)))) {
        $problems += 'b-precondition-reg'
    }

    foreach ($round in 1, 2) {
        $want = ($round -eq 1)
        $out = Join-Path $workRoot ("b{0}.out.log" -f $round)
        $err = Join-Path $workRoot ("b{0}.err.log" -f $round)
        $proc = Start-TrayAction $ExePath 'autostart' $out $err
        $running += $proc
        $null = $proc.WaitForExit(30000)
        $proc.Refresh()
        if (-not (Write-Check ("第 {0} 次 autostart 转发进程退出码 = 0" -f $round) ($proc.HasExited -and $proc.ExitCode -eq 0) ("HasExited=" + $proc.HasExited + " ExitCode=" + $proc.ExitCode))) {
            $problems += ("b{0}-forward-exit" -f $round)
        }
        $expectText = "开机自启已切换 autostart=$($want.ToString().ToLowerInvariant())"
        $null = Wait-Until { (Get-LogText $aErr).Contains($expectText) } 15
        $log = Get-LogText $aErr
        if (-not (Write-Check ("第 {0} 次动作日志（期望 autostart={1}）" -f $round, $want) ($log.Contains($expectText) -and $log.Contains('source="cli"') -and $log.Contains('action="toggleAutostart"')) $expectText)) {
            $problems += ("b{0}-log" -f $round)
        }
        $actualJson = Get-AutostartSetting $settingsPath
        if (-not (Write-Check ("第 {0} 次 desktop.json.autostart = {1}" -f $round, $want) ($actualJson -eq $want) ("实际=" + $actualJson))) {
            $problems += ("b{0}-json" -f $round)
        }
        $actualRun = Get-RunValue
        if ($want) {
            $expectedRun = '"' + $ExePath + '"'
            if (-not (Write-Check '第 1 次注册表 HKCU Run\hbot 已写入且指向当前 exe' ($actualRun -eq $expectedRun) ("实际=" + $actualRun + " 期望=" + $expectedRun))) {
                $problems += 'b1-reg'
            }
        }
        else {
            if (-not (Write-Check '第 2 次注册表 HKCU Run\hbot 已删除' ($null -eq $actualRun) ("实际=" + $actualRun))) {
                $problems += 'b2-reg'
            }
        }
    }

    # =====================================================================
    # C. quit：第一个实例应自行退出，退出码 0，且走优雅 shutdown
    # =====================================================================
    Write-Step 'C. 托盘动作 quit：第一个实例应优雅退出（退出码 0）'
    $quitOut = Join-Path $workRoot 'c-quit.out.log'
    $quitErr = Join-Path $workRoot 'c-quit.err.log'
    $quitProc = Start-TrayAction $ExePath 'quit' $quitOut $quitErr
    $running += $quitProc
    $null = $quitProc.WaitForExit(30000)
    $quitProc.Refresh()
    if (-not (Write-Check 'quit 转发进程退出码 = 0' ($quitProc.HasExited -and $quitProc.ExitCode -eq 0) ("HasExited=" + $quitProc.HasExited + " ExitCode=" + $quitProc.ExitCode))) {
        $problems += 'c-forward-exit'
    }

    $appExited = $app.WaitForExit(30000)
    $app.Refresh()
    if (-not (Write-Check '第一个实例自行退出（不是被杀的）' $appExited ("HasExited=" + $app.HasExited))) {
        $problems += 'c-hung'
    }
    elseif (-not (Write-Check '第一个实例退出码 = 0' ($app.ExitCode -eq 0) ("ExitCode=" + $app.ExitCode))) {
        $problems += 'c-exit-code'
    }
    $aLog = Get-LogText $aErr
    $quitLogged = $aLog.Contains('source="cli"') -and $aLog.Contains('menu_id="desktop/tray/quit"') -and $aLog.Contains('action="quit"')
    if (-not (Write-Check '日志证明 quit 来自隐藏入口' $quitLogged 'stderr 找 source="cli" / menu_id="desktop/tray/quit" / action="quit"')) {
        $problems += 'c-quit-log'
    }
    if (-not (Write-Check '日志证明走了优雅退出路径' ($aLog.Contains('桌面端开始优雅退出') -and $aLog.Contains('空库（没有机器人）')) 'stderr 找 "桌面端开始优雅退出" 与 "空库（没有机器人）"')) {
        $problems += 'c-graceful-log'
    }

    Write-Host '  第一个实例 stderr 中与派发相关的行：'
    ($aLog -split "`r?`n") | Where-Object { $_.Contains('托盘动作派发') -or $_.Contains('开机自启已切换') -or $_.Contains('桌面端开始优雅退出') -or $_.Contains('空库（没有机器人）') } | ForEach-Object { Write-Host "    $_" }
}
finally {
    $env:APPDATA = $savedAppData
    $env:USERPROFILE = $savedUserProfile

    # 还原真实用户的 HKCU Run 值：测试写过的必须清掉，原来有的必须放回去。
    try {
        if ($hadRunValue) {
            if (-not (Test-Path -LiteralPath $RunKey)) { New-Item -Path $RunKey -Force | Out-Null }
            Set-ItemProperty -LiteralPath $RunKey -Name $ValueName -Value $savedRunValue
            Write-Host "  已还原注册表 HKCU Run\$ValueName"
        }
        else {
            Remove-RunValue
            Write-Host "  已清理注册表 HKCU Run\$ValueName（测试前不存在）"
        }
    }
    catch {
        Write-Host ("  [WARN] 还原注册表失败：{0}" -f $_) -ForegroundColor Yellow
    }

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
    Write-Host 'A/B/C 全通过：show 让隐藏窗口重新可见；autostart 双向翻转 desktop.json + 注册表；quit 让进程退出码 0 且走优雅 shutdown。' -ForegroundColor Green
    Write-Host '注意：这验证的是「菜单 id → 动作」的派发，不是「鼠标点中托盘图标/菜单项」。'
    Write-Host "日志留在：$workRoot"
    exit 0
}
else {
    Write-Host ("失败项: " + ($problems -join ', ')) -ForegroundColor Red
    Write-Host "日志留在：$workRoot"
    exit 1
}
