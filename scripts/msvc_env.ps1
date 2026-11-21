param([Parameter(Mandatory = $true)][ValidateSet("x86_64", "aarch64")][string]$Architecture)
$ErrorActionPreference = "Stop"
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio/Installer/vswhere.exe"
$installation = (& $vswhere -latest -products '*' -property installationPath).Trim()
if (-not $installation) { throw "Visual Studio C/C++ tools are required" }
$module = Join-Path $installation "Common7/Tools/Microsoft.VisualStudio.DevShell.dll"
Import-Module $module
$target = if ($Architecture -eq "aarch64") { "arm64" } else { "amd64" }
$hostArchitecture = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }
Enter-VsDevShell -VsInstallPath $installation -SkipAutomaticLocation -DevCmdArguments "-arch=$target -host_arch=$hostArchitecture" | Out-Null
$compiler = (Get-Command cl.exe -CommandType Application -ErrorAction Stop).Source
$compilerDirectory = Split-Path -Parent $compiler
$linker = Join-Path $compilerDirectory "link.exe"
$librarian = Join-Path $compilerDirectory "lib.exe"
foreach ($tool in @($linker, $librarian)) {
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) { throw "Missing MSVC tool next to cl.exe: $tool" }
}
$toolsRoot = [IO.Path]::GetFullPath($env:VCToolsInstallDir).TrimEnd('\') + '\'
if (-not $compiler.StartsWith($toolsRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "cl.exe did not resolve inside the selected Visual Studio tools: $compiler"
}
$targetTriple = "$Architecture-pc-windows-msvc"
$rustVersion = & rustc -vV
if ($LASTEXITCODE -ne 0) { throw "Cannot inspect the active Rust host toolchain" }
$hostLine = @($rustVersion | Where-Object { $_ -match '^host: ' })
if ($hostLine.Count -ne 1) { throw "rustc did not report exactly one host triple" }
$hostTriple = $hostLine[0].Substring(6).Trim()
if ($hostTriple -ne $targetTriple) {
    throw "Native release requires Rust host $targetTriple; active host is $hostTriple"
}
# Git Bash can put its POSIX link.exe before MSVC even after PATH is exported.
# Cargo reads the host triple's linker for build scripts, including with --target.
$linkerVariables = @()
foreach ($triple in (@($hostTriple, $targetTriple) | Select-Object -Unique)) {
    $name = 'CARGO_TARGET_' + $triple.Replace('-', '_').ToUpperInvariant() + '_LINKER'
    [Environment]::SetEnvironmentVariable($name, $linker, 'Process')
    $linkerVariables += $name
}
$env:ZCHAT_MSVC_BIN = $compilerDirectory
$compilerVariables = @("PATH", "INCLUDE", "LIB", "LIBPATH", "VCINSTALLDIR", "VCToolsInstallDir", "WindowsSdkDir", "WindowsSDKVersion", "VisualStudioVersion", "VSCMD_ARG_HOST_ARCH", "VSCMD_ARG_TGT_ARCH", "UniversalCRTSdkDir", "UCRTVersion", "DevEnvDir", "WindowsSdkBinPath", "WindowsSdkVerBinPath", "VSINSTALLDIR", "VSCMD_VER", "ExtensionSdkDir")
foreach ($name in ($compilerVariables + $linkerVariables + @('ZCHAT_MSVC_BIN'))) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value) {
        if ($value.Contains("`n") -or $value.Contains("`r")) { throw "Unexpected multiline compiler environment" }
        Add-Content -LiteralPath $env:GITHUB_ENV -Value "$name=$value" -Encoding utf8
    }
}
Write-Output "Configured native MSVC compiler: $compiler"
Write-Output "Rust host and target: $hostTriple; absolute linker: $linker"
