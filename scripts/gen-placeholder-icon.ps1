<#
.SYNOPSIS
    生成 1024x1024 的**占位**应用主图标（纯本地绘制，不引用任何外部图片）。

.DESCRIPTION
    为什么需要它：`crates/hbot-desktop/icons/icon.png` 只有 256x256，
    而 `cargo tauri icon`（生成 .icns / 多尺寸 .ico 的唯一官方入口）
    要求输入图至少 1024x1024 —— 否则 macOS 的 .icns 做不出来。

    **这是占位图，不是最终设计稿**：深色圆角底 + "HB" 字标 + 底部 "PLACEHOLDER" 字样。
    设计侧给出正式主图后，用正式图替换 `icons/icon-1024.png` 再跑一次
    `cargo tauri icon` 即可（生成命令见 docs/spikes/S5-packaging.md §5）。

    用法：
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts/gen-placeholder-icon.ps1
        powershell ... -File scripts/gen-placeholder-icon.ps1 -OutPath D:\tmp\x.png -Size 1024

    退出码：0 = 生成成功；1 = 失败（打印原因）。
#>
[CmdletBinding()]
param(
    [string]$OutPath = '',
    [int]$Size = 1024
)

$ErrorActionPreference = 'Stop'

# PS 5.1 在 param 默认值里读不到 $PSScriptRoot（为空），所以在这里再解析脚本目录
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
if ([string]::IsNullOrWhiteSpace($OutPath)) {
    $OutPath = Join-Path $scriptDir '..\crates\hbot-desktop\icons\icon-1024.png'
}

try {
    Add-Type -AssemblyName System.Drawing
} catch {
    Write-Error "无法加载 System.Drawing（需要 Windows PowerShell 或带 GDI+ 的 pwsh）：$($_.Exception.Message)"
    exit 1
}

if ($Size -lt 1024) {
    Write-Error "Size 必须 >= 1024（tauri icon 的硬要求），收到 $Size"
    exit 1
}

$resolved = [System.IO.Path]::GetFullPath($OutPath)
$dir = [System.IO.Path]::GetDirectoryName($resolved)
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }

$bmp = $null
try {
    $bmp = [System.Drawing.Bitmap]::new($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
        $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $g.Clear([System.Drawing.Color]::Transparent)

        # 圆角矩形路径：半径 = 边长的 22%（接近 Windows/macOS 图标的视觉重量）
        $r = [int]($Size * 0.22)
        $d2 = $r * 2
        $path = [System.Drawing.Drawing2D.GraphicsPath]::new()
        $path.AddArc(0, 0, $d2, $d2, 180, 90)
        $path.AddArc($Size - $d2, 0, $d2, $d2, 270, 90)
        $path.AddArc($Size - $d2, $Size - $d2, $d2, $d2, 0, 90)
        $path.AddArc(0, $Size - $d2, $d2, $d2, 90, 90)
        $path.CloseFigure()

        # 底色：深蓝紫对角渐变
        $rect = [System.Drawing.Rectangle]::new(0, 0, $Size, $Size)
        $c1 = [System.Drawing.Color]::FromArgb(255, 30, 30, 46)
        $c2 = [System.Drawing.Color]::FromArgb(255, 49, 50, 78)
        $mode = [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal
        $brush = [System.Drawing.Drawing2D.LinearGradientBrush]::new($rect, $c1, $c2, $mode)
        try { $g.FillPath($brush, $path) } finally { $brush.Dispose() }

        # 字标 "HB"：字号随边长缩放，视觉中心略上移（给底部 PLACEHOLDER 留位）
        $fontMain = [System.Drawing.Font]::new('Segoe UI', [single]($Size * 0.40), [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
        $sf = [System.Drawing.StringFormat]::new()
        try {
            $sf.Alignment = [System.Drawing.StringAlignment]::Center
            $sf.LineAlignment = [System.Drawing.StringAlignment]::Center
            $mainRect = [System.Drawing.RectangleF]::new(0, [single]($Size * -0.04), [single]$Size, [single]$Size)
            $white = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 235, 238, 250))
            try { $g.DrawString('HB', $fontMain, $white, $mainRect, $sf) } finally { $white.Dispose() }
        } finally { $fontMain.Dispose(); $sf.Dispose() }

        # 底部 "PLACEHOLDER"：源图可读，缩到 32x32 自然糊掉，不破坏小尺寸观感
        $fontTag = [System.Drawing.Font]::new('Consolas', [single]($Size * 0.055), [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
        $sf2 = [System.Drawing.StringFormat]::new()
        try {
            $sf2.Alignment = [System.Drawing.StringAlignment]::Center
            $sf2.LineAlignment = [System.Drawing.StringAlignment]::Center
            $tagRect = [System.Drawing.RectangleF]::new(0, [single]($Size * 0.72), [single]$Size, [single]($Size * 0.12))
            $accent = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 137, 180, 250))
            try { $g.DrawString('PLACEHOLDER', $fontTag, $accent, $tagRect, $sf2) } finally { $accent.Dispose() }
        } finally { $fontTag.Dispose(); $sf2.Dispose() }

        $path.Dispose()
    } finally { $g.Dispose() }

    $bmp.Save($resolved, [System.Drawing.Imaging.ImageFormat]::Png)
    Write-Output "已生成: $resolved"
} catch {
    Write-Error "生成失败：$($_.Exception.Message)"
    exit 1
} finally {
    if ($bmp) { $bmp.Dispose() }
}

# 回报真实像素（从文件重新读，证明落盘的就是 1024）
try {
    $probe = [System.Drawing.Image]::FromFile($resolved)
    try { Write-Output ("尺寸 {0}x{1}，{2} 字节" -f $probe.Width, $probe.Height, (Get-Item $resolved).Length) }
    finally { $probe.Dispose() }
} catch {
    Write-Error "落盘校验失败：$($_.Exception.Message)"
    exit 1
}

exit 0