#Requires -Version 7.0
# 为什么要求 PowerShell 7：Windows PowerShell 5.1 的 Start-Process -RedirectStandardError <file>
# 会把子进程的 .ExitCode 丢成空值（本机实测），而本脚本要靠退出码判定；另外 5.1 的
# Get-Content 默认按 GBK 解 UTF-8 日志，断言会全变成乱码。
<#
.SYNOPSIS
    端到端验证崩溃上报：第一次启动主动 panic 写报告，第二次正常启动把报告变成界面可读的日志行。

.DESCRIPTION
    两段验证，全是真的进程行为：

      A. **崩溃第一次启动**：设置 `HBOT_TEST_PANIC_ON_START=1`（只有 debug 构建 + 显式
         环境变量才触发，release 下这段代码不编译），进程必须 panic 退出（退出码 101），
         且 `<data_dir>\logs\crash-*.log` 必须落盘，内容包含稳定标记 `S6-CRASH-E2E`，
         不包含测试密钥原文，且密钥写成 `<redacted>`。

      B. **第二次正常启动**：不设置触发变量，进程正常启动；stderr 必须出现
         `上次运行异常退出` + 标记 + `已上报上次崩溃（logs.source=agent）`；
         原报告文件必须改名为 `*.reported.log`；再用 `--logs-tail 50` 读 logs 表，
         必须找到 `level=error`、`source=agent` 的同一行 —— 这是"界面侧 logs_tail
         能读到"的数据侧证据。

    结论只覆盖"崩溃报告 → 下次启动日志表/ stderr"这条链。**不覆盖**：真实界面卡片
    渲染（本机没有可用的 UI 断言工具）、真实鼠标点击、托盘图标外观、Linux/macOS。

.NOTES
    脚本用临时 APPDATA/USERPROFILE 隔离数据目录，不碰真实 `%APPDATA%\hbot`。
    运行前请关掉所有 hbot-desktop（单实例锁是全局的）。

.PARAMETER ExePath
    可执行文件路径，默认 `$env:CARGO_TARGET_DIR\debug\hbot-desktop.exe`。

.EXAMPLE
    pwsh -File scripts/verify-crash-report.ps1
#>
[CmdletBinding()]
param(
    [string]$ExePath = ''
)

$ErrorActionPreference = 'Stop'

# 测试密钥：只用于证明崩溃报告经过脱敏；脚本不打印它。
$SecretLiteral = 'sk-live-test-s6-crash-e2e-7f3a9b2c'
$Marker = 'S6-CRASH-E2E'

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

