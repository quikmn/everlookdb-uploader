# Windows release, run on the owner's PC (the code-signing certificate is a SimplySign cloud certificate that only
# exists while SimplySign Desktop is logged in, so this can't run in CI):
#   1. bump "version" in src-tauri/tauri.conf.json and src-tauri/Cargo.toml, commit, tag vX.Y.Z, push the tag
#      (that starts the macOS build in GitHub Actions, which uploads the Mac files to the same release)
#   2. log in to SimplySign Desktop, then run:  powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1
#   3. when the Mac job has finished, run it again with -LatestOnly to add the Mac entries to latest.json
#   4. copy dist\latest.json to https://everlookdb.com/uploader/latest.json (installed apps check that file)
# Builds and signs the per-user NSIS installer, signs it for the updater, uploads it to the GitHub release and writes
# latest.json. The release stays a draft until it has both the Windows and the Mac files; then it is published.
param(
  [switch]$LatestOnly,  # only rebuild latest.json from the files already on the release
  [switch]$NoUpload,    # build and sign, but don't touch the GitHub release
  [string]$KeyFile = 'D:\Development_Projects\devkeys\everlookdb-uploader-updater.key'
)
$ErrorActionPreference = 'Stop'
# The Cert: drive comes from this module; Windows PowerShell can't autoload it when PSModulePath points at PowerShell 7's modules.
if (-not (Get-PSDrive Cert -ErrorAction SilentlyContinue)) { Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1') }
$Repo = 'quikmn/everlookdb-uploader'
$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root
$Version = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
$Tag = "v$Version"
$Dist = Join-Path $Root 'dist'
New-Item -ItemType Directory -Force $Dist | Out-Null
$WinAsset = "EverlookDB-Uploader_${Version}_x64-setup.exe"
$MacAsset = "EverlookDB-Uploader_${Version}_universal.app.tar.gz"
$Download = "https://github.com/$Repo/releases/download/$Tag"

if (-not $LatestOnly) {
  $cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.HasPrivateKey -and $_.Subject -like '*Open Source Developer Viktor Lundgren*' -and ($_.EnhancedKeyUsageList.ObjectId -contains '1.3.6.1.5.5.7.3.3') } |
    Select-Object -First 1
  if (-not $cert) { Write-Host 'No code-signing certificate: log in to SimplySign Desktop and run this again.' -ForegroundColor Red; exit 1 }
  if (-not (Test-Path $KeyFile) -or -not (Test-Path "$KeyFile.password")) { Write-Host "Updater key or its .password file is missing: $KeyFile" -ForegroundColor Red; exit 1 }
  if ((git tag --points-at HEAD) -notcontains $Tag) { Write-Host "HEAD isn't tagged $Tag. Tag and push it first." -ForegroundColor Red; exit 1 }

  $env:EVERLOOKDB_REQUIRE_SIGNING = '1'
  $env:TAURI_SIGNING_PRIVATE_KEY = $KeyFile
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = (Get-Content "$KeyFile.password" -Raw).Trim()
  try {
    npx --yes @tauri-apps/cli@2 build --bundles nsis
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }
  } finally {
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD, Env:EVERLOOKDB_REQUIRE_SIGNING -ErrorAction SilentlyContinue
  }
  $setup = Get-ChildItem src-tauri\target\release\bundle\nsis\*_x64-setup.exe | Where-Object Name -like "*_${Version}_*" | Select-Object -First 1
  if (-not $setup) { throw 'No installer was built.' }
  foreach ($f in @($setup.FullName, 'src-tauri\target\release\everlookdb-uploader.exe')) {
    $sig = Get-AuthenticodeSignature $f
    if ($sig.Status -ne 'Valid') { throw "$f isn't signed: $($sig.StatusMessage)" }
  }
  Copy-Item $setup.FullName (Join-Path $Dist $WinAsset) -Force
  Copy-Item "$($setup.FullName).sig" (Join-Path $Dist "$WinAsset.sig") -Force
  Write-Host "Built and signed $WinAsset" -ForegroundColor Green

  if (-not $NoUpload) {
    gh release view $Tag -R $Repo *> $null
    if ($LASTEXITCODE -ne 0) { gh release create $Tag -R $Repo --draft --title "EverlookDB Uploader $Version" --notes "EverlookDB Uploader $Version" }
    gh release upload $Tag -R $Repo (Join-Path $Dist $WinAsset) (Join-Path $Dist "$WinAsset.sig") --clobber
    if ($LASTEXITCODE -ne 0) { throw 'Upload failed.' }
  }
}

# latest.json for the Tauri updater: every platform points at the files on the GitHub release.
$platforms = [ordered]@{}
$winSig = Join-Path $Dist "$WinAsset.sig"
if (-not (Test-Path $winSig) -and -not $NoUpload) { gh release download $Tag -R $Repo -p "$WinAsset.sig" -D $Dist --clobber }
if (Test-Path $winSig) {
  $platforms['windows-x86_64'] = @{ signature = (Get-Content $winSig -Raw).Trim(); url = "$Download/$WinAsset" }
}
$macSig = Join-Path $Dist "$MacAsset.sig"
if (-not $NoUpload) { gh release download $Tag -R $Repo -p "$MacAsset.sig" -D $Dist --clobber 2>$null }
if (Test-Path $macSig) {
  $mac = @{ signature = (Get-Content $macSig -Raw).Trim(); url = "$Download/$MacAsset" }
  $platforms['darwin-aarch64'] = $mac
  $platforms['darwin-x86_64'] = $mac
} else {
  Write-Host 'The Mac files are not on the release yet: latest.json lists Windows only. Run with -LatestOnly when the Mac job is done.' -ForegroundColor Yellow
}
$latest = [ordered]@{
  version   = $Version
  notes     = "EverlookDB Uploader $Version"
  pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
  platforms = $platforms
}
$latestPath = Join-Path $Dist 'latest.json'
[IO.File]::WriteAllText($latestPath, ($latest | ConvertTo-Json -Depth 5))
Write-Host "Wrote $latestPath"
if (-not $NoUpload) {
  gh release upload $Tag -R $Repo $latestPath --clobber
  if ($platforms.Contains('windows-x86_64') -and $platforms.Contains('darwin-aarch64')) {
    gh release edit $Tag -R $Repo --draft=false
    Write-Host "Published $Tag. Now copy dist\latest.json to https://everlookdb.com/uploader/latest.json." -ForegroundColor Green
  }
}
