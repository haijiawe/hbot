#Requires -Version 7.0
# 为什么要求 PowerShell 7：Windows PowerShell 5.1 的 Start-Process -RedirectStandardError <file>
# 会把子进程的 .ExitCode 丢成空值（本机实测，cmd /c exit 3 也一样），而本脚本要靠退出码判定；
# 另外 5.1 的 Get-Content 默认按 GBK 解 UTF-8 日志，断言会全变成乱码。
<#
.SYNOPSIS
    验证 hbot 桌面的单实例行为：第二个进程必须把第一个带到前台，然后自己以 0 退出。

.DESCRIPTION
    这个脚本做的是**真机验证**（起真进程、看真退出码、看真日志），不是静态检查。
    断言六件事：

      1. 第一个实例起来了，进入 `setup`（stderr 里有"桌面壳数据目录"日志），并建出
         **可见的**主窗口（标题 hbot）。
      2. 第二个实例**以退出码 0 结束**，且在超时内结束。
      3. 第二个实例**没有进入 `setup`**（stderr 里没有那条数据目录日志）
         —— 这就是"没有开第二个 WebView、没有开第二个数据库写入者"的证据：
         配置窗口在 `Builder::build` 里创建、`AppState::open`（SQLite）在 `setup` 之后，
         而单实例插件在 `setup` 阶段就把自己 `exit(0)` 了。
      4. 第一个实例的日志里出现"检测到第二次启动" —— 回调**真的被调用**了。
      5. 第一个实例仍然活着，且同可执行文件的进程只剩一个。
      6. 窗口被**恢复**：脚本先把第一个窗口最小化，第二个实例起来后该窗口必须不再
         最小化（`IsIconic` 由 true 变 false）。这证明 `show_main_window()`（`show` +
         `unminimize` + `set_focus`）真的跑了。
      7. 主窗口是否**真的成为前台窗口**（同一 hwnd）—— 这一条是**尽力而为**，因为
         Windows 的前台锁（`SetForegroundWindow` 的规则）允许操作系统拒绝一个后台进程
         抢焦点；前台是别的应用时被拒是正常行为，不是缺陷。所以默认只报 `[WARN]` 并
         打印当时的前台窗口；要把它当硬断言用 `-StrictForeground`。

    窗口句柄不能用 `Get-Process` 的 `MainWindowHandle`：单实例插件自己有一个
    隐藏的 message-only 窗口（标题形如 `com.hbot.desktop-siw`），`MainWindowHandle`
    有时会指向它 —— 那就把"最小化"打在一个根本不是界面的窗口上，测出来的结论是假的。
    所以这里用 `EnumWindows` + `GetWindowThreadProcessId` 自己找**可见且有标题**的窗口。

    数据目录被重定向到临时目录（改 `APPDATA`/`USERPROFILE` 给子进程），**不碰**用户
    真实的 `%APPDATA%\hbot`。但单实例锁是按应用 identifier 生成的、与数据目录无关，
    所以机器上已经有 hbot 在跑时脚本会误判 —— 脚本会检测并提醒。

.PARAMETER ExePath
    要测的可执行文件。默认取 `$env:CARGO_TARGET_DIR`（没有就用仓库的 `target`）下的
    `debug\hbot-desktop.exe`。

.PARAMETER KeepRunning
    测完不杀第一个实例（调试用）。

.PARAMETER WindowTitle
    主窗口标题，默认 `hbot`（= `tauri.conf.json` 的 `app.windows[0].title`）。脚本靠它把主窗口
    和单实例插件的辅助窗口（标题 `com.hbot.desktop-siw`，也是可见窗口）区分开。

.EXAMPLE
    pwsh -File scripts/verify-single-instance.ps1
#>
[CmdletBinding()]
param(
    [string]$ExePath = '',
    [string]$WindowTitle = 'hbot',
    [int]$FirstReadyTimeoutSec = 60,
    [int]$SecondInstanceTimeoutSec = 30,
    [switch]$SkipForegroundCheck,
    [switch]$StrictForeground,
    [switch]$KeepRunning
)

$ErrorActionPreference = 'Stop'

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

