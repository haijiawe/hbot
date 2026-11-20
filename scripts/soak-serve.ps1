<#
.SYNOPSIS
长时稳定性压测：起一个真的 hbot serve，用 WS 持续注入消息，按固定间隔采样内存与库体积。
.DESCRIPTION
六条判据（阈值与 docs/plans/2026-11-21-desktop-acceptance.md 的 3.4 节一致）：
  1. 内存：每 10 分钟线性回归斜率 < 10 MB，且峰值 < 首采 + 250 MB；
  2. 延迟：注入到 run.log 的 p95 < 2 s，最大 < 10 s；
  3. 错误：没有 error 级 run.log，客户端也没有协议错误；
  4. 库体积：hbot.db 增长 < 200 MB；
  5. WAL：hbot.db-wal 峰值 < 100 MB（不膨胀）；
  6. 收尾：强杀后 5 秒内进程消失。

第 6 条为什么是强杀而不是优雅退出：crates/hbot-cli/src/serve.rs 的 wait_for_shutdown() 在
Windows 上只等 Ctrl-C（SIGTERM 分支是 cfg(unix)），脚本无法给独立进程发 Ctrl-C。优雅路径由
crates/hbot-rpc/tests/lifecycle_capability.rs 里那个把 shutdown 交给 onStop 的用例覆盖，
本脚本不重复。
.EXAMPLE
  pwsh -File scripts/soak-serve.ps1 -Minutes 5
  pwsh -File scripts/soak-serve.ps1 -Minutes 120 -Bin E:/hbot-target/desk/release/hbot.exe
#>
[CmdletBinding()]
param(
    [int]$Minutes = 120,
    [int]$IntervalSec = 5,
    [int]$SampleSec = 30,
    [string]$DataDir = "",
    [string]$Bin = "",
    [string]$Tag = ""
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $Tag) { $Tag = "soak-" + (Get-Date -Format "yyyyMMdd-HHmmss") }
if (-not $DataDir) { $DataDir = Join-Path $repo "_recovery/$Tag" }
$DataDir = [System.IO.Path]::GetFullPath($DataDir)
if (-not $Bin) {
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo "target" }
    $Bin = Join-Path $targetRoot "release/hbot.exe"
}
if (-not (Test-Path $Bin)) {
    Write-Host "[soak] 找不到 $Bin，先跑 cargo build --release -p hbot-cli" -ForegroundColor Red
    exit 2
}

