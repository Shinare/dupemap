<#
  DupeMap delete script for "@@HOST@@", made by DupeMap @@VERSION@@ on @@MADE@@.

  It removes @@COUNT_TEXT@@ (@@BYTES_TEXT@@) that you marked for deletion in DupeMap.
@@LAST_NOTE@@
  DupeMap chose them from the manifests it had imported; if files changed since, scan again
  first. Nothing is touched before you confirm.

  Run it on the computer that has this drive:
    right-click this file > Run with PowerShell, or
    powershell -ExecutionPolicy Bypass -File .\@@FILE_NAME@@ [options]

  It checks every file first (still there, same size@@HASH_NOTE@@), shows what it will do and
  asks before it starts. Options:
    -DryRun            only check, and show what would happen
    -Mode RecycleBin   move the files to the Recycle Bin@@DEFAULT_RECYCLE@@
    -Mode Move         move them into "_DupeMap deleted\<date>" at the top of their drive@@DEFAULT_MOVE@@
    -Mode Delete       delete them for good@@DEFAULT_DELETE@@
    -CheckHash         also check each file's SHA-256 first (reads every file, so it is slow)@@HASH_DEFAULT@@
    -Drive G:          the drive has another letter now
    -Yes               do not ask anything (for running it unattended)
  Drives without a Recycle Bin (network drives, USB sticks) get -Mode Move instead.
  Everything it does is written to @@LOG_NAME@@ next to this script.
#>
param(
  [switch]$DryRun,
  [ValidateSet('RecycleBin', 'Move', 'Delete')][string]$Mode = '@@MODE@@',
  [switch]$CheckHash = $@@CHECK_HASH@@,
  [string]$Drive,
  [switch]$Yes
)