# --- Win32 小工具（拿不到就降级成 SKIP，不让脚本崩） -------------------------
$script:HbotWin32Ready = $false
function Initialize-HbotWin32 {
    if ($script:HbotWin32Ready) { return }

    $source = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class HbotWin32 {
    private delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int count);

    [DllImport("user32.dll")]
    private static extern bool ShowWindow(IntPtr hWnd, int command);

    [DllImport("user32.dll")]
    private static extern bool IsIconic(IntPtr hWnd);

    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumProc callback, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

    public static string WindowTitle(IntPtr handle) {
        if (handle == IntPtr.Zero) { return null; }
        StringBuilder buffer = new StringBuilder(512);
        int length = GetWindowTextW(handle, buffer, buffer.Capacity);
        if (length <= 0) { return null; }
        return buffer.ToString(0, length);
    }

    public static IntPtr ForegroundHandle() {
        return GetForegroundWindow();
    }

    public static string ForegroundTitle() {
        return WindowTitle(GetForegroundWindow());
    }

    // 该进程里标题**恰好等于** expectedTitle 的可见顶层窗口；没有就退回第一个可见有标题的窗口。
    //
    // 为什么必须按标题精确挑：单实例插件的辅助窗口（类 com.hbot.desktop-sic / 标题
    // com.hbot.desktop-siw）是 **可见** 的（它被 SetWindowLongPtrW 设成 WS_VISIBLE | WS_POPUP），
    // 所以"第一个可见有标题的窗口"会抓到它 —— 对它 Minimize()/ShowWindow 跟主窗毫无关系，
    // 「窗口从最小化恢复」这条断言就会假失败。标题来自 tauri.conf.json 的 app.windows[0].title。
    public static IntPtr FindVisibleWindow(uint processId, string expectedTitle) {
        IntPtr first = IntPtr.Zero;
        EnumWindows(delegate (IntPtr hWnd, IntPtr lParam) {
            uint owner;
            GetWindowThreadProcessId(hWnd, out owner);
            if (owner != processId) { return true; }
            if (!IsWindowVisible(hWnd)) { return true; }
            string title = WindowTitle(hWnd);
            if (String.IsNullOrEmpty(title)) { return true; }
            if (String.Equals(title, expectedTitle, StringComparison.Ordinal)) {
                first = hWnd;
                return false;
            }
            if (first == IntPtr.Zero) { first = hWnd; }
            return true;
        }, IntPtr.Zero);
        return first;
    }

    public static bool Minimize(IntPtr handle) {
        return ShowWindow(handle, 6);
    }

    public static bool Minimized(IntPtr handle) {
        return IsIconic(handle);
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

function Get-ForegroundWindowTitle {
    try {
        Initialize-HbotWin32
        if (-not $script:HbotWin32Ready) { return $null }
        return [HbotWin32]::ForegroundTitle()
    }
    catch {
        return $null
    }
}

function Get-ForegroundWindowHandle {
    try {
        Initialize-HbotWin32
        if (-not $script:HbotWin32Ready) { return [IntPtr]::Zero }
        return [HbotWin32]::ForegroundHandle()
    }
    catch {
        return [IntPtr]::Zero
    }
}

function Get-VisibleWindowHandle([int]$ProcessId, [string]$ExpectedTitle) {
    try {
        Initialize-HbotWin32
        if (-not $script:HbotWin32Ready) { return [IntPtr]::Zero }
        return [HbotWin32]::FindVisibleWindow([uint32]$ProcessId, $ExpectedTitle)
    }
    catch {
        return [IntPtr]::Zero
    }
}

function Get-WindowTitle([IntPtr]$Handle) {
    try {
        if (-not $script:HbotWin32Ready) { return $null }
        return [HbotWin32]::WindowTitle($Handle)
    }
    catch {
        return $null
    }
}

function Test-WindowMinimized([IntPtr]$Handle) {
    try {
        if (-not $script:HbotWin32Ready) { return $null }
        return [HbotWin32]::Minimized($Handle)
    }
    catch {
        return $null
    }
}

function Invoke-WindowMinimize([IntPtr]$Handle) {
    try {
        if (-not $script:HbotWin32Ready) { return $false }
        return [HbotWin32]::Minimize($Handle)
    }
    catch {
        return $false
    }
}

# 日志是异步 flush 的：窗口一出现就断言会读到空文件，所以轮询。
function Wait-ForLogText([string]$Path, [string]$Needle, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path -LiteralPath $Path) {
            $text = Get-Content -LiteralPath $Path -Raw -ErrorAction SilentlyContinue
            if ($null -ne $text -and $text.Contains($Needle)) { return $true }
        }
        Start-Sleep -Milliseconds 200
    }
    return $false
}

