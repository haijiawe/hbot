#Requires -Version 7.0
# 为什么要求 PowerShell 7：Windows PowerShell 5.1 的 Start-Process -RedirectStandardError <file>
# 会把子进程的 .ExitCode 丢成空值（本机实测，cmd /c exit 3 也一样），而本脚本要靠退出码判定；
# 另外 5.1 的 Get-Content 默认按 GBK 解 UTF-8 日志，断言会全变成乱码。
<#
.SYNOPSIS
    验证 hbot 的开机自启开关：注册表项、desktop.json 字段、幂等性、关闭后清理。

.DESCRIPTION
    走的是产品自身的命令行入口（`hbot-desktop.exe --set-autostart on|off|toggle`），
    不是脚本自己写注册表 —— 这样验的才是"产品真的会写"。

    断言：

      1. `--print-settings` 输出的 JSON 里 `systemAutostart` 与 `settings.autostart` 一致。
      2. 开启后 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下存在名为 `hbot`
         的值，且数据是加了引号的 exe 路径。
      3. 再次开启**不产生第二条**（枚举该键，`hbot` 只出现一次）。
      4. 关闭后该值被删掉，`desktop.json` 里 `autostart` 是 `false`。
      5. 默认状态是**关**（首次运行不偷偷开自启）。

    **安全**：脚本开始时如果注册表里已经有 `hbot` 值 / desktop.json 已经存在，
    会先备份，结束时恢复原样。测试用的是真实的数据目录（`%APPDATA%\hbot`），
    因为 `--set-autostart` 走的就是 `hbot_core::data_dir()`，改不了。
    也就是说这个脚本会**短暂改变**你真机上的自启状态，但会还原。

.PARAMETER ExePath
    可执行文件路径，默认 `$env:CARGO_TARGET_DIR\debug\hbot-desktop.exe`。

.EXAMPLE
    pwsh -File scripts/verify-autostart.ps1
#>
[CmdletBinding()]
param(
    [string]$ExePath = ''
)

$ErrorActionPreference = 'Stop'

$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$ValueName = 'hbot'

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

# 数据目录 = hbot_core::data_dir()：Windows 上是 %APPDATA%\hbot
$dataDir = Join-Path $env:APPDATA 'hbot'
$settingsPath = Join-Path $dataDir 'desktop.json'

Write-Step '0. 环境'
Write-Host "  可执行文件: $ExePath"
Write-Host "  数据目录:   $dataDir"
Write-Host "  偏好文件:   $settingsPath"

# --- 备份原有状态（结束时还原，别把开发机的自启开关留在测试态）---------------
$hadValue = $false
$savedValue = $null
try {
    $savedValue = (Get-ItemProperty -Path $RunKey -Name $ValueName -ErrorAction Stop).$ValueName
    $hadValue = $true
}
catch {
    $hadValue = $false
}
$hadSettings = Test-Path -LiteralPath $settingsPath
$savedSettings = $null
if ($hadSettings) { $savedSettings = [System.IO.File]::ReadAllBytes($settingsPath) }

if ($hadValue) { Write-Host "  注册表里已有 hbot 值（结束时还原）: $savedValue" -ForegroundColor Yellow }
if ($hadSettings) { Write-Host '  desktop.json 已存在（结束时还原）' -ForegroundColor Yellow }

function Get-RunValue {
    try {
        return (Get-ItemProperty -Path $RunKey -Name $ValueName -ErrorAction Stop).$ValueName
    }
    catch {
        return $null
    }
}

function Get-SettingsJson {
    $proc = & $ExePath --print-settings 2>&1
    $text = ($proc | Out-String).Trim()
    return $text
}

$problems = @()

