<#
.SYNOPSIS
    NSIS 安装包冒烟测试：静默安装 → 启动进程/窗口 → 关闭 → 卸载 → 检查残留。

.DESCRIPTION
    给 `cargo tauri build --bundles nsis` 产出的 `*-setup.exe` 用。
    Tauri 的 NSIS 模板支持 `/S`（静默）与 `/D=<安装目录>`（必须是最后一个参数、不能带引号）。

    本脚本默认装到 `%LOCALAPPDATA%\hbot-smoke` 并在结束时**卸载 + 清理**，
    它检查的五件事：
      1. 安装器退出码为 0，且安装目录里出现主 exe 与 uninstall.exe；
      2. 启动主 exe 后进程活着、且在超时内**出现窗口**（MainWindowTitle 非空）；
      3. 关闭后进程真的退出；
      4. 卸载器退出码为 0，安装目录被删掉；
      5. HKCU 的卸载注册表项、开始菜单快捷方式不留残留。

    用法：
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-nsis-install.ps1 `
            -Installer D:\hbot-target\pkg\release\bundle\nsis\hbot_0.1.0_x64-setup.exe

    参数：
        -Installer      安装包路径（必填）
        -InstallDir     安装目标目录，默认 %LOCALAPPDATA%\hbot-smoke
        -WindowTimeoutSec  等窗口出现的上限，默认 30
        -KeepInstalled  装完不卸载（排查用；此时不检查残留）

    退出码：0 = 五项全过；1 = 任一失败（逐步打印结果）。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Installer,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'hbot-smoke'),
    [int]$WindowTimeoutSec = 30,
    [switch]$SkipWindowCheck,
    [switch]$KeepInstalled
)

$ErrorActionPreference = 'Continue'
$script:failures = 0
$script:steps = New-Object System.Collections.ArrayList

function Step([string]$name, [bool]$passed, [string]$detail) {
    $tag = if ($passed) { 'PASS' } else { 'FAIL' }
    [void]$script:steps.Add([pscustomobject]@{ Step = $name; Result = $tag; Detail = $detail })
    if (-not $passed) { $script:failures++ }
    Write-Output ("[{0}] {1}: {2}" -f $tag, $name, $detail)
}

function Get-TotalBytes([string]$dir) {
    if (-not (Test-Path $dir)) { return 0 }
    $sum = (Get-ChildItem $dir -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    if ($null -eq $sum) { return 0 }
    return [int64]$sum
}

function Get-UninstallKeys([string]$pattern) {
    $roots = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    $found = @()
    foreach ($r in $roots) {
        if (-not (Test-Path $r)) { continue }
        Get-ChildItem $r -ErrorAction SilentlyContinue | ForEach-Object {
            $props = Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue
            $name = [string]$props.DisplayName
            if ($name -and $name -match $pattern) {
                $found += [pscustomobject]@{ Root = $r; Key = $_.PSChildName; DisplayName = $name }
            }
        }
    }
    return $found
}

function Get-Shortcuts([string]$pattern) {
    $roots = @(
        (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
        (Join-Path $env:PROGRAMDATA 'Microsoft\Windows\Start Menu\Programs'),
        ([Environment]::GetFolderPath('Desktop'))
    )
    $found = @()
    foreach ($r in $roots) {
        if (-not (Test-Path $r)) { continue }
        Get-ChildItem $r -Recurse -Filter '*.lnk' -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match $pattern } |
            ForEach-Object { $found += $_.FullName }
    }
    return $found
}

Write-Output "=== 0. 前置检查 ==="
if (-not (Test-Path $Installer)) {
    Write-Output "安装包不存在: $Installer"
    exit 1
}
$installerItem = Get-Item $Installer
Write-Output ("安装包: {0}" -f $installerItem.FullName)
Write-Output ("体积:   {0} 字节 ({1:N2} MiB)" -f $installerItem.Length, ($installerItem.Length / 1MB))
Write-Output ("安装目录: {0}" -f $InstallDir)

# 让冒烟与「真机已有安装」互不干扰：先记录再断言，避免把用户的安装误卸载
$preExistingInstall = Test-Path (Join-Path $env:LOCALAPPDATA 'hbot')
$preExistingKeys = Get-UninstallKeys '^hbot$|hbot'
Write-Output ("已有 %LOCALAPPDATA%\hbot 安装: {0}；已有卸载注册表项: {1} 个" -f $preExistingInstall, $preExistingKeys.Count)
$preExistingShortcuts = Get-Shortcuts '^hbot'
Write-Output ("已有开始菜单/桌面快捷方式: {0} 个" -f $preExistingShortcuts.Count)

if (Test-Path $InstallDir) {
    Write-Output "清理上一次冒烟留下的 $InstallDir"
    Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
}

# WebView2 已存在时安装器不会联网装 bootstrapper —— 记下来，免得把「没装 WebView2」误判为 bug
$wvKey = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$wvVersion = if (Test-Path $wvKey) { (Get-ItemProperty $wvKey).pv } else { $null }
Write-Output ("WebView2 Runtime: {0}" -f $(if ($wvVersion) { $wvVersion } else { '未安装（安装器会联网拉 bootstrapper）' }))

Write-Output ''
Write-Output "=== 1. 静默安装 (/S) ==="
$t0 = Get-Date
$proc = Start-Process -FilePath $installerItem.FullName -ArgumentList "/S /D=$InstallDir" -Wait -PassThru
$installSeconds = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
Step 'installer 退出码 0' ($proc.ExitCode -eq 0) ("exit={0}, 耗时 {1}s" -f $proc.ExitCode, $installSeconds)

$exe = $null
$uninstaller = $null
# NSIS 的 /S 正常是同步的；仍给 20s 容忍：安装器可能先派生子进程再返回
$exeDeadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt $exeDeadline) {
    $exe = Get-ChildItem $InstallDir -Filter '*.exe' -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -notmatch '^uninstall' } | Select-Object -First 1
    $uninstaller = Get-ChildItem $InstallDir -Filter 'uninstall*.exe' -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($exe -and $uninstaller) { break }
    Start-Sleep -Milliseconds 500
}
Step '安装目录里有主 exe' ($null -ne $exe) $(if ($exe) { $exe.FullName } else { "未在 $InstallDir 找到 exe" })
Step '安装目录里有 uninstall.exe' ($null -ne $uninstaller) $(if ($uninstaller) { $uninstaller.FullName } else { '未找到卸载器' })

$installedBytes = Get-TotalBytes $InstallDir
Step '安装后占用已测量' ($installedBytes -gt 0) ("{0:N2} MiB（{1} 字节，含 exe 与内嵌前端资源）" -f ($installedBytes / 1MB), $installedBytes)

if ($null -eq $exe) {
    Write-Output '没有主 exe，后面的启动/卸载测试没有意义，终止。'
    exit 1
}

Write-Output ''
Write-Output "=== 2. 启动 + 窗口 ==="
$runProc = Start-Process -FilePath $exe.FullName -WorkingDirectory $InstallDir -PassThru
$windowDeadline = (Get-Date).AddSeconds($WindowTimeoutSec)
$windowSeen = $false
$windowTitle = ''
$firstWindowAt = $null
$tLaunch = Get-Date
while ((Get-Date) -lt $windowDeadline) {
    Start-Sleep -Milliseconds 400
    $live = Get-Process -Id $runProc.Id -ErrorAction SilentlyContinue
    if (-not $live) { break }
    if ($live.MainWindowHandle -ne 0) {
        $windowSeen = $true
        $windowTitle = $live.MainWindowTitle
        $firstWindowAt = [math]::Round(((Get-Date) - $tLaunch).TotalSeconds, 1)
        break
    }
}

$alive = $null -ne (Get-Process -Id $runProc.Id -ErrorAction SilentlyContinue)
$memBytes = 0
if ($alive) {
    $memBytes = (Get-Process -Id $runProc.Id).WorkingSet64
}
Step '进程存活' $alive ("pid={0}" -f $runProc.Id)
if ($SkipWindowCheck) {
    # CI runner（服务账户/无交互桌面）上窗口可能不出现，但那不代表应用坏了：
    # 这一档只要求进程活着，窗口与冷启动时间留给真机跑。
    Step '窗口检查（-SkipWindowCheck 跳过）' $alive '只验证进程存活与退出码，不据此判定可视化是否正常'
} else {
    Step '窗口出现（MainWindowHandle<>0）' $windowSeen ("标题='{0}'，启动到有窗口 {1}s，工作集 {2:N1} MiB" -f $windowTitle, $firstWindowAt, ($memBytes / 1MB))
}

# 记录窗口句柄数量：Tauri 主窗口 + 可能的隐藏 webview 辅助窗口
if ($alive) {
    $allWindows = @(Get-Process -Id $runProc.Id -ErrorAction SilentlyContinue)
    Write-Output ("进程工作集: {0:N1} MiB；MainWindowTitle: '{1}'" -f ($memBytes / 1MB), $windowTitle)
}

Write-Output ''
Write-Output "=== 3. 关闭 ==="
$closed = $true
try {
    if ($alive) {
        Stop-Process -Id $runProc.Id -Force -ErrorAction Stop
        Start-Sleep -Milliseconds 800
        $closed = $null -eq (Get-Process -Id $runProc.Id -ErrorAction SilentlyContinue)
    }
} catch {
    $closed = $null -eq (Get-Process -Id $runProc.Id -ErrorAction SilentlyContinue)
}
Step '进程已退出' $closed 'Stop-Process -Force 后 800ms 复查'

if ($KeepInstalled) {
    Write-Output ''
    Write-Output '已指定 -KeepInstalled：跳过卸载与残留检查。'
} else {
    Write-Output ''
    Write-Output "=== 4. 静默卸载 (/S) ==="
    if ($null -ne $uninstaller) {
        $t1 = Get-Date
        $uproc = Start-Process -FilePath $uninstaller.FullName -ArgumentList '/S' -Wait -PassThru
        $uninstallSeconds = [math]::Round(((Get-Date) - $t1).TotalSeconds, 1)
        Step 'uninstaller 退出码 0' ($uproc.ExitCode -eq 0) ("exit={0}, 耗时 {1}s" -f $uproc.ExitCode, $uninstallSeconds)

        # NSIS 卸载器在 /S 下可能先返回、再由后台子进程删文件；轮询到目录消失
        $deadline = (Get-Date).AddSeconds(60)
        while ((Get-Date) -lt $deadline -and (Test-Path $InstallDir)) {
            Start-Sleep -Milliseconds 500
        }
        $dirGone = -not (Test-Path $InstallDir)
        $leftBytes = Get-TotalBytes $InstallDir
        $leftFiles = @(Get-ChildItem $InstallDir -Recurse -File -ErrorAction SilentlyContinue)
        Step '安装目录被删除' $dirGone ("残留 {0} 个文件 / {1:N2} MiB" -f $leftFiles.Count, ($leftBytes / 1MB))
        if (-not $dirGone -and $leftFiles.Count -gt 0) {
            Write-Output '残留文件（前 20 个）：'
            $leftFiles | Select-Object -First 20 | ForEach-Object { Write-Output ("  " + $_.FullName) }
        }

        Write-Output ''
        Write-Output "=== 5. 注册表与快捷方式残留 ==="
        $keysAfter = Get-UninstallKeys '^hbot$|hbot'
        $newKeys = @($keysAfter | Where-Object { $_.Key -notin $preExistingKeys.Key })
        Step '无新增卸载注册表项' ($newKeys.Count -eq 0) $(if ($newKeys.Count -eq 0) { 'HKCU/HKLM Uninstall 下无 hbot 键' } else { ($newKeys | ForEach-Object { "$($_.Root)\$($_.Key)" }) -join '; ' })

        $shortcutsAfter = Get-Shortcuts '^hbot'
        $newShortcuts = @($shortcutsAfter | Where-Object { $_ -notin $preExistingShortcuts })
        Step '无新增快捷方式' ($newShortcuts.Count -eq 0) $(if ($newShortcuts.Count -eq 0) { '开始菜单/桌面无 hbot.lnk' } else { $newShortcuts -join '; ' })

        if (-not $dirGone) {
            Write-Output "清理残留目录: $InstallDir"
            Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    } else {
        Step 'uninstaller 可用' $false '没找到卸载器，无法验证卸载链路'
    }
}

Write-Output ''
Write-Output '================ 汇总 ================'
$script:steps | Format-Table -AutoSize | Out-String -Width 200 | Write-Output
if ($script:failures -eq 0) {
    Write-Output "结论：全部通过（$($script:steps.Count) 项）"
    exit 0
} else {
    Write-Output "结论：$($script:failures) 项失败 / 共 $($script:steps.Count) 项"
    exit 1
}