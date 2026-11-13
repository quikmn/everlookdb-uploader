# Called by `cargo tauri build` for every Windows file it signs (tauri.conf.json bundle.windows.signCommand).
# Signs with the Certum code-signing certificate ("Open Source Developer Viktor Lundgren", a SimplySign cloud
# certificate that is only present while SimplySign Desktop is logged in), timestamped so signatures stay valid
# after the certificate expires. Without the certificate it leaves files unsigned, so anyone can build, except
# in a release (EVERLOOKDB_REQUIRE_SIGNING=1, set by release-windows.ps1), where that must stop the build.
# EVERLOOKDB_SKIP_SIGNING=1 leaves files unsigned even when the certificate is there (test builds).
param([Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Files)
$ErrorActionPreference = 'Stop'
if ($env:EVERLOOKDB_SKIP_SIGNING -eq '1' -and $env:EVERLOOKDB_REQUIRE_SIGNING -ne '1') { Write-Host 'EVERLOOKDB_SKIP_SIGNING=1: leaving files unsigned.' -ForegroundColor Yellow; exit 0 }
# The Cert: drive comes from this module; Windows PowerShell can't autoload it when PSModulePath points at PowerShell 7's modules.
if (-not (Get-PSDrive Cert -ErrorAction SilentlyContinue)) { Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1') }
# Filter on the key usage itself: -CodeSigningCert is missing when the module path points at PowerShell 7's modules.
$cert = Get-ChildItem Cert:\CurrentUser\My |
  Where-Object { $_.HasPrivateKey -and $_.Subject -like '*Open Source Developer Viktor Lundgren*' -and ($_.EnhancedKeyUsageList.ObjectId -contains '1.3.6.1.5.5.7.3.3') } |
  Select-Object -First 1
if (-not $cert) {
  if ($env:EVERLOOKDB_REQUIRE_SIGNING -eq '1') { throw 'Signing failed: no code-signing certificate (SimplySign Desktop logged out?)' }
  Write-Host 'No code-signing certificate found: leaving files unsigned.' -ForegroundColor Yellow; exit 0
}
foreach ($file in $Files) {
  $result = Set-AuthenticodeSignature -FilePath $file -Certificate $cert -TimestampServer 'http://time.certum.pl' -HashAlgorithm SHA256
  if ($result.Status -ne 'Valid') { throw "Signing $file failed: $($result.StatusMessage)" }
  Write-Host "Signed $file"
}
