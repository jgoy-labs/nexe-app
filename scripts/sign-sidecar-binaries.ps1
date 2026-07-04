# sign-sidecar-binaries.ps1 — Authenticode-sign the sidecar bundle binaries.
#
# Gated by build-sidecar.sh Step 6.5: only invoked when WINDOWS_SIGNING_CERT is set.
# DEFERRED by default — the first Windows ARM64 installer ships UNSIGNED (SmartScreen
# shows an "unknown publisher" prompt with an "Run anyway" button; software still
# installs and runs). Wire a real cert (OV/EV from a commercial CA, or Azure Trusted Signing)
# by setting WINDOWS_SIGNING_CERT, then this script signs *.exe/*.dll/*.pyd in the bundle.
#
# Env:
#   WINDOWS_SIGNING_CERT     40-hex certificate thumbprint (preferred) OR a path to a .pfx
#   WINDOWS_SIGNING_TS_URL   RFC-3161 timestamp URL (default: DigiCert)
param(
    [Parameter(Mandatory = $true)][string]$SidecarDir
)
$ErrorActionPreference = "Stop"

$cert = $env:WINDOWS_SIGNING_CERT
if ([string]::IsNullOrEmpty($cert)) {
    Write-Error "WINDOWS_SIGNING_CERT unset — sign-sidecar-binaries.ps1 must not be called directly"
    exit 1
}
$tsUrl = if ($env:WINDOWS_SIGNING_TS_URL) { $env:WINDOWS_SIGNING_TS_URL } else { "http://timestamp.digicert.com" }

# Resolve signtool.exe (Windows SDK). Prefer PATH, then the SDK bin dirs (arm64/x64).
$signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source
if (-not $signtool) {
    $cand = Get-ChildItem `
        "C:\Program Files (x86)\Windows Kits\10\bin\*\arm64\signtool.exe", `
        "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe" `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cand) { $signtool = $cand.FullName }
}
if (-not $signtool) { Write-Error "signtool.exe not found (install the Windows SDK)"; exit 1 }

# Certificate selector: 40-hex thumbprint -> /sha1; otherwise treat as a .pfx path (/f).
$certArgs = if ($cert -match '^[0-9A-Fa-f]{40}$') { @("/sha1", $cert) } else { @("/f", $cert) }

$exts = @("*.exe", "*.dll", "*.pyd")
$targets = Get-ChildItem -Path $SidecarDir -Recurse -Include $exts -File -ErrorAction SilentlyContinue
Write-Host "sign-sidecar: signing $($targets.Count) binaries in $SidecarDir ..."
foreach ($f in $targets) {
    & $signtool sign /fd sha256 /tr $tsUrl /td sha256 @certArgs $f.FullName
    if ($LASTEXITCODE -ne 0) { Write-Error "signtool failed on $($f.FullName)"; exit 1 }
}
Write-Host "sign-sidecar: done ($($targets.Count) binaries signed)."
