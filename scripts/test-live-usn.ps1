param(
    [string]$Compiler = (Join-Path $env:USERPROFILE '.rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin\rustc.exe')
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $projectRoot
if (!(Test-Path -LiteralPath $Compiler -PathType Leaf)) { throw "Missing existing compiler: $Compiler" }
$runName = 'run-' + [Guid]::NewGuid().ToString('N')
$outputRoot = Join-Path $projectRoot 'target\live-usn'
$runRoot = Join-Path $outputRoot $runName
New-Item -ItemType Directory -Path $runRoot | Out-Null
$testExe = Join-Path $runRoot 'live-usn-tests.exe'
$compileArgs = @('--crate-name', 'rustsearch', '--edition=2021', '--test', 'src/main.rs', '-L', 'dependency=target/release/deps', '-o', $testExe)
foreach ($name in @('anyhow', 'bincode', 'eframe', 'egui', 'egui_ratatui', 'global_hotkey', 'ratatui', 'serde', 'soft_ratatui', 'tray_icon', 'walkdir')) {
    $library = @(Get-ChildItem -LiteralPath 'target\release\deps' -Filter "lib$name-*.rlib")
    if ($library.Count -ne 1) { throw "Missing or ambiguous existing compiled library: $name" }
    $compileArgs += @('--extern', "$name=$($library[0].FullName)")
}
$compileArgs += @('--extern', 'windows_sys=target/release/deps/libwindows_sys-cf24304c650bc985.rlib')
foreach ($version in @('0.52.6', '0.53.1')) {
    $native = Join-Path $projectRoot "target\validation-cargo-home\registry\src\index.crates.io-1949cf8c6b5b557f\windows_x86_64_msvc-$version\lib"
    if (!(Test-Path -LiteralPath $native)) { throw "Missing existing native library: $native" }
    $compileArgs += @('-L', "native=$native")
}
& $compiler @compileArgs
if ($LASTEXITCODE -ne 0) { throw 'Live test compilation failed' }
$log = Join-Path $outputRoot "$runName-results.txt"
Write-Output "Live fixture: $runRoot"
Write-Output "Results: $log"
& $testExe --ignored --exact live_usn_tests::live_usn_roundtrip --nocapture --test-threads=1 2>&1 | Tee-Object -FilePath $log
if ($LASTEXITCODE -ne 0) { throw "Live USN test failed; results: $log" }
