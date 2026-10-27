<#
  DupeMap scan script for Windows, made by DupeMap @@VERSION@@.

  Hashes every file of at least @@MIN_SIZE_TEXT@@ on this computer's @@KINDS_TEXT@@ and writes a
  manifest DupeMap can import, next to this script:
    manifest-<COMPUTERNAME>-@@WHAT@@.tsv

  Run it as administrator, so it can read every folder. Either
    right-click this file > Run with PowerShell (it asks for administrator rights itself), or
    open Terminal (Admin) or Windows PowerShell (Admin) from the Start button's right-click menu:
      powershell -ExecutionPolicy Bypass -File .\@@FILE_NAME@@

  Then copy the manifest to the computer with DupeMap and import it there
  (File > Import manifests, or drag the file onto DupeMap's window).

  Options:
    -Paths "D:\,E:\Photos"   only these drives or folders instead of the ones chosen in DupeMap
    -MinSize 100KB           smaller files are not hashed (default @@MIN_SIZE_TEXT@@)
    -Fresh                   start over instead of continuing an interrupted run
    -IncludeCloudFolders     also hash the local copies in OneDrive and Google Drive folders

  An interrupted run (Ctrl+C, closing the window, a crash or a power cut) continues where it
  stopped when you run it again. Online-only OneDrive / Google Drive files are never downloaded:
  they are skipped. Windows, Program Files, ProgramData, AppData and the recycle bin are skipped
  too, as by DupeMap's own scan.
#>
param(
  [string]$Paths,
  [string]$Kinds = '@@KINDS@@',
  [long]$MinSize = @@MIN_SIZE@@,
  [switch]$IncludeCloudFolders,
  [switch]$Fresh,
  [switch]$NoElevate,
  [string]$Out
)

$base = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
$computer = $env:COMPUTERNAME

# ---- administrator rights ---------------------------------------------------------------
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin -and -not $NoElevate) {
  Write-Host "Asking for administrator rights (so every folder can be read)..."
  $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-NoExit', '-File', "`"$PSCommandPath`"")
  if ($Paths) { $argList += @('-Paths', "`"$Paths`"") }
  $argList += @('-Kinds', "`"$Kinds`"", '-MinSize', "$MinSize")
  if ($IncludeCloudFolders) { $argList += '-IncludeCloudFolders' }
  if ($Fresh) { $argList += '-Fresh' }
  if ($Out) { $argList += @('-Out', "`"$Out`"") }
  try {
    Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -ErrorAction Stop
    return
  } catch {
    Write-Warning "Continuing without administrator rights: folders only an administrator can read are skipped."
  }
}

# ---- what to scan -----------------------------------------------------------------------
$wanted = @($Kinds -split '[,;+ ]' | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ })
$disks = @(Get-CimInstance Win32_LogicalDisk)

# bus type per drive letter, to tell USB drives from internal ones
$bus = @{}
try {
  foreach ($p in @(Get-Partition -ErrorAction Stop | Where-Object { "$($_.DriveLetter)" -match '^[A-Za-z]$' })) {
    $d = Get-Disk -Number $p.DiskNumber -ErrorAction SilentlyContinue
    if ($d) { $bus["$($p.DriveLetter):".ToUpper()] = "$($d.BusType)" }
  }
} catch { }

function Get-DriveKind($disk) {
  switch ([int]$disk.DriveType) {
    4 { return 'network' }
    2 { return 'usb' }
    3 { if (@('USB', 'SD', 'MMC') -contains $bus[$disk.DeviceID.ToUpper()]) { return 'usb' } else { return 'internal' } }
    6 { return 'internal' }
    default { return $null }
  }
}

# Each root: where to read, what to write as the path, and the source (host) name.
$roots = @()
function Add-Root([string]$read, [string]$write, [string]$letter, [string]$label) {
  $script:roots += [pscustomobject]@{ Read = $read; Write = $write; Tag = "$computer $letter ($label)" }
}

if ($Paths) {
  $what = 'folders'
  foreach ($p in ($Paths -split ',')) {
    $p = $p.Trim()
    if (-not $p) { continue }
    if ($p -match '^[A-Za-z]:') {
      $letter = $p.Substring(0, 2).ToUpper()
      $label = ($disks | Where-Object { $_.DeviceID -eq $letter }).VolumeName
      Add-Root $p $p $letter $label
    } else {
      $script:roots += [pscustomobject]@{ Read = $p; Write = $p; Tag = "$computer $p" }
    }
  }
} else {
  $what = ($wanted -join '+')
  foreach ($d in $disks) {
    $k = Get-DriveKind $d
    if (-not $k -or $wanted -notcontains $k) { continue }
    if ($d.VolumeName -eq 'Google Drive') {
      Write-Host "Skipping $($d.DeviceID) (Google Drive for desktop: reading it would download every file)."
      continue
    }
    if ($k -ne 'network' -and -not (Test-Path -LiteralPath ($d.DeviceID + '\'))) { continue }   # no disk in the reader
    Add-Root ($d.DeviceID + '\') ($d.DeviceID + '\') $d.DeviceID $d.VolumeName
  }
  if ($wanted -contains 'network') {
    # Drives mapped in your normal session are invisible to a program run as administrator;
    # they are reached through their share and written with their letter.
    foreach ($m in @(Get-ChildItem 'HKCU:\Network' -ErrorAction SilentlyContinue)) {
      $letter = $m.PSChildName.ToUpper() + ':'
      if (@($disks | ForEach-Object { $_.DeviceID }) -contains $letter) { continue }
      $remote = (Get-ItemProperty -LiteralPath $m.PSPath -ErrorAction SilentlyContinue).RemotePath
      if ($remote -and (Test-Path -LiteralPath $remote)) { Add-Root ($remote.TrimEnd('\') + '\') ($letter + '\') $letter '' }
    }
  }
}
if (-not $roots) {
  Write-Warning "Nothing to scan: no drives of the chosen kinds ($($wanted -join ', ')) were found."
  return
}
if (-not $Out) { $Out = Join-Path $base "manifest-$computer-$what.tsv" }

# System folders and caches (as DupeMap's own scan); OneDrive / Google Drive folders unless asked.
$skip = '^[A-Za-z]:\\(Windows|Program Files|ProgramData|\$Recycle\.Bin|System Volume Information)|\\AppData\\'
if (-not $IncludeCloudFolders) { $skip += '|^[A-Za-z]:\\Users\\[^\\]+\\(OneDrive[^\\]*|Google Drive)\\' }

# ---- continue an interrupted run ----------------------------------------------------------
$done = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$append = $false
if ((Test-Path -LiteralPath $Out) -and -not $Fresh) {
  $fs = [System.IO.FileStream]::new($Out, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
  try {
    if ($fs.Length -gt 0) {
      $fs.Position = $fs.Length - 1
      if ($fs.ReadByte() -ne 10) {
        # the last line was cut off mid-write: remove it
        $chunk = New-Object byte[] 65536
        $pos = $fs.Length
        $cut = 0
        while ($pos -gt 0) {
          $len = [int][Math]::Min(65536, $pos)
          $pos -= $len
          $fs.Position = $pos
          $got = $fs.Read($chunk, 0, $len)
          $i = [Array]::LastIndexOf($chunk, [byte]10, $got - 1)
          if ($i -ge 0) { $cut = $pos + $i + 1; break }
        }
        $fs.SetLength($cut)
      }
    }
    $hasData = $fs.Length -gt 0
  } finally { $fs.Dispose() }
  if ($hasData) {
    $sr = [System.IO.StreamReader]::new($Out, [System.Text.Encoding]::UTF8)
    [void]$sr.ReadLine()
    while ($null -ne ($line = $sr.ReadLine())) {
      $p = $line.Split("`t", 4)
      if ($p.Count -eq 4 -and $p[0].Length -eq 64) { [void]$done.Add($p[3]) }
    }
    $sr.Close()
    $append = $true
    Write-Host ("Continuing: {0} files are already in {1}" -f $done.Count, (Split-Path $Out -Leaf))
  }
}

# ---- keep the computer awake while hashing --------------------------------------------------
Add-Type -Namespace DupeMap -Name Power -MemberDefinition '[DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint f);' -ErrorAction SilentlyContinue
[void][DupeMap.Power]::SetThreadExecutionState([uint32]2147483649)   # ES_CONTINUOUS | ES_SYSTEM_REQUIRED

$buffer = New-Object byte[] (4MB)
function Get-Sha256([string]$path) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  $fs = [System.IO.FileStream]::new($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
          [System.IO.FileShare]::ReadWrite, 1048576, [System.IO.FileOptions]::SequentialScan)
  try {
    while (($r = $fs.Read($buffer, 0, $buffer.Length)) -gt 0) { [void]$sha.TransformBlock($buffer, 0, $r, $null, 0) }
    [void]$sha.TransformFinalBlock($buffer, 0, 0)
    [BitConverter]::ToString($sha.Hash).Replace('-', '').ToLower()
  } finally { $fs.Dispose(); $sha.Dispose() }
}

$sw = [System.IO.StreamWriter]::new($Out, $append, [System.Text.UTF8Encoding]::new($false))
$sw.AutoFlush = $true    # every line reaches the disk: an interruption loses at most one file
if (-not $append) { $sw.WriteLine("hash`tsize`thost`tpath") }
$n = 0; $bytes = 0L; $failed = 0; $skippedCloud = 0; $resumed = 0
$started = Get-Date; $lastReport = Get-Date

try {
  foreach ($root in $roots) {
    Write-Host "Scanning $($root.Write) as '$($root.Tag)'"
    Get-ChildItem -LiteralPath $root.Read -Recurse -File -Force -ErrorAction SilentlyContinue |
      ForEach-Object {
        $f = $_
        # 0x1000 Offline, 0x40000 RecallOnOpen, 0x400000 RecallOnDataAccess: online-only cloud file
        if (([int]$f.Attributes -band 0x441000) -ne 0) { $script:skippedCloud++; return }
        if ($f.Length -lt $MinSize) { return }
        $path = $f.FullName
        if ($root.Write -ne $root.Read -and $path.StartsWith($root.Read, [StringComparison]::OrdinalIgnoreCase)) {
          $path = $root.Write + $path.Substring($root.Read.Length)
        }
        if ($path -match $skip -or $f.FullName -eq $Out) { return }
        if ($done.Contains($path)) { $script:resumed++; return }
        try {
          $h = Get-Sha256 $f.FullName
          $sw.WriteLine("$h`t$($f.Length)`t$($root.Tag)`t$path")
          $script:n++
          $script:bytes += $f.Length
        } catch {
          $script:failed++
          Write-Warning "Could not read: $path"
        }
        if (((Get-Date) - $script:lastReport).TotalSeconds -ge 30) {
          $script:lastReport = Get-Date
          $secs = ((Get-Date) - $started).TotalSeconds
          Write-Host ("  {0} files, {1:N1} GB hashed this run ({2:N0} MB/s)" -f $script:n, ($script:bytes / 1GB), ($script:bytes / 1MB / [Math]::Max($secs, 1)))
        }
      }
  }
} finally {
  $sw.Close()   # also on Ctrl+C: the manifest is always left complete up to its last line
  [void][DupeMap.Power]::SetThreadExecutionState([uint32]2147483648)
}

Write-Host ("Done: {0} files, {1:N1} GB this run -> {2}" -f $n, ($bytes / 1GB), $Out)
if ($resumed -gt 0) { Write-Host "$resumed files were already in the manifest from the earlier run." }
if ($failed -gt 0) { Write-Host "$failed files could not be read (in use or no permission)." }
if ($skippedCloud -gt 0) { Write-Host "$skippedCloud online-only cloud files were skipped (not downloaded)." }
Write-Host "Copy the manifest to the computer with DupeMap and import it there (File > Import manifests)."