$projectDir = Join-Path $DataDir "project"
$serveData = Join-Path $DataDir "data"
$logs = Join-Path $DataDir "logs"
foreach ($dir in @($projectDir, (Join-Path $projectDir "src"), $serveData, $logs)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}
$botId = "88888888-8888-4888-8888-888888888888"
$token = [guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N")
$utf8 = New-Object System.Text.UTF8Encoding $false
$handler = "export default {" + [Environment]::NewLine + "  async onMessage(ctx) {" + [Environment]::NewLine + "    await ctx.reply('echo ' + ctx.text);" + [Environment]::NewLine + "  }," + [Environment]::NewLine + "};"
[System.IO.File]::WriteAllText((Join-Path $projectDir "src/main.ts"), $handler, $utf8)
$manifest = '{"name":"soak","platform":{"kind":"local"}}'
[System.IO.File]::WriteAllText((Join-Path $projectDir "hbot.json"), $manifest, $utf8)
$bots = '{"bots":[{"id":"' + $botId + '","platform":"local","name":"soak"}]}'
[System.IO.File]::WriteAllText((Join-Path $projectDir "bots.json"), $bots, $utf8)

$stdout = Join-Path $logs "serve.out.log"
$stderr = Join-Path $logs "serve.err.log"
$clientOut = Join-Path $logs "client.out.log"
$clientErr = Join-Path $logs "client.err.log"
$resultJson = Join-Path $logs "client.json"
$samplesCsv = Join-Path $logs "samples.csv"

Write-Host "[soak] bin     = $Bin"
Write-Host "[soak] data    = $DataDir"
Write-Host "[soak] minutes = $Minutes / interval = $IntervalSec s / sample = $SampleSec s"

$serverArgs = @("serve", "--bind", "127.0.0.1:0", "--data-dir", $serveData, "--token", $token)
$server = Start-Process -FilePath $Bin -ArgumentList $serverArgs -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr

$address = ""
$serverPid = 0
$deadline = (Get-Date).AddSeconds(60)
# 就绪行是 serde 按字段名字母序排出来的：{"addr":…,"pid":…,"type":"ready"}。
# 别写成前缀匹配 '{"type":"ready"*')，type 不在第一个。
while (-not $address -and (Get-Date) -lt $deadline) {
    if (Test-Path $stdout) {
        foreach ($line in (Get-Content $stdout -ErrorAction SilentlyContinue)) {
            if ($line -like '*"type":"ready"*') {
                $ready = $line | ConvertFrom-Json
                $address = "ws://" + $ready.addr
                $serverPid = [int]$ready.pid
                break
            }
        }
    }
    if (-not $address) { Start-Sleep -Milliseconds 200 }
}
if (-not $address) {
    Write-Host "[soak] 服务没有在 60 秒内就绪，见 $stderr" -ForegroundColor Red
    if (-not $server.HasExited) { Stop-Process -Id $server.Id -Force }
    exit 3
}
if ($serverPid -eq 0) { $serverPid = $server.Id }
Write-Host "[soak] ready   = $address (pid $serverPid)"

$clientArgs = @((Join-Path $repo "scripts/soak-client.mjs"), "--address", $address, "--token", $token, "--project", $projectDir, "--bot", $botId, "--minutes", "$Minutes", "--interval", "$IntervalSec", "--out", $resultJson, "--marker", "soak")
$client = Start-Process -FilePath "node" -ArgumentList $clientArgs -PassThru -WindowStyle Hidden -RedirectStandardOutput $clientOut -RedirectStandardError $clientErr
Write-Host "[soak] client  = pid $($client.Id)"

"at,elapsedMin,privateMb,dbBytes,walBytes" | Out-File -FilePath $samplesCsv -Encoding utf8
$dbPath = Join-Path $serveData "hbot.db"
$walPath = Join-Path $serveData "hbot.db-wal"
$started = Get-Date
$sampleCount = 0
$firstMb = $null
$peakMb = $null
$peakWal = [int64]0
$firstDb = [int64]0
$lastDb = [int64]0
$regressionMbPer10Min = 0.0

function Get-Size([string]$path) {
    if (Test-Path $path) { return (Get-Item $path).Length }
    return [int64]0
}

while (-not $client.HasExited) {
    $now = Get-Date
    $elapsed = ($now - $started).TotalMinutes
    $privateMb = $null
    $proc = Get-Process -Id $serverPid -ErrorAction SilentlyContinue
    if ($proc) { $privateMb = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1) }
    $db = Get-Size $dbPath
    $wal = Get-Size $walPath
    if ($null -ne $privateMb) {
        if ($null -eq $firstMb) { $firstMb = $privateMb; $firstDb = $db }
        if ($null -eq $peakMb -or $privateMb -gt $peakMb) { $peakMb = $privateMb }
    }
    if ($wal -gt $peakWal) { $peakWal = $wal }
    if ($db -gt $lastDb) { $lastDb = $db }
    $sampleCount += 1
    $row = "{0:o},{1:N2},{2},{3},{4}" -f $now, $elapsed, $privateMb, $db, $wal
    $row | Out-File -FilePath $samplesCsv -Append -Encoding utf8
    Write-Host ("[soak] {0,6:N1} min  rss {1,7} MB  db {2,7:N1} MB  wal {3,6:N1} MB" -f $elapsed, $privateMb, ($db / 1MB), ($wal / 1MB))
    $client.WaitForExit($SampleSec * 1000) | Out-Null
}

$clientExit = $client.ExitCode
Write-Host "[soak] 客户端退出码 $clientExit"

