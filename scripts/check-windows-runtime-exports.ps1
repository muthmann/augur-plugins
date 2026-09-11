[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Root
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class AugurRuntimeExportProbe
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryEx(
        string fileName, IntPtr file, uint flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern IntPtr GetProcAddress(
        IntPtr module, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeLibrary(IntPtr module);
}
'@

$dlls = @(Get-ChildItem -LiteralPath $Root -Recurse -File -Filter "*.dll")
if ($dlls.Count -eq 0) {
    throw "No DLLs found below $Root"
}

$failures = @()
foreach ($dll in $dlls) {
    $handle = [AugurRuntimeExportProbe]::LoadLibraryEx(
        $dll.FullName, [IntPtr]::Zero, 1)

    if ($handle -eq [IntPtr]::Zero) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        $failures += "$($dll.FullName): LoadLibraryEx failed with Win32 error $errorCode"
        continue
    }

    try {
        $symbol = [AugurRuntimeExportProbe]::GetProcAddress(
            $handle, "augur_plugin_vtable")
        if ($symbol -eq [IntPtr]::Zero) {
            $failures += "$($dll.FullName): missing augur_plugin_vtable"
        } else {
            Write-Host "OK: $($dll.FullName)"
        }
    }
    finally {
        [AugurRuntimeExportProbe]::FreeLibrary($handle) | Out-Null
    }
}

if ($failures.Count -gt 0) {
    $failures | ForEach-Object { Write-Error $_ }
    exit 1
}
