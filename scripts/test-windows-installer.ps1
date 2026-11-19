param(
  [string]$HelperPath = (Join-Path $PSScriptRoot '..\crates\atman-cli\src\upgrade\windows_replace.ps1')
)

$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
  throw 'Windows executable publication requires a native Windows test host.'
}
. $HelperPath

function Assert-Condition {
  param([bool]$Condition, [string]$Message)
  if (-not $Condition) { throw $Message }
}

function Stop-FixtureProcess {
  param([System.Diagnostics.Process]$Process)
  if ($null -ne $Process -and -not $Process.HasExited) {
    $Process.Kill()
    Assert-Condition ($Process.WaitForExit(5000)) 'The original fixture did not exit.'
  }
}

$root = Join-Path ([System.IO.Path]::GetTempPath()) ("atman upgrade test " + [guid]::NewGuid().ToString('N'))
$directory = Join-Path $root 'bin'
$destination = Join-Path $directory 'atman.exe'
$incoming = Join-Path $root 'incoming.exe'
$running = $null
try {
  $null = [System.IO.Directory]::CreateDirectory($directory)
  $original = Join-Path ([Environment]::SystemDirectory) 'ping.exe'
  $revision = Join-Path ([Environment]::SystemDirectory) 'hostname.exe'
  [System.IO.File]::Copy($original, $destination)
  [System.IO.File]::Copy($revision, $incoming)
  $oldHash = (Get-FileHash -LiteralPath $destination).Hash
  $newHash = (Get-FileHash -LiteralPath $incoming).Hash
  Assert-Condition ($oldHash -ne $newHash) 'The original and replacement images must differ.'

  $running = Start-Process -FilePath $destination -ArgumentList @('-t', '127.0.0.1') -PassThru -NoNewWindow `
    -RedirectStandardOutput (Join-Path $root 'stdout.log') -RedirectStandardError (Join-Path $root 'stderr.log')
  Assert-Condition (-not $running.HasExited) 'The original native image must be running during publication.'
  $blocked = $false
  try {
    Copy-Item -LiteralPath $incoming -Destination $destination -ErrorAction Stop
  } catch {
    $errorCode = $_.Exception.GetBaseException().HResult -band 0xffff
    if ($errorCode -notin @(5, 32, 33)) { throw }
    $blocked = $true
  }
  Assert-Condition $blocked 'Windows allowed in-place writes to a mapped executable; the fixture did not reproduce the original failure.'
  Assert-Condition ((Get-FileHash -LiteralPath $destination).Hash -eq $oldHash) 'The failed in-place copy changed the executable.'

  $stageFailed = $false
  try {
    Install-AtmanExecutable (Join-Path $root 'missing.exe') $destination
  } catch {
    $stageFailed = $true
  }
  Assert-Condition $stageFailed 'An unreadable incoming image must fail without publishing.'
  Assert-Condition ((Get-FileHash -LiteralPath $destination).Hash -eq $oldHash) 'Staging failure changed the executable.'
  Assert-Condition (@([System.IO.Directory]::GetFiles($directory)).Count -eq 1) 'Staging failure left lock or staging files.'

  $stale = Join-Path $directory ('.atman.exe.atman-retired-' + [guid]::NewGuid().ToString('N') + '.exe')
  $unrelated = Join-Path $directory '.atman.exe.atman-retired-do-not-remove.exe'
  [System.IO.File]::Copy($original, $stale)
  [System.IO.File]::WriteAllText($unrelated, 'user-owned file')

  Install-AtmanExecutable $incoming $destination
  Assert-Condition ((Get-FileHash -LiteralPath $destination).Hash -eq $newHash) 'The installed image differs from the prepared image.'
  Assert-Condition (-not $running.HasExited) 'Publication must not stop the original process.'
  Assert-Condition (-not [System.IO.File]::Exists($stale)) 'A previous unmapped installer-owned image was not cleaned up.'
  $retired = @([System.IO.Directory]::GetFiles($directory) | Where-Object {
    [System.IO.Path]::GetFileName($_) -cmatch '^\.atman\.exe\.atman-retired-[0-9a-f]{32}\.exe$'
  })
  Assert-Condition ($retired.Count -eq 1) 'The original mapped image must remain on disk.'
  Assert-Condition ((Get-FileHash -LiteralPath $retired[0]).Hash -eq $oldHash) 'The retired file differs from the original image.'

  $installed = Start-Process -FilePath $destination -PassThru -Wait -NoNewWindow `
    -RedirectStandardOutput (Join-Path $root 'new-stdout.log') -RedirectStandardError (Join-Path $root 'new-stderr.log')
  Assert-Condition ($installed.ExitCode -eq 0) 'The newly published image did not start successfully.'

  Stop-FixtureProcess $running
  Install-AtmanExecutable $incoming $destination
  Assert-Condition (-not [System.IO.File]::Exists($retired[0])) 'A later publication did not reclaim the retired image after its owner exited.'
  Assert-Condition ([System.IO.File]::ReadAllText($unrelated) -eq 'user-owned file') 'The installer changed an unrelated file.'
  Assert-Condition (@([System.IO.Directory]::GetFiles($directory)).Count -eq 2) 'Unexpected files remain next to the installed executable.'
  Write-Host 'Verified Windows publication, mapped-image retention, and later cleanup.'
} finally {
  try {
    Stop-FixtureProcess $running
  } finally {
    if ($null -ne $running) { $running.Dispose() }
    if ([System.IO.Directory]::Exists($root)) { [System.IO.Directory]::Delete($root, $true) }
  }
}