function Start-IsolatedInstance([string]$ExePath, [string]$StdOut, [string]$StdErr, [string[]]$Arguments = @()) {
    return Start-Process -FilePath $ExePath -ArgumentList $Arguments -PassThru `
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

$existing = @(Get-Process -Name 'hbot-desktop' -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
    Write-Host ("已有 {0} 个 hbot-desktop 在跑（PID: {1}）；单实例锁是全局的，请先关掉。" -f $existing.Count, (($existing | ForEach-Object { $_.Id }) -join ',')) -ForegroundColor Yellow
    exit 2
}

$workRoot = Join-Path $env:TEMP ("hbot-crash-e2e-{0}-{1}" -f $PID, (Get-Date -Format 'HHmmss'))
$appDataDir = Join-Path $workRoot 'appdata'
$userProfileDir = Join-Path $workRoot 'profile'
$dataDir = Join-Path $appDataDir 'hbot'
$logsDir = Join-Path $dataDir 'logs'
New-Item -ItemType Directory -Force -Path $logsDir, $userProfileDir | Out-Null

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
    # A. 第一次启动：HBOT_TEST_PANIC_ON_START=1 → panic 退出 + crash 文件
    # =====================================================================
    Write-Step 'A. 崩溃第一次启动（HBOT_TEST_PANIC_ON_START=1）'
    $aOut = Join-Path $workRoot 'a.out.log'
    $aErr = Join-Path $workRoot 'a.err.log'

    $env:HBOT_TEST_PANIC_ON_START = '1'
    try {
        $crashProc = Start-IsolatedInstance $ExePath $aOut $aErr
    }
    finally {
        Remove-Item Env:\HBOT_TEST_PANIC_ON_START -ErrorAction SilentlyContinue
    }
    $running += $crashProc
    Write-Host ("  PID: {0}" -f $crashProc.Id)

    $crashed = $crashProc.WaitForExit(60000)
    $crashProc.Refresh()
    if (-not (Write-Check '进程已退出（panic 没有被吞掉）' $crashed ("HasExited=" + $crashProc.HasExited))) {
        $problems += 'a-not-exited'
    }
    else {
        if (-not (Write-Check '退出码 = 101（Rust panic 默认）' ($crashProc.ExitCode -eq 101) ("ExitCode=" + $crashProc.ExitCode))) {
            $problems += 'a-exit-code'
        }
    }

    $null = Wait-Until {
        @(Get-ChildItem -LiteralPath $logsDir -Filter 'crash-*.log' -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -notlike '*.reported.log' }).Count -ge 1
    } 15
    $crashFile = @(Get-ChildItem -LiteralPath $logsDir -Filter 'crash-*.log' -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -notlike '*.reported.log' } | Sort-Object Name | Select-Object -Last 1)
    if (-not (Write-Check 'crash-*.log 已落盘' ($crashFile.Count -eq 1) ("未上报报告数: " + $crashFile.Count))) {
        $problems += 'a-no-crash-file'
        throw '没有崩溃报告，后面无法继续'
    }
    $crashFile = $crashFile[0]
    if (-not (Write-Check '文件名形如 crash-YYYYMMDDTHHMMSSZ.log（无冒号）' ($crashFile.Name -match '^crash-\d{8}T\d{6}Z\.log$') $crashFile.Name)) {
        $problems += 'a-file-name'
    }
    $report = [System.IO.File]::ReadAllText($crashFile.FullName)
    Write-Host "  报告路径: $($crashFile.FullName)"
    Write-Host '  报告前 12 行：'
    ($report -split "`r?`n" | Select-Object -First 12) | ForEach-Object { Write-Host "    $_" }
    if (-not (Write-Check '报告含稳定标记 S6-CRASH-E2E' ($report.Contains($Marker)) '报告里查找 marker')) {
        $problems += 'a-marker'
    }
    if (-not (Write-Check '报告里密钥被写成 <redacted>' ($report.Contains('api_key=<redacted>')) '报告里查找 api_key=<redacted>')) {
        $problems += 'a-redacted-token'
    }
    if (-not (Write-Check '报告不含测试密钥原文' (-not $report.Contains($SecretLiteral)) '报告里不得出现 SecretLiteral')) {
        $problems += 'a-secret-leaked'
    }

    # =====================================================================
    # B. 第二次正常启动：stderr 有崩溃行，logs_tail 能读到 error/agent 行
    # =====================================================================
    Write-Step 'B. 第二次正常启动（不设 HBOT_TEST_PANIC_ON_START）'
    $bOut = Join-Path $workRoot 'b.out.log'
    $bErr = Join-Path $workRoot 'b.err.log'
    $normalProc = Start-IsolatedInstance $ExePath $bOut $bErr
    $running += $normalProc
    Write-Host ("  PID: {0}" -f $normalProc.Id)

    $sawCrashLine = Wait-Until {
        $log = Get-LogText $bErr
        $log.Contains('上次运行异常退出') -and $log.Contains($Marker)
    } 60
    $bLog = Get-LogText $bErr
    if (-not (Write-Check 'stderr 出现「上次运行异常退出」+ 标记' $sawCrashLine 'stderr 里查找 上次运行异常退出 与 S6-CRASH-E2E')) {
        $problems += 'b-no-stderr-crash-line'
    }
    # `report_pending()` 在后台跑：stderr 先打「上次运行异常退出」，随后才写 logs 表、
    # 轮询确认落库（最多 5s）、再改名。别在 stderr 第一行出现时就抢着断言后两步。
    $reported = Wait-Until {
        $log = Get-LogText $bErr
        $marked = @(Get-ChildItem -LiteralPath $logsDir -Filter 'crash-*.reported.log' -File -ErrorAction SilentlyContinue).Count -ge 1
        $log.Contains('已上报上次崩溃（logs.source=agent）') -and $marked
    } 30
    $bLog = Get-LogText $bErr
    if (-not (Write-Check 'stderr 出现「已上报上次崩溃（logs.source=agent）」' $reported 'stderr 里查找 已上报上次崩溃')) {
        $problems += 'b-no-reported-line'
    }
    $reportedFiles = @(Get-ChildItem -LiteralPath $logsDir -Filter 'crash-*.reported.log' -File -ErrorAction SilentlyContinue)
    if (-not (Write-Check '原报告改名为 *.reported.log' ($reportedFiles.Count -ge 1) ("reported 文件数: " + $reportedFiles.Count))) {
        $problems += 'b-not-marked'
    }

    # --logs-tail 走的是同一条 logs 表读取路径（bot_id=None 的全项目视图）。
    Write-Step 'C. --logs-tail 50：logs 表里必须查到 source=agent 的 error 行'
    $tailOut = Join-Path $workRoot 'tail.out.log'
    $tailErr = Join-Path $workRoot 'tail.err.log'
    $tailProc = Start-IsolatedInstance $ExePath $tailOut $tailErr @('--logs-tail', '50')
    $running += $tailProc
    $tailExited = $tailProc.WaitForExit(30000)
    $tailProc.Refresh()
    if (-not (Write-Check '--logs-tail 进程退出码 = 0' ($tailExited -and $tailProc.ExitCode -eq 0) ("HasExited=" + $tailProc.HasExited + " ExitCode=" + $tailProc.ExitCode))) {
        $problems += 'c-tail-exit'
    }
    $tailText = Get-LogText $tailOut
    Write-Host "  --logs-tail 输出（前 400 字符）: $($tailText.Substring(0, [Math]::Min(400, $tailText.Length)))"
    $tailRows = @()
    try {
        $tailRows = @($tailText | ConvertFrom-Json)
    }
    catch {
        $tailRows = @()
    }
    $crashRow = @($tailRows | Where-Object { $_.line -like '*上次运行异常退出*' -and $_.line -like "*$Marker*" } | Select-Object -Last 1)
    if (-not (Write-Check 'logs_tail 里找到崩溃行' ($crashRow.Count -eq 1) ("匹配行数: " + $crashRow.Count))) {
        $problems += 'c-no-row'
    }
    else {
        $row = $crashRow[0]
        if (-not (Write-Check '该行 level = error' ($row.level -eq 'error') ("level=" + $row.level))) {
            $problems += 'c-level'
        }
        if (-not (Write-Check '该行 source = agent（bot_id=NULL 的 Agent 行）' ($row.source -eq 'agent') ("source=" + $row.source))) {
            $problems += 'c-source'
        }
        if (-not (Write-Check '该行同样不含测试密钥原文' (-not $row.line.Contains($SecretLiteral)) 'logs_tail 行检查 SecretLiteral')) {
            $problems += 'c-secret-leaked'
        }
    }

    # 用托盘退出的同一入口收尾，确保第一个实例优雅退出（不是被 taskkill）。
    $quitOut = Join-Path $workRoot 'quit.out.log'
    $quitErr = Join-Path $workRoot 'quit.err.log'
    $quitProc = Start-IsolatedInstance $ExePath $quitOut $quitErr @('--tray-action', 'quit')
    $running += $quitProc
    $null = $quitProc.WaitForExit(30000)
    $quitProc.Refresh()
    $normalExited = $normalProc.WaitForExit(30000)
    $normalProc.Refresh()
    if (-not (Write-Check '第一个实例收到 quit 后自行退出' ($normalExited -and $normalProc.ExitCode -eq 0) ("HasExited=" + $normalProc.HasExited + " ExitCode=" + $normalProc.ExitCode))) {
        $problems += 'c-normal-exit'
    }
}
finally {
    $env:APPDATA = $savedAppData
    $env:USERPROFILE = $savedUserProfile
    Remove-Item Env:\HBOT_TEST_PANIC_ON_START -ErrorAction SilentlyContinue

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
    Write-Host 'A/B/C 全通过：panic 落盘 crash-*.log（脱敏）、第二次启动 stderr 与 logs_tail 都能读到 error/agent 行。' -ForegroundColor Green
    Write-Host "日志留在：$workRoot"
    exit 0
}
else {
    Write-Host ("失败项: " + ($problems -join ', ')) -ForegroundColor Red
    Write-Host "日志留在：$workRoot"
    exit 1
}
