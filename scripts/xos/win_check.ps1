# Attach a flashcp VHD on Windows, chkdsk it read-only, verify it against the manifest,
# run a write test, chkdsk again, dismount.
#
#   pwsh scripts/xos/win_check.ps1 -Vhd vol.vhd -Manifest manifest.json -Label prefix-auto -LogDir logs [-Superfloppy]
#
# Exit 0 only if Windows mounts the volume, chkdsk reports no problems before and after the write
# test, and every file matches. -Superfloppy only changes the wording of the report.
param(
  [Parameter(Mandatory)] [string] $Vhd,
  [Parameter(Mandatory)] [string] $Manifest,
  [Parameter(Mandatory)] [string] $Label,
  [string] $LogDir = "logs",
  [switch] $Superfloppy
)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
New-Item -ItemType Directory -Force $LogDir | Out-Null
$Vhd = (Resolve-Path $Vhd).Path
$Manifest = (Resolve-Path $Manifest).Path
$summary = $env:GITHUB_STEP_SUMMARY
function Report([string] $line) {
  Write-Host $line
  if ($summary) { Add-Content -Path $summary -Value $line -Encoding utf8 }
}

function Run-Chkdsk([string] $letter, [string] $tag) {
  $log = Join-Path $LogDir "chkdsk-$Label-$tag.txt"
  # chkdsk without /f is read-only; on a mounted volume it reports problems but fixes nothing.
  $out = & chkdsk.exe "$($letter):" 2>&1 | Out-String
  $code = $LASTEXITCODE
  Set-Content -Path $log -Value $out -Encoding utf8
  Write-Host "----- chkdsk $($letter): ($tag), exit $code -----"
  Write-Host $out
  $clean = ($code -eq 0) -and ($out -match "found no problems")
  if (-not $clean) {
    Report "- ``$Label`` chkdsk ($tag): **FAIL** (exit $code)"
    Report '```'
    ($out -split "`r?`n" | Where-Object { $_.Trim() -ne "" } | Select-Object -Last 25) | ForEach-Object { Report $_ }
    Report '```'
  } else {
    Report "- ``$Label`` chkdsk ($tag): OK"
  }
  return $clean
}

$ok = $true
$letter = $null
$disk = $null
try {
  $img = Mount-DiskImage -ImagePath $Vhd -StorageType VHD -PassThru
  $disk = $img | Get-Disk
  Write-Host ($disk | Format-List Number, FriendlyName, PartitionStyle, OperationalStatus, IsOffline, IsReadOnly, Size | Out-String)
  if ($disk.IsOffline) { Set-Disk -Number $disk.Number -IsOffline $false }
  if ($disk.IsReadOnly) { Set-Disk -Number $disk.Number -IsReadOnly $false }

  # Wait for the volume to show up (Windows needs a moment after attach).
  $vol = $null
  for ($i = 0; $i -lt 30 -and -not $vol; $i++) {
    $parts = @(Get-Partition -DiskNumber $disk.Number -ErrorAction SilentlyContinue)
    if ($parts.Count -gt 0) {
      $p = $parts[0]
      if (-not $p.DriveLetter -or $p.DriveLetter -eq [char]0) {
        Add-PartitionAccessPath -DiskNumber $disk.Number -PartitionNumber $p.PartitionNumber -AssignDriveLetter -ErrorAction SilentlyContinue
      }
      $p = Get-Partition -DiskNumber $disk.Number -PartitionNumber $p.PartitionNumber
      if ($p.DriveLetter -and $p.DriveLetter -ne [char]0) { $vol = Get-Volume -DriveLetter $p.DriveLetter -ErrorAction SilentlyContinue }
    }
    if (-not $vol) { Start-Sleep -Seconds 1 }
  }
  Write-Host (Get-Partition -DiskNumber $disk.Number -ErrorAction SilentlyContinue | Format-Table -AutoSize | Out-String)
  Write-Host (Get-Volume | Format-Table -AutoSize | Out-String)
  if (-not $vol) {
    $kind = if ($Superfloppy) { "superfloppy (no partition table)" } else { "MBR" }
    Report "- ``$Label``: **Windows did not expose a volume** for the $kind disk (PartitionStyle=$($disk.PartitionStyle))"
    throw "no volume"
  }
  $letter = [string]$vol.DriveLetter
  Report "- ``$Label``: mounted as $($letter): FileSystem=$($vol.FileSystem) Label=$($vol.FileSystemLabel) Size=$($vol.Size) Health=$($vol.HealthStatus)"
  if ($vol.FileSystem -ne "exFAT") { Report "  - unexpected file system '$($vol.FileSystem)'"; $ok = $false }

  if (-not (Run-Chkdsk $letter "fresh")) { $ok = $false }

  python "$here/verify.py" $Manifest "$($letter):\" --label "windows:$Label" 2>&1 | Tee-Object -FilePath (Join-Path $LogDir "verify-$Label.txt")
  if ($LASTEXITCODE -ne 0) { Report "- ``$Label`` verify: **FAIL** (see verify-$Label.txt)"; $ok = $false } else { Report "- ``$Label`` verify: OK" }

  $after = Join-Path $LogDir "after-$Label.json"
  python "$here/write_test.py" "$($letter):\" $Manifest $after 2>&1 | Tee-Object -FilePath (Join-Path $LogDir "writetest-$Label.txt")
  if ($LASTEXITCODE -ne 0) { Report "- ``$Label`` write test: **FAIL**"; $ok = $false } else { Report "- ``$Label`` write test: OK" }

  # Report-only; mutates accents/ (see nfc_probe.py), so it runs after the write test.
  $probe = python "$here/nfc_probe.py" "$($letter):\" "windows:$Label" 2>&1
  $probe | Set-Content -Path (Join-Path $LogDir "nfcprobe-$Label.txt") -Encoding utf8
  $probe | ForEach-Object { Report "  - $_" }

  Write-VolumeCache -DriveLetter $letter

  if (-not (Run-Chkdsk $letter "after-write")) { $ok = $false }
}
catch {
  Write-Host "ERROR: $_"
  $ok = $false
}
finally {
  Dismount-DiskImage -ImagePath $Vhd -ErrorAction SilentlyContinue | Out-Null
}
if (-not $ok) { exit 1 }