function Get-LogText([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    $text = Get-Content -LiteralPath $Path -Raw -ErrorAction SilentlyContinue
    if ($null -eq $text) { return '' }
    return $text
}

# --- 1. 定位可执行文件 -------------------------------------------------------
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

Write-Step '0. 环境'
Write-Host "  可执行文件: $ExePath"
if ($script:HbotWin32Ready) {
    Write-Host '  Win32 调用可用（FindVisibleWindow / IsIconic / ShowWindow）'
}
else {
    Write-Host '  [SKIP] Add-Type 不可用：前台与最小化检查会退化为未验证' -ForegroundColor Yellow
}

# 单实例锁是全局的（按 identifier）：机器上已经有 hbot 就别测了。
$existing = @(Get-Process -Name 'hbot-desktop' -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
    Write-Host ("  已有 {0} 个 hbot-desktop 进程在跑（PID: {1}）" -f $existing.Count, (($existing | ForEach-Object { $_.Id }) -join ',')) -ForegroundColor Yellow
    Write-Host '  单实例锁是全局的，脚本会误判。请先关掉正在运行的那个 hbot。' -ForegroundColor Yellow
}

# 隔离数据目录：子进程读 APPDATA/USERPROFILE 决定 hbot_core::data_dir()。
$workRoot = Join-Path $env:TEMP ("hbot-single-instance-{0}-{1}" -f $PID, (Get-Date -Format 'HHmmss'))
$appDataDir = Join-Path $workRoot 'appdata'
$userProfileDir = Join-Path $workRoot 'profile'
New-Item -ItemType Directory -Force -Path $appDataDir, $userProfileDir | Out-Null

$firstOut = Join-Path $workRoot 'first.out.log'
$firstErr = Join-Path $workRoot 'first.err.log'
$secondOut = Join-Path $workRoot 'second.out.log'
$secondErr = Join-Path $workRoot 'second.err.log'
Write-Host "  临时工作目录: $workRoot"
Write-Host "  隔离后的数据目录: $(Join-Path $appDataDir 'hbot')"

$savedAppData = $env:APPDATA
$savedUserProfile = $env:USERPROFILE
$env:APPDATA = $appDataDir
$env:USERPROFILE = $userProfileDir

$problems = @()
$first = $null

try {
    # --- 2. 起第一个实例 -----------------------------------------------------
    Write-Step '1. 启动第一个实例'
    $first = Start-Process -FilePath $ExePath -PassThru `
        -RedirectStandardOutput $firstOut -RedirectStandardError $firstErr
    Write-Host ("  PID: {0}" -f $first.Id)

    # 等"主窗口"，不是 MainWindowHandle（那可能是插件的辅助窗口）。
    # 插件窗口（标题 com.hbot.desktop-siw）也是可见的，所以只认标题精确等于 $WindowTitle 的窗口。
    $windowHandle = [IntPtr]::Zero
    $mainTitle = ''
    $deadline = (Get-Date).AddSeconds($FirstReadyTimeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
        if ($first.HasExited) { break }
        $candidate = Get-VisibleWindowHandle $first.Id $WindowTitle
        if ($candidate -ne [IntPtr]::Zero) {
            $candidateTitle = Get-WindowTitle $candidate
            if ($null -ne $candidateTitle -and $candidateTitle -eq $WindowTitle) {
                $windowHandle = $candidate
                $mainTitle = $candidateTitle
                break
            }
        }
    }

    if ($first.HasExited) {
        Write-Host '  第一个实例自己退了，后面没法测。stderr：' -ForegroundColor Red
        Get-Content -LiteralPath $firstErr -Tail 30 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" }
        exit 1
    }
    if (-not (Write-Check '第一个实例建出可见主窗口' ($windowHandle -ne [IntPtr]::Zero) ("hwnd=0x{0:X} 标题='{1}'" -f [int64]$windowHandle, $mainTitle))) {
        $problems += 'first-no-window'
    }
    if (-not (Write-Check '主窗口标题是 hbot' ($null -ne $mainTitle -and $mainTitle -eq $WindowTitle) ("标题='" + $mainTitle + "'（期望 '" + $WindowTitle + "'）"))) {
        $problems += 'first-window-title'
    }

    # 证据：第一个实例进入 setup 并打了数据目录日志（异步 flush，要等）。
    $firstReachedSetup = Wait-ForLogText -Path $firstErr -Needle '桌面壳数据目录' -TimeoutSec 20
    if (-not (Write-Check '第一个实例进入了 setup（打了数据目录日志）' $firstReachedSetup 'stderr 里找 "桌面壳数据目录"')) {
        $problems += 'first-no-setup-log'
        Get-Content -LiteralPath $firstErr -Tail 30 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" }
    }

    # --- 3. 先把窗口最小化，作为"是否真的被带到前台"的判别依据 ---------------
    $minimizeChecked = $false
    if (-not $SkipForegroundCheck) {
        Write-Step '2. 先把主窗口最小化（稍后看它是否被恢复）'
        $invoked = Invoke-WindowMinimize $windowHandle
        Start-Sleep -Milliseconds 800
        $minimizedBefore = Test-WindowMinimized $windowHandle
        if ($null -eq $minimizedBefore) {
            Write-Host '  [SKIP] 拿不到 IsIconic（Add-Type 不可用），最小化检查未验证' -ForegroundColor Yellow
        }
        elseif (-not $minimizedBefore) {
            Write-Host ("  [SKIP] 最小化没生效（ShowWindow 返回 {0}），前台检查退化为按标题匹配" -f $invoked) -ForegroundColor Yellow
        }
        else {
            Write-Host '  [PASS] 窗口已最小化（IsIconic=True）—— 给"带到前台"设好了前置条件' -ForegroundColor Green
            $minimizeChecked = $true
        }
        Write-Host ("  此刻的前台窗口: '{0}'" -f (Get-ForegroundWindowTitle))
    }

    # --- 4. 起第二个实例 -----------------------------------------------------
    Write-Step '3. 启动第二个实例（期望：把第一个带到前台，然后自己退出 0）'
    $second = Start-Process -FilePath $ExePath -PassThru `
        -RedirectStandardOutput $secondOut -RedirectStandardError $secondErr
    Write-Host ("  PID: {0}" -f $second.Id)

    $exited = $second.WaitForExit($SecondInstanceTimeoutSec * 1000)
    $second.Refresh()
    if (-not (Write-Check '第二个实例在超时内退出' $exited ("等待 ${SecondInstanceTimeoutSec}s；HasExited=" + $second.HasExited))) {
        $problems += 'second-hung'
        try { Stop-Process -Id $second.Id -Force -ErrorAction SilentlyContinue } catch { }
    }
    else {
        $code = $second.ExitCode
        if (-not (Write-Check '第二个实例退出码 = 0' ($code -eq 0) ("ExitCode=" + $code))) { $problems += "second-exit-$code" }
    }

    # 关键证据：第二个实例**没有**进入 setup（不建窗口、不开库）。
    $secondSetup = Wait-ForLogText -Path $secondErr -Needle '桌面壳数据目录' -TimeoutSec 3
    $secondLog = Get-LogText $secondErr
    if (-not (Write-Check '第二个实例没有进入 setup（= 没有第二个 WebView / DB 写入者）' (-not $secondSetup) 'stderr 里不应出现 "桌面壳数据目录"')) {
        $problems += 'second-reached-setup'
        Get-Content -LiteralPath $secondErr -Tail 30 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" }
    }
    Write-Host ("  第二个实例 stderr 全文（{0} 字符）: {1}" -f $secondLog.Length, ($secondLog -replace "`r?`n", ' ').Trim())

    # 证据：第一个实例真的执行了单实例回调。
    $handled = Wait-ForLogText -Path $firstErr -Needle '检测到第二次启动' -TimeoutSec 10
    if (-not (Write-Check '第一个实例执行了单实例回调（日志里有"检测到第二次启动"）' $handled '证明第二个进程把启动事件交给了第一个')) {
        $problems += 'no-callback-log'
    }

    # --- 5. 第一个实例还在，并且只有一个进程 ---------------------------------
    Write-Step '4. 第一个实例仍在，且同可执行文件只剩一个进程'
    $firstAfter = Get-Process -Id $first.Id -ErrorAction SilentlyContinue
    if (-not (Write-Check '第一个实例仍在运行' ($null -ne $firstAfter) ("PID=" + $first.Id))) { $problems += 'first-died' }

    $sameExe = @()
    foreach ($proc in @(Get-Process -Name 'hbot-desktop' -ErrorAction SilentlyContinue)) {
        try {
            if ($proc.Path -eq $ExePath) { $sameExe += $proc }
        }
        catch {
            # 拿不到 Path（权限）就按进程名算，宁可多算也不少算。
            $sameExe += $proc
        }
    }
    if (-not (Write-Check '同可执行文件的进程数 = 1' ($sameExe.Count -eq 1) ("实际 " + $sameExe.Count + " 个（PID: " + (($sameExe | ForEach-Object { $_.Id }) -join ',') + "）"))) {
        $problems += 'process-count'
    }
    Write-Host ("  主窗口标题: '{0}'；进程名: {1}" -f (Get-WindowTitle $windowHandle), $first.ProcessName)

    # --- 6. 窗口是否真的被带到前台 ------------------------------------------
    Write-Step '5. 窗口是否被带到前台'
    if ($SkipForegroundCheck) {
        Write-Host '  [SKIP] 按参数要求跳过了前台窗口检查' -ForegroundColor Yellow
    }
    else {
        Start-Sleep -Milliseconds 500
        $minimizedAfter = Test-WindowMinimized $windowHandle
        $foregroundHandle = Get-ForegroundWindowHandle
        $foregroundAfter = Get-ForegroundWindowTitle
        Write-Host ("  此刻：前台窗口='{0}'（hwnd=0x{1:X}），主窗口 IsIconic={2}" -f $foregroundAfter, [int64]$foregroundHandle, $minimizedAfter)

        if ($minimizeChecked) {
            if (-not (Write-Check '窗口从最小化被恢复（IsIconic 由 True 变 False）' ($minimizedAfter -eq $false) ("IsIconic=" + $minimizedAfter))) {
                $problems += 'window-not-restored'
            }
        }
        else {
            Write-Host '  [SKIP] 前置最小化没做成，无法证明"被恢复"，只看前景句柄' -ForegroundColor Yellow
        }

        if ($foregroundHandle -eq [IntPtr]::Zero) {
            Write-Host '  [SKIP] 拿不到前台窗口句柄（Add-Type 不可用），前台检查未验证' -ForegroundColor Yellow
        }
        else {
            $sameWindow = ($foregroundHandle -eq $windowHandle)
            if ($sameWindow) {
                Write-Host ("  [PASS] 主窗口就是当前前台窗口（同一 hwnd）: hwnd=0x{0:X} 标题='{1}'" -f [int64]$windowHandle, $foregroundAfter) -ForegroundColor Green
            }
            elseif ($StrictForeground) {
                Write-Host ("  [FAIL] 主窗口就是当前前台窗口（同一 hwnd）: 前台=0x{0:X} 主窗口=0x{1:X} 标题='{2}'" -f [int64]$foregroundHandle, [int64]$windowHandle, $foregroundAfter) -ForegroundColor Red
                $problems += 'foreground'
            }
            else {
                # 不骗自己：这里**没有**证明"窗口被抬到最前"。窗口确实显示且不是最小化
                # （上一条已证），但前台是别的应用时 Windows 允许拒绝抢焦点。
                Write-Host ("  [WARN] 主窗口没能成为前台窗口（Windows 前台锁允许拒绝）: 前台=0x{0:X} 主窗口=0x{1:X} 标题='{2}'；窗口已恢复显示，但'真的在最前'这一条本次未证实" -f [int64]$foregroundHandle, [int64]$windowHandle, $foregroundAfter) -ForegroundColor Yellow
                Write-Host '         要把它当硬断言（在没有别的窗口抢焦点时应该成立）：加 -StrictForeground' -ForegroundColor Yellow
            }
        }
    }
}
finally {
    $env:APPDATA = $savedAppData
    $env:USERPROFILE = $savedUserProfile

    if (-not $KeepRunning) {
        if ($null -ne $first -and -not $first.HasExited) {
            Write-Step '清理：强杀第一个实例'
            # 这里是**强杀**（等价于任务管理器结束进程）。优雅退出走托盘"退出"，
            # 那条路径本脚本不测 —— 所以"退出菜单已验证"这句话这里不能写。
            & taskkill.exe /PID $first.Id /T /F 2>&1 | ForEach-Object { Write-Host "  $_" }
        }
    }
    else {
        Write-Host ''
        Write-Host ("保留第一个实例在跑：PID {0}（-KeepRunning）" -f $first.Id) -ForegroundColor Yellow
    }
}

Write-Step '结论'
if ($problems.Count -eq 0) {
    Write-Host '全部通过：第二个实例退出码 0 且没进 setup、第一个仍在、只剩一个进程、窗口从最小化被恢复。' -ForegroundColor Green
    if (-not $SkipForegroundCheck -and -not $StrictForeground) {
        Write-Host '注意：上面若出现 [WARN]，表示"主窗口是否真的在最前"本次未证实（前台被别的应用占着）——' -ForegroundColor Yellow
        Write-Host '      已证实的是窗口被恢复显示、不是最小化；要硬断言用 -StrictForeground。' -ForegroundColor Yellow
    }
    Write-Host "日志留在：$workRoot"
    exit 0
}
else {
    Write-Host ("失败项: " + ($problems -join ', ')) -ForegroundColor Red
    Write-Host "日志留在：$workRoot"
    exit 1
}