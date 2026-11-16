<#
.SYNOPSIS
    headless `hbot serve`「到 ready」分段采样：量「进程创建 → stdout 打出 ready 行」，
    并（在 `-Trace` 模式下）用 `HBOT_STARTUP_TRACE=1` 的事件行把这段拆成分段。

.DESCRIPTION
    只读测量脚本：不修改源码，不改预算口径。

    ## 口径（与 docs/spikes/S2-tauri.md 的桌面探针统一）
    - 起点 = **进程创建时刻**（`System.Diagnostics.Process.StartTime`，即内核记的
      creation time，含 exe 映像加载、CRT 初始化）。
    - 终点 = **ready 行的字节已经写进管道**（`{"type":"ready",...}`）。
      进程内埋点在 flush 之后打一条 `ev=ready_written`，它的 `unix_ms` 是主口径；
      父进程侧另有 `ReadLineAsync` 观测值做交叉校验（含 ~1–2 ms 的 `Process.Start()`
      调用与 PowerShell 唤醒开销，**只用来对齐，不用来定案**）。

    ## 两种模式
    - `-Trace`：设 `HBOT_STARTUP_TRACE=1`，采集分段事件（约 30 行 stderr）。
      埋点本身有开销（每行一次写管道），因此**分段表用这一档**，
      但「到 ready 总时长」以 `-Plain` 档为准。
    - `-Plain`：不设任何环境变量，只有父进程侧观测，量真实的到 ready 总时长。

    ## 数据目录
    用 `--data-dir <临时目录>` 指到 `_recovery/` 下，**不动用户 `%APPDATA%\hbot`**。
    默认 `-Warmup 1`：第一次跑建库/写迁移，不计入统计；正式采样跑在「已有数据」的库上
    （与 ADR-0002 §4 的「非首次建库」口径一致）。`-Cold` 则每次前删库，量首次建库。

.EXAMPLE
    pwsh scripts/headless-startup-probe.ps1 -Trace -Runs 7
    pwsh scripts/headless-startup-probe.ps1 -Plain -Runs 7
