<#
.SYNOPSIS
    冷启动分段采样：启动 hbot-desktop，采集 `HBOT_STARTUP_TRACE=1` 打出的
    `hbot-startup` 事件行，外加外部可观测的窗口出现时刻，输出每次运行的
    分段耗时与中位 / p95。

.DESCRIPTION
    只读测量脚本：不修改源码，不改预算口径。
    - 时间基准统一到「进程启动」（Process.StartTime）。Rust 侧事件带 `t_ms`
      （相对 `run()` 入口）与 `unix_ms`，脚本用 `run_enter` 行的 `unix_ms`
      把两者对齐。
    - 窗口出现 = `MainWindowHandle != 0`（与 docs/spikes/S2-tauri.md 同口径），
      25 ms 轮询。
    - 首帧 = 页面脚本在双 rAF 后写 `document.title = 'hbot-frame@<first-paint ms>'`
      （同时尝试 `plugin:event|emit`，两条路互为备份；仅 trace 开启时注入）。
    - 每次运行结束用 `taskkill /T /F` 结束进程树，避免单实例插件把下一次启动挡掉。

.PARAMETER Exe
    hbot-desktop.exe 路径（默认 release 构建）。

.PARAMETER Runs
    正式采样次数，默认 7。

.PARAMETER DataDir
    数据目录。脚本把它设为子进程的 `%APPDATA%`（`hbot_core::data_dir()` 用
    APPDATA + "hbot"），不动真实用户目录。默认 `_recovery/startup-probe-data`。

.PARAMETER Warmup
    正式采样前的预热次数（建库 + WebView2 profile），默认 1。预热不计入统计。

.PARAMETER TimeoutSec
    单次运行等待首帧标记的超时，默认 40 秒。

.PARAMETER OutDir
    每次运行的 stderr/stdout 与汇总 JSON 的输出目录。