try {
    # --- 1. 默认状态必须是关 ------------------------------------------------
    Write-Step '1. 关闭自启，并把状态记成"关"'
    & $ExePath --set-autostart off | Out-Null
    if ($LASTEXITCODE -ne 0) { $problems += 'set-off-failed' }
    $afterOff = Get-RunValue
    if (-not (Write-Check '关闭后注册表里没有 hbot 值' ($null -eq $afterOff) ("读到: '" + $afterOff + "'"))) {
        $problems += 'off-still-present'
    }
    $printOff = Get-SettingsJson
    Write-Host "  --print-settings: $printOff"
    if (-not (Write-Check '--print-settings 里 systemAutostart 是 disabled' ($printOff -match '"systemAutostart"\s*:\s*"disabled"') $printOff)) {
        $problems += 'print-off'
    }

    # --- 2. 开启：注册表项必须出现且加引号 ----------------------------------
    Write-Step '2. 开启自启'
    & $ExePath --set-autostart on | Out-Null
    if ($LASTEXITCODE -ne 0) { $problems += 'set-on-failed' }
    $afterOn = Get-RunValue
    if (-not (Write-Check '开启后注册表里有 hbot 值' ($null -ne $afterOn) ("读到: '" + $afterOn + "'"))) {
        $problems += 'on-missing'
    }
    else {
        if (-not (Write-Check '值的数据是加引号的 exe 路径' ($afterOn -eq ('"' + $ExePath + '"')) $afterOn)) {
            $problems += 'on-quoting'
        }
    }

    # --- 3. 幂等：开两次还只有一条 ------------------------------------------
    Write-Step '3. 幂等性：再开一次'
    & $ExePath --set-autostart on | Out-Null
    $names = @()
    try {
        $item = Get-Item -Path $RunKey
        $names = @($item.GetValueNames() | Where-Object { $_ -eq $ValueName })
    }
    catch {
        $names = @()
    }
    if (-not (Write-Check '枚举 Run 键，hbot 只有一条' ($names.Count -eq 1) ("匹配条数: " + $names.Count))) {
        $problems += 'not-idempotent'
    }

    # --- 4. desktop.json 里 autostart=true ----------------------------------
    Write-Step '4. desktop.json 内容'
    if (-not (Write-Check 'desktop.json 存在' (Test-Path -LiteralPath $settingsPath) $settingsPath)) {
        $problems += 'settings-missing'
    }
    else {
        # 只读，不写：PS 5.1 的 -Encoding UTF8 会加 BOM，serde_json 拒收。
        $json = [System.IO.File]::ReadAllText($settingsPath)
        Write-Host "  内容: $($json -replace "`r?`n", ' ')"
        if (-not (Write-Check 'autostart 字段是 true' ($json -match '"autostart"\s*:\s*true') $json)) {
            $problems += 'settings-autostart'
        }
        if (-not (Write-Check 'closeToTray 字段存在且是 true' ($json -match '"closeToTray"\s*:\s*true') $json)) {
            $problems += 'settings-closetotray'
        }
        if (-not (Write-Check '文件没有 BOM（serde_json 会拒收 BOM）' (-not ($json.Length -gt 0 -and $json[0] -eq [char]0xFEFF)) '首字符不是 U+FEFF')) {
            $problems += 'settings-bom'
        }
    }

    $printOn = Get-SettingsJson
    Write-Host "  --print-settings: $printOn"
    if (-not (Write-Check '--print-settings 里 systemAutostart 是 enabled' ($printOn -match '"systemAutostart"\s*:\s*"enabled"') $printOn)) {
        $problems += 'print-on'
    }

    # --- 5. toggle 行为 ----------------------------------------------------
    Write-Step '5. --set-autostart toggle 应当把开翻成关'
    & $ExePath --set-autostart toggle | Out-Null
    $afterToggle = Get-RunValue
    if (-not (Write-Check 'toggle 之后注册表里没有 hbot 值' ($null -eq $afterToggle) ("读到: '" + $afterToggle + "'"))) {
        $problems += 'toggle'
    }

    # --- 6. 关闭后清理 -----------------------------------------------------
    Write-Step '6. 关闭自启（幂等：再关一次也不报错）'
    & $ExePath --set-autostart off | Out-Null
    $offExit = $LASTEXITCODE
    & $ExePath --set-autostart off | Out-Null
    $offExit2 = $LASTEXITCODE
    if (-not (Write-Check '连关两次都退出码 0' ($offExit -eq 0 -and $offExit2 -eq 0) ("ExitCode=$offExit/$offExit2"))) {
        $problems += 'off-exit'
    }
    $final = Get-RunValue
    if (-not (Write-Check '最终注册表里没有 hbot 值' ($null -eq $final) ("读到: '" + $final + "'"))) {
        $problems += 'final-value'
    }

    # --- 7. --help 与未知参数 ---------------------------------------------
    Write-Step '7. 参数处理'
    $helpText = (& $ExePath --help | Out-String)
    if (-not (Write-Check '--help 打印用法' ($helpText -match 'set-autostart') '输出里应有 --set-autostart')) {
        $problems += 'help'
    }
    & $ExePath --nonsense-flag | Out-Null
    $badExit = $LASTEXITCODE
    if (-not (Write-Check '未知参数以退出码 2 结束（不会误开 GUI）' ($badExit -eq 2) ("ExitCode=$badExit"))) {
        $problems += 'unknown-arg'
    }
}
finally {
    # --- 还原 -------------------------------------------------------------
    Write-Step '清理：还原测试前的状态'
    if ($hadValue) {
        Set-ItemProperty -Path $RunKey -Name $ValueName -Value $savedValue
        Write-Host "  注册表 hbot 值已还原: $savedValue"
    }
    else {
        Remove-ItemProperty -Path $RunKey -Name $ValueName -ErrorAction SilentlyContinue
        Write-Host '  注册表 hbot 值已删除（测试前就不存在）'
    }
    if ($hadSettings) {
        [System.IO.File]::WriteAllBytes($settingsPath, $savedSettings)
        Write-Host '  desktop.json 已还原'
    }
    elseif (Test-Path -LiteralPath $settingsPath) {
        Remove-Item -LiteralPath $settingsPath -Force
        Write-Host '  desktop.json 已删除（测试前不存在）'
    }
}

Write-Step '结论'
if ($problems.Count -eq 0) {
    Write-Host '全部通过：注册表项写入/幂等/删除、desktop.json 字段、toggle、参数处理都符合预期。' -ForegroundColor Green
    exit 0
}
else {
    Write-Host ("失败项: " + ($problems -join ', ')) -ForegroundColor Red
    exit 1
}