$ErrorActionPreference = 'Stop'
$source = '@@HOST_PS@@'
# size <TAB> SHA-256 <TAB> path (an empty size or hash: not known)
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
# \\?\ lets Windows reach paths longer than 260 characters
function LongPath([string]$p) {
  if ($p.StartsWith('\\?\')) { return $p }
  if ($p.StartsWith('\\')) { return '\\?\UNC\' + $p.Substring(2) }
  return '\\?\' + $p
}

# ---- the files -----------------------------------------------------------------------------
$entries = New-Object System.Collections.Generic.List[object]
foreach ($line in ($list -split "`r?`n")) {
  if (-not $line) { continue }
  $p = $line.Split("`t", 3)
  if ($p.Count -lt 3) { continue }
  $size = -1L
  if ($p[0]) { $size = [long]$p[0] }
  $entries.Add([pscustomobject]@{ Size = $size; Hash = $p[1]; Path = $p[2] })
}
$dry = ''
if ($DryRun) { $dry = ', dry run' }
Log 'start' ("{0} files, mode {1}{2}" -f $entries.Count, $Mode, $dry)

# ---- the right drive? ------------------------------------------------------------------------
$m = [regex]::Match($source, '^(?<pc>.+?) (?<letter>[A-Za-z]):(?: \((?<label>.*)\))?$')
if ($m.Success) {
  $letter = $m.Groups['letter'].Value.ToUpper() + ':'
  $label = $m.Groups['label'].Value
  $now = $letter
  if ($Drive) {
    $now = $Drive.Trim().Substring(0, 1).ToUpper() + ':'
    foreach ($e in $entries) {
      if ($e.Path.StartsWith($letter, [StringComparison]::OrdinalIgnoreCase)) { $e.Path = $now + $e.Path.Substring(2) }
    }
  }
  $disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$now'" -ErrorAction SilentlyContinue
  if (-not $disk) {
    Stop-Here "There is no drive $now on this computer. Connect the drive, or run this with -Drive <letter> if it has another letter now."
  }
  if ($label -and $disk.VolumeName -ne $label) {
    Stop-Here ("Drive {0} is called '{1}', but these files were scanned on '{2}'. Connect the right drive, or run this with -Drive <letter> if it has another letter now." -f $now, $disk.VolumeName, $label)
  }
  if ($m.Groups['pc'].Value -ne $env:COMPUTERNAME) {
    Write-Warning ("These files were scanned on {0}; this computer is {1}. That is fine for a drive you moved here." -f $m.Groups['pc'].Value, $env:COMPUTERNAME)
  }
  if ($Mode -eq 'RecycleBin' -and ($disk.DriveType -eq 4 -or $disk.DriveType -eq 2)) {
    Write-Host "Drive $now has no Recycle Bin (a network drive or USB stick): the files are moved into '_DupeMap deleted' on it instead."
    $Mode = 'Move'
  }
}
if ($Mode -eq 'RecycleBin' -and [IntPtr]::Size -eq 4) {
  Stop-Here 'The Recycle Bin needs the 64-bit PowerShell: run this from the normal PowerShell, or with -Mode Move.'
}

# ---- check every file ------------------------------------------------------------------------
Write-Host ("Checking {0} files..." -f $entries.Count)
$todo = New-Object System.Collections.Generic.List[object]
$todoBytes = 0L
$missing = 0
$changed = 0
foreach ($e in $entries) {
  $fi = [System.IO.FileInfo]::new((LongPath $e.Path))
  if (-not $fi.Exists) { $missing++; Log 'not there (skipped)' $e.Path; continue }
  if ($e.Size -ge 0 -and $fi.Length -ne $e.Size) {
    $changed++
    Log ("another size: {0} bytes, was {1} (skipped)" -f $fi.Length, $e.Size) $e.Path
    continue
  }
  $todo.Add($e)
  $todoBytes += $fi.Length
}
$verb = switch ($Mode) {
  'RecycleBin' { 'Move to the Recycle Bin' }
  'Move' { "Move into '_DupeMap deleted'" }
  default { 'Delete for good' }
}
Write-Host ''
Write-Host ("{0}: {1} files, {2:N1} GB" -f $verb, $todo.Count, ($todoBytes / 1GB))
if ($missing) { Write-Host "  $missing are not there any more (left out)" }
if ($changed) { Write-Host "  $changed have another size than when they were scanned (left out: they changed)" }
if ($CheckHash) { Write-Host '  each file''s SHA-256 is checked first; a file that changed is left alone' }

if ($DryRun) {
  $shown = 0
  foreach ($e in $todo) {
    if ($shown -lt 20) { Write-Host "  $($e.Path)" }
    $shown++
    Log 'would remove' $e.Path
  }
  if ($todo.Count -gt 20) { Write-Host ("  ... and {0} more (all of them are in {1})" -f ($todo.Count - 20), $logPath) }
  Write-Host 'Dry run: nothing was changed.'
  Close-Window
  exit 0
}
if ($todo.Count -eq 0) {
  Write-Host 'Nothing to do.'
  Close-Window
  exit 0
}
if (-not $Yes) {
  $answer = Read-Host ("Type YES to go ahead ({0} files)" -f $todo.Count)
  if ($answer -ne 'YES') { Write-Host 'Nothing was changed.'; Close-Window; exit 0 }
}

# ---- do it ---------------------------------------------------------------------------------
if ($Mode -eq 'RecycleBin') {
  Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace DupeMap {
  public static class RecycleBin {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct SHFILEOPSTRUCT {
      public IntPtr hwnd;
      public uint wFunc;
      public string pFrom;
      public string pTo;
      public ushort fFlags;
      [MarshalAs(UnmanagedType.Bool)] public bool fAnyOperationsAborted;
      public IntPtr hNameMappings;
      public string lpszProgressTitle;
    }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHFileOperation(ref SHFILEOPSTRUCT op);
    // 0 when the file went to the Recycle Bin
    public static int Send(string path) {
      SHFILEOPSTRUCT op = new SHFILEOPSTRUCT();
      op.wFunc = 3;               // FO_DELETE
      op.pFrom = path + "\0";     // the list ends with two NULs; marshalling adds one
      op.fFlags = 0x0040 | 0x0010 | 0x0400 | 0x0004;   // ALLOWUNDO NOCONFIRMATION NOERRORUI SILENT
      int r = SHFileOperation(ref op);
      if (r == 0 && op.fAnyOperationsAborted) { return -1; }
      return r;
    }
  }
}
'@
}
$stamp = Get-Date -Format 'yyyy-MM-dd HHmm'
$sha = [System.Security.Cryptography.SHA256]::Create()
$done = 0
$doneBytes = 0L
$failed = 0
$hashChanged = 0
$i = 0
foreach ($e in $todo) {
  $i++
  if ($i % 100 -eq 0) {
    Write-Progress -Activity $verb -Status ("{0} of {1}" -f $i, $todo.Count) -PercentComplete ($i * 100 / $todo.Count)
  }
  $lp = LongPath $e.Path
  try {
    if ($CheckHash -and $e.Hash) {
      $fs = [System.IO.File]::OpenRead($lp)
      try { $h = [BitConverter]::ToString($sha.ComputeHash($fs)).Replace('-', '').ToLower() } finally { $fs.Dispose() }
      if ($h -ne $e.Hash) { $hashChanged++; Log 'another SHA-256 (left alone)' $e.Path; continue }
    }
    $len = ([System.IO.FileInfo]::new($lp)).Length
    $how = $Mode
    if ($how -eq 'RecycleBin' -and $e.Path.StartsWith('\\')) { $how = 'Move' }   # shares have no Recycle Bin
    if ($how -eq 'RecycleBin') {
      $r = [DupeMap.RecycleBin]::Send($e.Path)
      if ($r -ne 0) { throw ("the Recycle Bin did not take it (code {0}); paths over 260 characters cannot go there: use -Mode Move" -f $r) }
    } elseif ($how -eq 'Move') {
      $t = [regex]::Match($e.Path, '^(?<root>[A-Za-z]:\\|\\\\[^\\]+\\[^\\]+\\)(?<rest>.+)$')
      if (-not $t.Success) { throw 'could not tell the top of its drive' }
      $root = $t.Groups['root'].Value
      if ($env:DUPEMAP_MOVE_ROOT) { $root = $env:DUPEMAP_MOVE_ROOT.TrimEnd('\') + '\' }   # (another top only in DupeMap's tests)
      $dest = $root + '_DupeMap deleted\' + $stamp + '\' + $t.Groups['rest'].Value
      [void][System.IO.Directory]::CreateDirectory((LongPath $dest.Substring(0, $dest.LastIndexOf('\'))))
      [System.IO.File]::Move($lp, (LongPath $dest))
    } else {
      $fi = [System.IO.FileInfo]::new($lp)
      if ($fi.IsReadOnly) { $fi.IsReadOnly = $false }
      $fi.Delete()
    }
    $done++
    $doneBytes += $len
    Log $how $e.Path
  } catch {
    $failed++
    Log ("failed: {0}" -f $_.Exception.Message) $e.Path
    Write-Warning ("Could not remove {0}: {1}" -f $e.Path, $_.Exception.Message)
  }
}
Write-Progress -Activity $verb -Completed
Write-Host ''
Write-Host ("Done: {0} files, {1:N1} GB." -f $done, ($doneBytes / 1GB))
if ($Mode -eq 'RecycleBin') { Write-Host 'They are in the Recycle Bin: empty it to free the space, or restore anything from there.' }
if ($Mode -eq 'Move') { Write-Host "They are in '_DupeMap deleted' at the top of the drive: look through them, then delete that folder to free the space." }
if ($hashChanged) { Write-Host "$hashChanged had another SHA-256 than when they were scanned (they changed) and were left alone." }
if ($failed) { Write-Host "$failed could not be removed (see the log)." }
Write-Host "Log: $logPath"
Log 'done' ("{0} files, {1} bytes; {2} failed, {3} changed" -f $done, $doneBytes, $failed, $hashChanged)
Close-Window