#>
[CmdletBinding()]
param(
    [string]$Exe = '',
    [int]$Runs = 7,
    [string]$DataDir = '',
    [int]$Warmup = 1,
    [int]$TimeoutSec = 40,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
    $Exe = Join-Path $targetRoot 'release\hbot-desktop.exe'
}
if (-not $OutDir) { $OutDir = Join-Path $repo '_recovery\startup-probe' }
if (-not $DataDir) { $DataDir = Join-Path $repo '_recovery\startup-probe-data' }
if (-not (Test-Path $Exe)) { throw "找不到 hbot-desktop.exe: $Exe" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$oldAppData = $env:APPDATA
$oldTrace = $env:HBOT_STARTUP_TRACE
try {
    $env:APPDATA = $DataDir
    $env:HBOT_STARTUP_TRACE = '1'
    $exePath = (Resolve-Path $Exe).Path

    function Start-HbotOnce {
        param([string]$Tag)
        $err = Join-Path $OutDir "$Tag.stderr.log"
        $out = Join-Path $OutDir "$Tag.stdout.log"
        Remove-Item $err, $out -ErrorAction SilentlyContinue

        $proc = Start-Process -FilePath $exePath -PassThru -RedirectStandardError $err -RedirectStandardOutput $out
        $start = $proc.StartTime
        $deadline = $start.AddSeconds($TimeoutSec)
        $windowAt = $null
        $frameAt = $null
        $framePaint = $null

        while ((Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 25
            if ($proc.HasExited) { break }
            $proc.Refresh()
            if (-not $windowAt -and $proc.MainWindowHandle -ne 0) {
                $windowAt = Get-Date
            }
            $title = $proc.MainWindowTitle
            if (-not $frameAt -and $title -match '^hbot-frame@([0-9.]+)') {
                $frameAt = Get-Date
                $framePaint = [double]$Matches[1]
                break
            }
            # 事件行也可能比标题更早到（emit 通路成功时）
            if (Test-Path $err) {
                $text = Get-Content $err -Raw -ErrorAction SilentlyContinue
                if ($text -and $text.Contains('"phase":"frame"')) { break }
            }
        }

        # 给 stderr 一点收尾时间（tracing 的 info 行与 dispatch 行）
        Start-Sleep -Milliseconds 300
        if (-not $proc.HasExited) {
            & taskkill.exe /PID $proc.Id /T /F 2>&1 | Out-Null
            $null = $proc.WaitForExit(5000)
        }

        $events = @()
        if (Test-Path $err) {
            foreach ($line in Get-Content $err) {
                if ($line -match '^hbot-startup ev=') {
                    $item = [ordered]@{ raw = $line; event = $null; t_ms = $null; unix_ms = $null; dur_ms = $null; cmd = $null; payload = $null; detail = $line }
                    if ($line -match 'ev=(\S+)') { $item.event = $Matches[1] }
                    if ($line -match 't_ms=([0-9.]+)') { $item.t_ms = [double]$Matches[1] }
                    if ($line -match 'unix_ms=(\d+)') { $item.unix_ms = [long]$Matches[1] }
                    if ($line -match 'dur_ms=([0-9.]+)') { $item.dur_ms = [double]$Matches[1] }
                    if ($line -match 'cmd=(\S+)') { $item.cmd = $Matches[1] }
                    if ($line -match 'payload=(\{.+\})\s*$') { $item.payload = $Matches[1] }
                    $events += [pscustomobject]$item
                }
            }
        }

        [pscustomobject]@{
            tag        = $Tag
            start      = $start
            windowAt   = $windowAt
            frameAt    = $frameAt
            framePaint = $framePaint
            events     = $events
            errLog     = $err
        }
    }

    function Get-EventValue {
        param($Run, [string]$Name, [string]$Field = 't_ms')
        $ev = $Run.events | Where-Object { $_.event -eq $Name } | Select-Object -First 1
        if (-not $ev) { return $null }
        return $ev.$Field
    }

    function Get-StorePhaseStart {
        param($Run, [string]$Name, [long]$RunEnterUnix)
        $ev = $Run.events | Where-Object { $_.event -eq $Name } | Select-Object -First 1
        if (-not $ev -or $null -eq $ev.unix_ms -or $null -eq $ev.dur_ms) { return $null }
        return [double]($ev.unix_ms - $RunEnterUnix - $ev.dur_ms)
    }

    function Measure-Run {
        param($Run)
        $runEnter = $Run.events | Where-Object { $_.event -eq 'run_enter' } | Select-Object -First 1
        $runEnterUnix = if ($runEnter) { [long]$runEnter.unix_ms } else { $null }
        $processToRun = if ($runEnterUnix) {
            $startUnix = ([DateTimeOffset]$Run.start).ToUnixTimeMilliseconds()
            [double]($runEnterUnix - $startUnix)
        } else { $null }

        $toProcess = {
            param($t)
            if ($null -eq $t -or $null -eq $processToRun) { return $null }
            return [double]$t + $processToRun
        }

        $setupEnter = & $toProcess (Get-EventValue $Run 'setup_enter')
        $setupExit = & $toProcess (Get-EventValue $Run 'setup_exit')
        $openBegin = & $toProcess (Get-EventValue $Run 'open_blocking_enter')
        $coreReady = & $toProcess (Get-EventValue $Run 'core_ready')
        $pageLoaded = & $toProcess (Get-EventValue $Run 'page_load_finished')

        $d1 = $Run.events | Where-Object { $_.event -eq 'dispatch_in' -and $_.detail -match 'n=1\s' } | Select-Object -First 1
        $d1out = $Run.events | Where-Object { $_.event -eq 'dispatch_out' -and $_.detail -match 'n=1\s' } | Select-Object -First 1
        $dispatch1In = if ($d1) { & $toProcess $d1.t_ms } else { $null }
        $dispatch1Out = if ($d1out) { & $toProcess $d1out.t_ms } else { $null }

        $window = if ($Run.windowAt) { [double]($Run.windowAt - $Run.start).TotalMilliseconds } else { $null }

        $frontendMark = $Run.events | Where-Object { $_.event -eq 'frontend_mark' -and $_.payload -match '"phase":"frame"' } | Select-Object -Last 1
        if (-not $frontendMark) {
            $frontendMark = $Run.events | Where-Object { $_.event -eq 'frontend_mark' } | Select-Object -Last 1
        }
        $firstPaint = $null
        $fcp = $null
        if ($frontendMark -and $frontendMark.payload) {
            try {
                $p = $frontendMark.payload | ConvertFrom-Json
                $pageOrigin = [double]$p.time_origin - $runEnterUnix
                if ($null -ne $p.first_paint) { $firstPaint = & $toProcess ([double]$pageOrigin + [double]$p.first_paint) }
                if ($null -ne $p.fcp) { $fcp = & $toProcess ([double]$pageOrigin + [double]$p.fcp) }
            } catch { }
        }
        if ($null -eq $firstPaint -and $null -ne $Run.framePaint) {
            # 标题里是 first-paint 的 epoch 毫秒（timeOrigin + 相对毫秒）。
            $startUnix = ([DateTimeOffset]$Run.start).ToUnixTimeMilliseconds()
            $firstPaint = [double]$Run.framePaint - $startUnix
        }

        $row = [ordered]@{
            tag                = $Run.tag
            process_to_run_ms  = if ($processToRun) { [math]::Round($processToRun, 1) } else { $null }
            window_ms          = if ($window) { [math]::Round($window, 1) } else { $null }
            setup_enter_ms     = if ($setupEnter) { [math]::Round($setupEnter, 1) } else { $null }
            setup_exit_ms      = if ($setupExit) { [math]::Round($setupExit, 1) } else { $null }
            open_begin_ms      = if ($openBegin) { [math]::Round($openBegin, 1) } else { $null }
            core_ready_ms      = if ($coreReady) { [math]::Round($coreReady, 1) } else { $null }
            page_loaded_ms     = if ($pageLoaded) { [math]::Round($pageLoaded, 1) } else { $null }
            first_rpc_in_ms    = if ($dispatch1In) { [math]::Round($dispatch1In, 1) } else { $null }
            first_rpc_out_ms   = if ($dispatch1Out) { [math]::Round($dispatch1Out, 1) } else { $null }
            first_paint_ms     = if ($firstPaint) { [math]::Round($firstPaint, 1) } else { $null }
            fcp_ms             = if ($fcp) { [math]::Round($fcp, 1) } else { $null }
            s1_process_setup   = if ($setupEnter) { [math]::Round($setupEnter, 1) } else { $null }
            s2_setup_open      = if ($setupEnter -and $openBegin) { [math]::Round($openBegin - $setupEnter, 1) } else { $null }
            s3_open_total      = Get-EventValue $Run 'appstate_open_total' 'dur_ms'
            s4_open_window     = if ($coreReady -and $window) { [math]::Round($window - $coreReady, 1) } else { $null }
            s5_window_rpc      = if ($window -and $dispatch1Out) { [math]::Round($dispatch1Out - $window, 1) } else { $null }
            s6_window_frame    = if ($window -and $firstPaint) { [math]::Round($firstPaint - $window, 1) } else { $null }
        }

        # 库内阶段（从 unix_ms/dur_ms 换算到「进程启动」基准）
        foreach ($phase in @('store_open_total','store_migrations','store_writer_thread','app_create_data_dir','app_store_open','app_agent_manager','appstate_open_total')) {
            $startRel = Get-StorePhaseStart $Run $phase $runEnterUnix
            $ev = $Run.events | Where-Object { $_.event -eq $phase } | Select-Object -First 1
            $row["phase_$phase"] = if ($null -ne $startRel -and $ev) { [math]::Round($startRel + $processToRun, 1) } else { $null }
            $row["dur_$phase"] = if ($ev) { [math]::Round($ev.dur_ms, 1) } else { $null }
        }
        # 每条连接的三段 PRAGMA 开销（detail 里带 role=writer/read0/read1）
        foreach ($connEvent in @('store_conn_open','store_conn_wal','store_conn_pragmas')) {
            foreach ($role in @('writer','read0','read1')) {
                $ev = $Run.events | Where-Object { $_.event -eq $connEvent -and $_.detail -match "role=$role" } | Select-Object -First 1
                $row["dur_${connEvent}_$role"] = if ($ev) { [math]::Round($ev.dur_ms, 1) } else { $null }
            }
        }

        [pscustomobject]$row
    }
    function Get-Percentile {
        param([double[]]$Values, [double]$P)
        $v = $Values | Where-Object { $null -ne $_ } | Sort-Object
        if ($v.Count -eq 0) { return $null }
        $idx = [math]::Ceiling($P / 100.0 * $v.Count) - 1
        if ($idx -lt 0) { $idx = 0 }
        if ($idx -ge $v.Count) { $idx = $v.Count - 1 }
        return [math]::Round($v[$idx], 1)
    }

    $warmupRuns = @()
    for ($i = 1; $i -le $Warmup; $i += 1) {
        Write-Host "预热 $i/$Warmup ..."
        $warmupRuns += Start-HbotOnce -Tag ("warmup-{0:d2}" -f $i)
    }

    $sampleRuns = @()
    for ($i = 1; $i -le $Runs; $i += 1) {
        Write-Host "采样 $i/$Runs ..."
        $sampleRuns += Start-HbotOnce -Tag ("run-{0:d2}" -f $i)
    }

    $rows = @($sampleRuns | ForEach-Object { Measure-Run $_ })

    $metrics = @(
        'window_ms','setup_enter_ms','open_begin_ms','core_ready_ms','first_rpc_in_ms','first_rpc_out_ms',
        'first_paint_ms','fcp_ms','s3_open_total','s4_open_window','s5_window_rpc','s6_window_frame',
        'dur_store_open_total','dur_store_migrations','dur_store_writer_thread','dur_app_create_data_dir',
        'dur_app_agent_manager','dur_appstate_open_total',
        'dur_store_conn_open_writer','dur_store_conn_wal_writer','dur_store_conn_pragmas_writer',
        'dur_store_conn_open_read0','dur_store_conn_wal_read0','dur_store_conn_pragmas_read0',
        'dur_store_conn_open_read1','dur_store_conn_wal_read1','dur_store_conn_pragmas_read1'
    )
    $summary = [ordered]@{}
    foreach ($m in $metrics) {
        $values = @($rows | ForEach-Object { $_.$m } | Where-Object { $null -ne $_ })
        if ($values.Count -gt 0) {
            $summary[$m] = [ordered]@{
                median = Get-Percentile $values 50
                p95    = Get-Percentile $values 95
                min    = [math]::Round(($values | Measure-Object -Minimum).Minimum, 1)
                max    = [math]::Round(($values | Measure-Object -Maximum).Maximum, 1)
                n      = $values.Count
            }
        }
    }

    $result = [ordered]@{
        generated_at = (Get-Date).ToString('o')
        exe          = $exePath
        exe_bytes    = (Get-Item $exePath).Length
        data_dir     = $DataDir
        runs         = $rows
        warmups      = @($warmupRuns | ForEach-Object { $_.tag })
        summary      = $summary
    }
    $jsonPath = Join-Path $OutDir 'startup-probe-result.json'
    $result | ConvertTo-Json -Depth 8 | Set-Content -Encoding utf8 $jsonPath

    Write-Host ''
    Write-Host '== 每次运行（ms，相对进程启动） =='
    $rows | Format-Table tag, window_ms, setup_enter_ms, open_begin_ms, core_ready_ms, first_rpc_out_ms, first_paint_ms, s3_open_total, s5_window_rpc, s6_window_frame -AutoSize | Out-String | Write-Host

    Write-Host '== 汇总（中位 / p95） =='
    foreach ($m in ($summary.Keys)) {
        $s = $summary[$m]
        Write-Host ("{0,-26} median={1,8}  p95={2,8}  min={3,8}  max={4,8}  n={5}" -f $m, $s.median, $s.p95, $s.min, $s.max, $s.n)
    }
    Write-Host ''
    Write-Host "JSON: $jsonPath"
}
finally {
    $env:APPDATA = $oldAppData
    $env:HBOT_STARTUP_TRACE = $oldTrace
}


