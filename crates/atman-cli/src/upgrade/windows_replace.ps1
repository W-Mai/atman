function Open-AtmanInstallLock {
  param([string]$Path)

  for ($attempt = 0; $attempt -lt 100; $attempt++) {
    try {
      return [System.IO.FileStream]::new(
        $Path, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::None, 4096, [System.IO.FileOptions]::DeleteOnClose)
    } catch {
      $errorCode = $_.Exception.GetBaseException().HResult -band 0xffff
      if ($errorCode -notin @(5, 32, 33, 80, 183)) { throw }
      if ($attempt -eq 99) { throw "Could not acquire the Atman installer lock at $Path. $_" }
      Start-Sleep -Milliseconds 50
    }
  }
}

function Clear-AtmanRetiredExecutables {
  param([string]$Directory, [string]$FileName)

  $pattern = '^\.' + [regex]::Escape($FileName) + '\.atman-retired-[0-9a-f]{32}\.exe$'
  try {
    foreach ($candidate in [System.IO.Directory]::EnumerateFiles($Directory, ".$FileName.atman-retired-*")) {
      if ([System.IO.Path]::GetFileName($candidate) -cnotmatch $pattern) { continue }
      try {
        if ([System.IO.File]::GetAttributes($candidate) -band [System.IO.FileAttributes]::ReparsePoint) { continue }
        # Acquiring exclusive write access fails while an executing process maps this image.
        $unmapped = [System.IO.FileStream]::new(
          $candidate, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite,
          [System.IO.FileShare]::None, 4096, [System.IO.FileOptions]::DeleteOnClose)
        $unmapped.Dispose()
      } catch [System.IO.IOException], [System.UnauthorizedAccessException] {
        Write-Verbose "Retaining $candidate until Windows releases it."
      }
    }
  } catch [System.IO.IOException], [System.UnauthorizedAccessException] {
    Write-Verbose "Could not sweep retired Atman executables in $Directory."
  }
}

function Install-AtmanExecutable {
  [CmdletBinding()]
  param(
    [Parameter(Mandatory, Position = 0)][string]$Source,
    [Parameter(Mandatory, Position = 1)][string]$Destination
  )

  $sourcePath = [System.IO.Path]::GetFullPath($Source)
  $destinationPath = [System.IO.Path]::GetFullPath($Destination)
  $directory = [System.IO.Path]::GetDirectoryName($destinationPath)
  $name = [System.IO.Path]::GetFileName($destinationPath)
  $id = [guid]::NewGuid().ToString('N')
  $staged = [System.IO.Path]::Combine($directory, ".$name.atman-stage-$id.tmp")
  $retired = [System.IO.Path]::Combine($directory, ".$name.atman-retired-$id.exe")
  $lock = Open-AtmanInstallLock ([System.IO.Path]::Combine($directory, ".$name.atman-upgrade.lock"))
  $ownStage = $false
  $movedOriginal = $false
  try {
    # Fully prepare the new image before moving the old executable out of the way.
    $incoming = [System.IO.File]::Open($sourcePath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    try {
      if ($incoming.ReadByte() -ne 0x4d -or $incoming.ReadByte() -ne 0x5a) {
        throw "The downloaded file is not a Windows executable: $sourcePath"
      }
      $incoming.Position = 0
      $output = [System.IO.File]::Open($staged, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
      $ownStage = $true
      try {
        $incoming.CopyTo($output)
        $output.Flush($true)
      } finally {
        $output.Dispose()
      }
    } finally {
      $incoming.Dispose()
    }

    if ([System.IO.File]::Exists($destinationPath)) {
      if ([System.IO.File]::GetAttributes($destinationPath) -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "Cannot replace an Atman executable installed through a link: $destinationPath"
      }
      # Renaming an image on the same volume leaves existing mappings on the old file.
      [System.IO.File]::Move($destinationPath, $retired)
      $movedOriginal = $true
    }
    try {
      [System.IO.File]::Move($staged, $destinationPath)
      $ownStage = $false
    } catch {
      $publication = $_
      if ($movedOriginal) {
        try {
          [System.IO.File]::Move($retired, $destinationPath)
        } catch {
          throw "Could not install $destinationPath. The prior executable is at $retired. Publication: $publication. Restore: $_"
        }
      }
      throw $publication
    }

    Clear-AtmanRetiredExecutables $directory $name
  } finally {
    try {
      if ($ownStage) { [System.IO.File]::Delete($staged) }
    } catch [System.IO.IOException], [System.UnauthorizedAccessException] {
      Write-Verbose "Could not remove the unpublished stage $staged."
    } finally {
      $lock.Dispose()
    }
  }
}
