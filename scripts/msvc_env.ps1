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
Get-Command cl.exe -ErrorAction Stop | Out-Null
Get-Command lib.exe -ErrorAction Stop | Out-Null
$compilerVariables = @("PATH", "INCLUDE", "LIB", "LIBPATH", "VCINSTALLDIR", "VCToolsInstallDir", "WindowsSdkDir", "WindowsSDKVersion", "VisualStudioVersion", "VSCMD_ARG_HOST_ARCH", "VSCMD_ARG_TGT_ARCH", "UniversalCRTSdkDir", "UCRTVersion", "DevEnvDir", "WindowsSdkBinPath", "WindowsSdkVerBinPath", "VSINSTALLDIR", "VSCMD_VER", "ExtensionSdkDir")
foreach ($name in $compilerVariables) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value) {
        if ($value.Contains("`n") -or $value.Contains("`r")) { throw "Unexpected multiline compiler environment" }
        Add-Content -LiteralPath $env:GITHUB_ENV -Value "$name=$value" -Encoding utf8
    }
}
Write-Output "Configured the MSVC $target compiler."