#>
[CmdletBinding()]
param(
    # hbot.exe 路径；默认从 CARGO_TARGET_DIR（或 target）解析 release 产物。
    [string]$Exe = '',
    # 正式采样次数。
    [int]$Runs = 7,
    # 预热次数（不计入统计）。
    [int]$Warmup = 1,
    # 每次运行前删库，量「首次建库」路径。
    [switch]$Cold,
    # 打开 HBOT_STARTUP_TRACE，采集分段。
    [switch]$Trace,
    # 显式选「不埋点」档（默认就是它；写出来是为了让命令行自解释）。
    [switch]$Plain,
    # 单次等待 ready 的超时（毫秒）。
    [int]$TimeoutMs = 30000,
    # 数据目录（默认 _recovery/headless-startup-data）。
    [string]$DataDir = '',
    # 输出目录（默认 _recovery/headless-startup）。
    [string]$OutDir = '',
    # 结果文件前缀，便于区分改动前后。
    [string]$Tag = '',
    # 额外传给 hbot 的参数（默认只有 serve --data-dir）。
    [string[]]$ExtraArgs = @()
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

if (-not $Exe) {
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
    $Exe = Join-Path $targetRoot 'release\hbot.exe'
}
if (-not $OutDir) { $OutDir = Join-Path $repo '_recovery\headless-startup' }
if (-not $DataDir) { $DataDir = Join-Path $repo '_recovery\headless-startup-data' }
if (-not (Test-Path $Exe)) { throw "找不到 hbot.exe: $Exe（先 cargo build --release -p hbot-cli）" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$exePath = (Resolve-Path $Exe).Path
if (-not $Tag) { $Tag = if ($Trace) { 'trace' } else { 'plain' } }

$mode = if ($Trace) { 'trace' } else { 'plain' }
$oldTrace = $env:HBOT_STARTUP_TRACE

# ---------------------------------------------------------------- 单次运行
function Invoke-HbotOnce {
    param([string]$RunTag)

    if ($Cold) {
        Remove-Item (Join-Path $DataDir 'hbot.db*') -ErrorAction SilentlyContinue
    }

    $argList = @('serve', '--data-dir', $DataDir) + $ExtraArgs
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $exePath
    foreach ($a in $argList) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardInput = $false
    $psi.CreateNoWindow = $true
    $psi.WorkingDirectory = $repo
    if ($Trace) {
        $psi.EnvironmentVariables['HBOT_STARTUP_TRACE'] = '1'
    } else {
        $null = $psi.EnvironmentVariables.Remove('HBOT_STARTUP_TRACE')
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = [System.Diagnostics.Process]::Start($psi)
    $startTime = $proc.StartTime
    $startUnixMs = ([DateTimeOffset]$startTime).ToUnixTimeMilliseconds()

    $readTask = $proc.StandardOutput.ReadLineAsync()
    $readyObservedMs = $null
    $readyLine = $null
    $timedOut = $false
    if ([System.Threading.Tasks.Task]::WaitAny(@($readTask), $TimeoutMs) -eq 0) {
        $readyObservedMs = $sw.Elapsed.TotalMilliseconds
        $readyLine = $readTask.Result
    } else {
        $timedOut = $true
    }
    $sw.Stop()

    # 收尾：整棵进程树杀掉（serve 会挂着 accept 任务）。
    try {
        & taskkill.exe /PID $proc.Id /T /F 2>&1 | Out-Null
        $null = $proc.WaitForExit(5000)
    } catch { }
    $stderrText = ''
    try { $stderrText = $proc.StandardError.ReadToEnd() } catch { }
    $proc.Dispose()

    # 解析 HBOT_STARTUP_TRACE 事件行。
    $events = [ordered]@{}
    $rawEvents = New-Object System.Collections.Generic.List[string]
    foreach ($line in ($stderrText -split "`r?`n")) {
        if ($line -notmatch '^hbot-startup ev=') { continue }
        $rawEvents.Add($line)
        $ev = @{ event = $null; unix_ms = $null; dur_ms = $null; detail = $line }
        if ($line -match 'ev=(\S+)') { $ev.event = $Matches[1] }
        if ($line -match 'unix_ms=(\d+)') { $ev.unix_ms = [long]$Matches[1] }
        if ($line -match 'dur_ms=([0-9.]+)') { $ev.dur_ms = [double]$Matches[1] }
        # 同名事件可能多次出现（store_conn_* 有 role=writer/read0/read1）；保留第一条，
        # 带 role 的另存 key。
        $key = $ev.event
        if ($line -match 'role=(\w+)') { $key = "$($ev.event)@$($Matches[1])" }
        if (-not $events.Contains($key)) { $events[$key] = $ev }
    }

    $logBase = Join-Path $OutDir "$RunTag"
    Set-Content -Path "$logBase.stderr.log" -Value $stderrText -Encoding utf8
    if ($readyLine) { Set-Content -Path "$logBase.stdout.log" -Value $readyLine -Encoding utf8 }

    [pscustomobject]@{
        tag             = $RunTag
        mode            = $mode
        start_unix_ms   = $startUnixMs
        ready_observed  = if ($readyObservedMs) { [math]::Round($readyObservedMs, 1) } else { $null }
        ready_line      = $readyLine
        timed_out       = $timedOut
        events          = $events
        raw_events      = $rawEvents
        stderr_log      = "$logBase.stderr.log"
    }
}

# ------------------------------------------------------- 分段口径（事件 → 毫秒）
# 时间基准：全部换算成「相对进程创建」的毫秒数。
# 点事件：t = unix_ms - start_unix_ms
# 段事件：起 = unix_ms - dur_ms - start_unix_ms，止 = unix_ms - start_unix_ms
function Get-EventEnd {
    param($Run, [string]$Key)
    $ev = $Run.events[$Key]
    if (-not $ev -or $null -eq $ev.unix_ms) { return $null }
    return [double]($ev.unix_ms - $Run.start_unix_ms)
}
function Get-EventStart {
    param($Run, [string]$Key)
    $ev = $Run.events[$Key]
    if (-not $ev -or $null -eq $ev.unix_ms -or $null -eq $ev.dur_ms) { return $null }
    return [double]($ev.unix_ms - $Run.start_unix_ms - $ev.dur_ms)
}
function Get-Dur {
    param($Run, [string]$Key)
    $ev = $Run.events[$Key]
    if (-not $ev -or $null -eq $ev.dur_ms) { return $null }
    return [double]$ev.dur_ms
}
function Get-Gap {
    param($Run, [string]$From, [string]$To)
    $a = Get-EventEnd $Run $From
    $b = Get-EventEnd $Run $To
    if ($null -eq $a -or $null -eq $b) { return $null }
    return [math]::Round($b - $a, 1)
}

function Measure-Run {
    param($Run)

    # 到 ready 的总时长：主口径取进程内 `ready_written`（flush 返回之后的时刻）。
    $procToReady = Get-EventEnd $Run 'ready_written'
    if ($null -eq $procToReady) {
        # 没有埋点（-Plain 档）：用父进程观测值兜底。
        $procToReady = $Run.ready_observed
    }

    $row = [ordered]@{
        tag                    = $Run.tag
        mode                   = $Run.mode
        # ---- 总时长 ----
        proc_to_ready_ms       = if ($null -ne $procToReady) { [math]::Round($procToReady, 1) } else { $null }
        ready_observed_ms      = $Run.ready_observed
        # ---- 前段：进程创建 → main ----
        proc_to_main_ms        = Get-EventEnd $Run 'main_enter'
        # ---- 各相邻分段 ----
        main_to_cli_ms         = Get-Gap $Run 'main_enter' 'cli_parsed'
        cli_to_tracing_ms      = Get-Gap $Run 'cli_parsed' 'tracing_ready'
        tracing_to_rt_ms       = Get-Gap $Run 'tracing_ready' 'rt_built'
        rt_to_run_ms           = Get-Gap $Run 'rt_built' 'run_enter'
        run_to_serve_ms        = Get-Gap $Run 'run_enter' 'serve_enter'
        serve_to_plan_ms       = Get-Gap $Run 'serve_enter' 'plan_ready'
        plan_to_appstate_ms    = Get-Gap $Run 'plan_ready' 'appstate_begin'
        appstate_to_ready_ms   = Get-Gap $Run 'appstate_end' 'ready_written'
        # ---- AppState::open 内部 ----
        appstate_open_dur      = Get-Dur $Run 'appstate_open_total'
        app_create_data_dir    = Get-Dur $Run 'app_create_data_dir'
        app_store_open_dur     = Get-Dur $Run 'app_store_open'
        app_agent_manager_dur  = Get-Dur $Run 'app_agent_manager'
        # ---- Store::open 内部 ----
        store_open_total_dur   = Get-Dur $Run 'store_open_total'
        store_migrations_dur   = Get-Dur $Run 'store_migrations'
        store_writer_thread    = Get-Dur $Run 'store_writer_thread'
        mig_schema_table       = Get-Dur $Run 'mig_schema_table'
        mig_read_applied       = Get-Dur $Run 'mig_read_applied'
        mig_user_version       = Get-Dur $Run 'mig_user_version'
        mig_apply_one          = Get-Dur $Run 'mig_apply_one'
        mig_user_version_fixup = Get-Dur $Run 'mig_user_version_fixup'
        conn_open_writer       = Get-Dur $Run 'store_conn_open@writer'
        conn_wal_writer        = Get-Dur $Run 'store_conn_wal@writer'
        conn_pragmas_writer    = Get-Dur $Run 'store_conn_pragmas@writer'
        conn_open_read0        = Get-Dur $Run 'store_conn_open@read0'
        conn_wal_read0         = Get-Dur $Run 'store_conn_wal@read0'
        conn_pragmas_read0     = Get-Dur $Run 'store_conn_pragmas@read0'
        conn_open_read1        = Get-Dur $Run 'store_conn_open@read1'
        conn_wal_read1         = Get-Dur $Run 'store_conn_wal@read1'
        conn_pragmas_read1     = Get-Dur $Run 'store_conn_pragmas@read1'
        # ---- ready 之前的装配 ----
        dispatcher_dur         = Get-Dur $Run 'serve_dispatcher'
        ws_bind_dur            = Get-Dur $Run 'serve_ws_bind'
        ready_write_dur        = Get-Dur $Run 'serve_ready_write'
        # ---- 派生：三段大头（互不重叠） ----
        # 进程→main；main→appstate_begin（参数解析+运行时+装配）；appstate_begin→ready。
        seg_pre_main           = Get-EventEnd $Run 'main_enter'
        seg_main_to_appstate   = if ((Get-EventEnd $Run 'appstate_begin')) { Get-EventEnd $Run 'appstate_begin' } else { $null }
        seg_appstate_to_ready  = Get-Gap $Run 'appstate_begin' 'ready_written'
    }
    [pscustomobject]$row
}

function Get-Percentile {
    param([double[]]$Values, [double]$P)
    $v = @($Values | Where-Object { $null -ne $_ } | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    $idx = [math]::Ceiling($P / 100.0 * $v.Count) - 1
    if ($idx -lt 0) { $idx = 0 }
    if ($idx -ge $v.Count) { $idx = $v.Count - 1 }
    return [math]::Round($v[$idx], 1)
}

# ---------------------------------------------------------------- 主流程
try {
    if ($Trace) { $env:HBOT_STARTUP_TRACE = '1' } else { Remove-Item Env:\HBOT_STARTUP_TRACE -ErrorAction SilentlyContinue }

    $cpu = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
    $load = $null
    try { $load = [math]::Round((Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average, 0) } catch { }

    for ($i = 1; $i -le $Warmup; $i += 1) {
        Write-Host "预热 $i/$Warmup ..."
        $null = Invoke-HbotOnce -RunTag ("{0}-warmup-{1:d2}" -f $Tag, $i)
    }

    $sampleRuns = @()
    for ($i = 1; $i -le $Runs; $i += 1) {
        Write-Host "采样 $i/$Runs ($mode) ..."
        $run = Invoke-HbotOnce -RunTag ("{0}-run-{1:d2}" -f $Tag, $i)
        if (-not $run.ready_line) {
            Write-Warning "run $i 没拿到 ready 行（timed_out=$($run.timed_out)），见 $($run.stderr_log)"
        }
        $sampleRuns += $run
    }

    $rows = @($sampleRuns | ForEach-Object { Measure-Run $_ })

    $metrics = @(
        'proc_to_ready_ms', 'proc_to_main_ms',
        'main_to_cli_ms', 'cli_to_tracing_ms', 'tracing_to_rt_ms', 'rt_to_run_ms',
        'run_to_serve_ms', 'serve_to_plan_ms', 'plan_to_appstate_ms', 'appstate_to_ready_ms',
        'appstate_open_dur', 'app_create_data_dir', 'app_store_open_dur', 'app_agent_manager_dur',
        'store_open_total_dur', 'store_migrations_dur', 'store_writer_thread',
        'mig_schema_table', 'mig_read_applied', 'mig_user_version', 'mig_apply_one', 'mig_user_version_fixup',
        'conn_open_writer', 'conn_wal_writer', 'conn_pragmas_writer',
        'conn_open_read0', 'conn_wal_read0', 'conn_pragmas_read0',
        'conn_open_read1', 'conn_wal_read1', 'conn_pragmas_read1',
        'dispatcher_dur', 'ws_bind_dur', 'ready_write_dur'
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
        tag          = $Tag
        mode         = $mode
        exe          = $exePath
        exe_bytes    = (Get-Item $exePath).Length
        data_dir     = $DataDir
        cold         = [bool]$Cold
        runs         = $Runs
        logical_cpus = $cpu
        cpu_load_pct_medianish = $load
        rows         = $rows
        summary      = $summary
        raw_events   = @($sampleRuns | ForEach-Object { [ordered]@{ tag = $_.tag; events = $_.raw_events } })
    }
    $jsonPath = Join-Path $OutDir "$Tag-result.json"
    $result | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 $jsonPath

    Write-Host ''
    Write-Host "== 每次运行（ms，相对进程创建） tag=$Tag mode=$mode =="
    $rows | Format-Table tag, proc_to_ready_ms, ready_observed_ms, proc_to_main_ms, main_to_cli_ms, tracing_to_rt_ms, rt_to_run_ms, appstate_open_dur, store_open_total_dur, store_migrations_dur, ws_bind_dur -AutoSize | Out-String -Width 240 | Write-Host
    Write-Host '== 汇总（中位 / p95 / min / max） =='
    foreach ($m in ($summary.Keys)) {
        $s = $summary[$m]
        Write-Host ("{0,-24} median={1,8}  p95={2,8}  min={3,8}  max={4,8}  n={5}" -f $m, $s.median, $s.p95, $s.min, $s.max, $s.n)
    }
    Write-Host ''
    Write-Host "JSON: $jsonPath"
}
finally {
    $env:HBOT_STARTUP_TRACE = $oldTrace
}