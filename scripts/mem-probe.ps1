# 采样进程树内存（Windows）。用于 S0/S2 基线，不参与产品运行时。
#
# 用法：
#   pwsh -File scripts/mem-probe.ps1 -Name hbot
#   pwsh -File scripts/mem-probe.ps1 -Id 1234 -Seconds 5 -IncludeChildren -Json
#   pwsh -File scripts/mem-probe.ps1 -Id 1234 -Seconds 30 -IncludeChildren -SplitChildren -IntervalMs 500 -Json
#
# 口径：WorkingSet64 是任务管理器“内存（工作集）”；PrivateMemorySize64 是提交的私有内存。
# 报告里必须写清用的是哪一个，别混。
#
# -SplitChildren：把「根进程」与「全部后代合计」分开报。多进程 UI（WebView2）必须用它，
# 否则单看根进程会漏掉渲染/GPU/网络三个子进程，而任务管理器里用户看到的是加起来的总数。
#   注意：子进程集合每个采样点重新枚举（子进程会随页面加载陆续出现），所以 Children
#   的数字是“当时存活的子进程合计”，不是固定集合。

[CmdletBinding(DefaultParameterSetName = 'ByName')]
param(
    [Parameter(ParameterSetName = 'ByName', Mandatory = $true)]
    [string]$Name,

    [Parameter(ParameterSetName = 'ById', Mandatory = $true)]
    [int]$Id,

    [int]$Seconds = 3,
    [int]$IntervalMs = 200,
    [switch]$IncludeChildren,
    [switch]$SplitChildren,
    [switch]$Json
)

$ErrorActionPreference = 'Stop'

function Get-RootProcess {
    if ($PSCmdlet.ParameterSetName -eq 'ById') {
        return Get-Process -Id $Id -ErrorAction Stop
    }
    $p = Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $p) { throw "找不到进程：$Name" }
    return $p
}

function Get-DescendantIds {
    param([int]$RootId)
    $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId
    $children = @{}
    foreach ($row in $all) {
        if (-not $children.ContainsKey([int]$row.ParentProcessId)) {
            $children[[int]$row.ParentProcessId] = @()
        }
        $children[[int]$row.ParentProcessId] += [int]$row.ProcessId
    }
    $result = New-Object System.Collections.Generic.List[int]
    $queue = New-Object System.Collections.Generic.Queue[int]
    $queue.Enqueue($RootId)
    while ($queue.Count -gt 0) {
        $cur = $queue.Dequeue()
        if ($children.ContainsKey($cur)) {
            foreach ($c in $children[$cur]) {
                if (-not $result.Contains($c)) {
                    $result.Add($c)
                    $queue.Enqueue($c)
                }
            }
        }
    }
    return $result
}

$root = Get-RootProcess
$samples = New-Object System.Collections.Generic.List[object]
$deadline = (Get-Date).AddSeconds($Seconds)

while ((Get-Date) -lt $deadline) {
    $ids = @($root.Id)
    if ($IncludeChildren) {
        $ids += (Get-DescendantIds -RootId $root.Id)
    }

    $working = 0L
    $private = 0L
    $alive = 0
    $rootWorking = 0L
    $rootPrivate = 0L
    $childWorking = 0L
    $childPrivate = 0L
    $childCount = 0
    foreach ($procId in $ids) {
        $p = Get-Process -Id $procId -ErrorAction SilentlyContinue
        if ($p) {
            $isRoot = ($procId -eq $root.Id)
            $working += $p.WorkingSet64
            $private += $p.PrivateMemorySize64
            $alive++
            if ($isRoot) {
                $rootWorking = $p.WorkingSet64
                $rootPrivate = $p.PrivateMemorySize64
            } else {
                $childWorking += $p.WorkingSet64
                $childPrivate += $p.PrivateMemorySize64
                $childCount++
            }
        }
    }
    if ($alive -eq 0) { throw "进程已退出：$($root.Id)" }

    $samples.Add([pscustomobject]@{
            At            = (Get-Date).ToString('o')
            Processes     = $alive
            ChildCount    = $childCount
            WorkingSet    = $working
            Private       = $private
            RootWorking   = $rootWorking
            RootPrivate   = $rootPrivate
            ChildWorking  = $childWorking
            ChildPrivate  = $childPrivate
        })
    Start-Sleep -Milliseconds $IntervalMs
}

function Stat([object[]]$Values) {
    $nums = @($Values | ForEach-Object { [double]$_ })
    $avg = ($nums | Measure-Object -Average).Average
    [pscustomobject]@{
        Min = [math]::Round(($nums | Measure-Object -Minimum).Minimum / 1MB, 1)
        Avg = [math]::Round($avg / 1MB, 1)
        Max = [math]::Round(($nums | Measure-Object -Maximum).Maximum / 1MB, 1)
    }
}

$result = [pscustomobject]@{
    Name            = $root.ProcessName
    Pid             = $root.Id
    IncludeChildren = [bool]$IncludeChildren
    SplitChildren   = [bool]$SplitChildren
    IntervalMs      = $IntervalMs
    Seconds         = $Seconds
    Samples         = $samples.Count
    WorkingSetMB    = Stat ($samples | ForEach-Object { $_.WorkingSet })
    PrivateMB       = Stat ($samples | ForEach-Object { $_.Private })
    GeneratedAt     = (Get-Date).ToString('o')
}

if ($SplitChildren) {
    $result | Add-Member -NotePropertyName Root -NotePropertyValue ([pscustomobject]@{
            WorkingSetMB = Stat ($samples | ForEach-Object { $_.RootWorking })
            PrivateMB    = Stat ($samples | ForEach-Object { $_.RootPrivate })
        })
    $result | Add-Member -NotePropertyName Children -NotePropertyValue ([pscustomobject]@{
            CountMin     = ($samples | ForEach-Object { $_.ChildCount } | Measure-Object -Minimum).Minimum
            CountMax     = ($samples | ForEach-Object { $_.ChildCount } | Measure-Object -Maximum).Maximum
            WorkingSetMB = Stat ($samples | ForEach-Object { $_.ChildWorking })
            PrivateMB    = Stat ($samples | ForEach-Object { $_.ChildPrivate })
        })
}

if ($Json) {
    $result | ConvertTo-Json -Depth 4
} else {
    $result | Format-List
}