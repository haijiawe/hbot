# 一条命令的本地闸门（Windows）。
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

Write-Host '== fmt =='; cargo fmt --all -- --check
Write-Host '== clippy =='; cargo clippy --workspace --all-targets -- -D warnings
Write-Host '== test =='; cargo test --workspace
Write-Host '== deps direction =='; cargo run -q -p xtask -- deps

if (Test-Path 'apps/web/node_modules') {
  Write-Host '== web =='
  Push-Location apps/web; try { pnpm run check } finally { Pop-Location }
} else {
  Write-Host '== web skipped (no node_modules) =='
}
Write-Host 'OK'
