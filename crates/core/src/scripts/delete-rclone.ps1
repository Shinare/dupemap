<#
  DupeMap delete script for the cloud drive "@@HOST@@" (an rclone remote), made by DupeMap
  @@VERSION@@ on @@MADE@@.

  It removes @@COUNT_TEXT@@ (@@BYTES_TEXT@@) that you marked for deletion in DupeMap, with rclone.
@@LAST_NOTE@@
  DupeMap chose them from the manifests it had imported; if files changed since, scan again
  first. Nothing is touched before you confirm.

  It needs rclone (on PATH, or rclone.exe next to this script) with the remote "@@HOST@@" set up,
  as for the cloud scan. Run it:
    right-click this file > Run with PowerShell, or
    powershell -ExecutionPolicy Bypass -File .\@@FILE_NAME@@ [options]

  It checks every file first (still there, same size), shows what it will do and asks before
  it starts. Options:
    -DryRun      only check, and show what would happen
    -Permanent   delete them for good instead of into the trash@@DEFAULT_PERMANENT@@
    -Yes         do not ask anything (for running it unattended)
  Google Drive and OneDrive keep removed files in their trash / recycle bin (about 30 days),
  where you can restore them; some other kinds of cloud storage delete for good.
  Google Drive allows two files with the same path: rclone removes every file with a listed path.
  Everything it does is written to @@LOG_NAME@@ next to this script.
#>
param(
  [switch]$DryRun,
  [switch]$Permanent = $@@PERMANENT@@,
  [switch]$Yes
)

$ErrorActionPreference = 'Stop'
$remote = '@@HOST_PS@@'
# size <TAB> path from the top of the remote (an empty size: not known)
$list = @'
@@LIST@@
'@

$base = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
$logPath = Join-Path $base '@@LOG_NAME@@'
$utf8 = [System.Text.UTF8Encoding]::new($false)
$logWriter = $null
function Log([string]$what, [string]$path) {
  if (-not $script:logWriter) {
    $script:logWriter = [System.IO.StreamWriter]::new($logPath, $true, $utf8)
    $script:logWriter.AutoFlush = $true
  }
  $script:logWriter.WriteLine(("{0}`t{1}`t{2}" -f [DateTime]::Now.ToString('yyyy-MM-dd HH:mm:ss'), $what, $path))
}
function Close-Window {
  if (-not $Yes) { [void](Read-Host 'Press Enter to close') }
}
function Stop-Here([string]$why) {
  Write-Host ''
  Write-Host $why -ForegroundColor Red
  Log 'stopped' $why
  Close-Window
  exit 1
}

$rclone = 'rclone'
if (-not (Get-Command rclone -ErrorAction SilentlyContinue)) {
  $local = Join-Path $base 'rclone.exe'
  if (Test-Path -LiteralPath $local) { $rclone = $local } else { Stop-Here 'rclone was not found: put rclone.exe next to this script or on PATH.' }
}
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$entries = New-Object System.Collections.Generic.List[object]
foreach ($line in ($list -split "`r?`n")) {
  if (-not $line) { continue }
  $p = $line.Split("`t", 2)
  if ($p.Count -lt 2) { continue }
  $size = -1L
  if ($p[0]) { $size = [long]$p[0] }
  $entries.Add([pscustomobject]@{ Size = $size; Path = $p[1] })
}
$dry = ''
if ($DryRun) { $dry = ', dry run' }
Log 'start' ("{0} files on {1}{2}" -f $entries.Count, $remote, $dry)

# ---- check every file: is it there, with the same size? ------------------------------------------
$all = Join-Path $env:TEMP ("dupemap-delete-{0}-all.txt" -f $PID)
$todoFile = Join-Path $env:TEMP ("dupemap-delete-{0}.txt" -f $PID)
try {
  [System.IO.File]::WriteAllText($all, ((@($entries | ForEach-Object { $_.Path }) -join "`n") + "`n"), $utf8)
  Write-Host ("Checking {0} files on {1} (rclone lists the remote; this can take a while)..." -f $entries.Count, $remote)
  $found = [System.Collections.Generic.Dictionary[string, long]]::new([StringComparer]::Ordinal)
  $out = & $rclone lsf $remote -R --files-only --files-from-raw $all --format sp --separator "`t" 2>$null
  if ($LASTEXITCODE -ne 0) { Stop-Here ("rclone could not list {0} (exit code {1}). Check the remote with: rclone lsd {0}" -f $remote, $LASTEXITCODE) }
  foreach ($l in $out) {
    $p = "$l".Split("`t", 2)
    if ($p.Count -eq 2) { $found[$p[1]] = [long]$p[0] }
  }
  $todo = New-Object System.Collections.Generic.List[string]
  $todoBytes = 0L
  $missing = 0
  $changed = 0
  foreach ($e in $entries) {
    if (-not $found.ContainsKey($e.Path)) { $missing++; Log 'not there (left out)' $e.Path; continue }
    $now = $found[$e.Path]
    if ($e.Size -ge 0 -and $now -ne $e.Size) {
      $changed++
      Log ("another size: {0} bytes, was {1} (left out)" -f $now, $e.Size) $e.Path
      continue
    }
    $todo.Add($e.Path)
    $todoBytes += $now
  }
  $verb = 'Move to the trash'
  if ($Permanent) { $verb = 'Delete for good' }
  Write-Host ''
  Write-Host ("{0}: {1} files, {2:N1} GB on {3}" -f $verb, $todo.Count, ($todoBytes / 1GB), $remote)
  if ($missing) { Write-Host "  $missing are not there any more (left out)" }
  if ($changed) { Write-Host "  $changed have another size than when they were scanned (left out: they changed)" }
  if ($DryRun) {
    $shown = 0
    foreach ($t in $todo) {
      if ($shown -lt 20) { Write-Host "  $t" }
      $shown++
      Log 'would remove' $t
    }
    if ($todo.Count -gt 20) { Write-Host ("  ... and {0} more (all of them are in {1})" -f ($todo.Count - 20), $logPath) }
    Write-Host 'Dry run: nothing was changed.'
    Close-Window
    exit 0
  }
  if ($todo.Count -eq 0) { Write-Host 'Nothing to do.'; Close-Window; exit 0 }
  if (-not $Yes) {
    $answer = Read-Host ("Type YES to go ahead ({0} files)" -f $todo.Count)
    if ($answer -ne 'YES') { Write-Host 'Nothing was changed.'; Close-Window; exit 0 }
  }
  [System.IO.File]::WriteAllText($todoFile, (($todo -join "`n") + "`n"), $utf8)
  $flags = @('delete', $remote, '--files-from-raw', $todoFile, '-v', '--log-file', $logPath)
  if ($Permanent) { $flags += @('--drive-use-trash=false', '--onedrive-hard-delete') }
  Log 'rclone' ($flags -join ' ')
  # rclone writes to the same log: let go of it meanwhile
  if ($logWriter) { $logWriter.Close(); $script:logWriter = $null }
  & $rclone @flags
  $code = $LASTEXITCODE
  Write-Host ''
  if ($code -eq 0) {
    Write-Host ("Done: {0} files, {1:N1} GB." -f $todo.Count, ($todoBytes / 1GB))
    if (-not $Permanent) { Write-Host 'Google Drive and OneDrive keep them in their trash / recycle bin for a while: empty it to free the space.' }
  } else {
    Write-Host ("rclone ended with exit code {0}: some files may not have been removed (see the log)." -f $code) -ForegroundColor Red
  }
  Write-Host "Log: $logPath"
} finally {
  Remove-Item -LiteralPath $all, $todoFile -ErrorAction SilentlyContinue
}
Close-Window