$stopBegan = Get-Date
if (-not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
$stoppedAfterMs = -1
while (((Get-Date) - $stopBegan).TotalSeconds -lt 10) {
    if (-not (Get-Process -Id $serverPid -ErrorAction SilentlyContinue)) {
        $stoppedAfterMs = [int](((Get-Date) - $stopBegan).TotalMilliseconds)
        break
    }
    Start-Sleep -Milliseconds 100
}
Write-Host "[soak] 进程消失耗时 $stoppedAfterMs ms"

$rows = @(Get-Content $samplesCsv | Select-Object -Skip 1 | ForEach-Object {
    $parts = $_ -split ","
    if ($parts.Count -ge 3 -and $parts[2] -ne "") { [pscustomobject]@{ x = [double]$parts[1]; y = [double]$parts[2] } }
})
if ($rows.Count -ge 3) {
    $meanX = ($rows | Measure-Object -Property x -Average).Average
    $meanY = ($rows | Measure-Object -Property y -Average).Average
    $num = 0.0
    $den = 0.0
    foreach ($row in $rows) {
        $num += ($row.x - $meanX) * ($row.y - $meanY)
        $den += ($row.x - $meanX) * ($row.x - $meanX)
    }
    if ($den -gt 0) { $regressionMbPer10Min = [math]::Round($num / $den * 10, 2) }
}

$summary = Get-Content $resultJson -Raw | ConvertFrom-Json
$dbGrowthMb = [math]::Round(($lastDb - $firstDb) / 1MB, 1)
$peakWalMb = [math]::Round($peakWal / 1MB, 1)

$checks = @()
$checks += [pscustomobject]@{ name = "memory-regression"; pass = ($regressionMbPer10Min -lt 10); detail = "$regressionMbPer10Min MB / 10 min" }
$checks += [pscustomobject]@{ name = "memory-peak"; pass = ($null -ne $peakMb -and ($peakMb - $firstMb) -lt 250); detail = "first $firstMb MB, peak $peakMb MB" }
$checks += [pscustomobject]@{ name = "latency-p95"; pass = ($null -ne $summary.p95 -and $summary.p95 -lt 2000); detail = "p95 $($summary.p95) ms, max $($summary.max) ms" }
$checks += [pscustomobject]@{ name = "latency-max"; pass = ($null -ne $summary.max -and $summary.max -lt 10000); detail = "max $($summary.max) ms" }
$checks += [pscustomobject]@{ name = "reply-rate"; pass = ($summary.sent -gt 0 -and $summary.replied -ge [math]::Floor($summary.sent * 0.95)); detail = "$($summary.replied)/$($summary.sent)" }
$checks += [pscustomobject]@{ name = "error-lines"; pass = ($summary.errorLines -eq 0 -and $summary.clientErrors -eq 0); detail = "error lines $($summary.errorLines), client errors $($summary.clientErrors)" }
$checks += [pscustomobject]@{ name = "db-growth"; pass = ($dbGrowthMb -lt 200); detail = "$dbGrowthMb MB" }
$checks += [pscustomobject]@{ name = "wal-peak"; pass = ($peakWalMb -lt 100); detail = "peak $peakWalMb MB" }
$checks += [pscustomobject]@{ name = "process-exit"; pass = ($stoppedAfterMs -ge 0 -and $stoppedAfterMs -lt 5000); detail = "$stoppedAfterMs ms" }

$failed = @($checks | Where-Object { -not $_.pass })
$report = [pscustomobject]@{ tag = $Tag; startedAt = $started.ToString("o"); minutes = $Minutes; intervalSec = $IntervalSec; sampleSec = $SampleSec; samples = $sampleCount; client = $summary; clientExit = $clientExit; memoryFirstMb = $firstMb; memoryPeakMb = $peakMb; memoryRegressionMbPer10Min = $regressionMbPer10Min; dbGrowthMb = $dbGrowthMb; walPeakMb = $peakWalMb; stoppedAfterMs = $stoppedAfterMs; checks = $checks; failed = $failed.Count }
$reportPath = Join-Path $logs "soak-report.json"
$report | ConvertTo-Json -Depth 6 | Out-File -FilePath $reportPath -Encoding utf8

Write-Host ""
Write-Host "===== soak 判据 ====="
foreach ($check in $checks) {
    $mark = if ($check.pass) { "PASS" } else { "FAIL" }
    Write-Host ("[{0}] {1,-20} {2}" -f $mark, $check.name, $check.detail)
}
Write-Host ""
Write-Host "[soak] 报告 $reportPath"
if ($failed.Count -gt 0) { Write-Host "[soak] $($failed.Count) 条判据未过" -ForegroundColor Red; exit 1 }
Write-Host "[soak] 全部通过" -ForegroundColor Green
exit 0

