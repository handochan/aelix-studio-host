# Provision real NTFS and SMB fixtures on the disposable hosted Windows runner.
$ErrorActionPreference = 'Stop'
if ($env:HOST60_RUNNER_ENVIRONMENT -ne 'github-hosted') { throw 'These machine-level fixtures require a disposable GitHub-hosted runner.' }
# Run the normal contract tests even if provisioning an integration fixture fails.
cargo test --locked -p aionui-common --lib chat_attachment:: -- --nocapture
$basicExit = $LASTEXITCODE
if ($basicExit -ne 0) { Write-Warning 'Basic Windows attachment tests failed; collecting integration results.' }

$base = Join-Path (Join-Path $env:GITHUB_WORKSPACE target) ("h60-" + [guid]::NewGuid().ToString('N'))
$share = Split-Path -Leaf $base
New-Item -ItemType Directory -Path $base -Force | Out-Null
$letter = (90..68 | ForEach-Object { [char]$_ } | Where-Object { -not (Get-PSDrive -Name $_ -ErrorAction SilentlyContinue) } | Select-Object -First 1)
if (-not $letter) { throw 'No drive letter available for the SMB fixture.' }
$driveName = [string]$letter + ':'
$mapped = $false
$shared = $false
$substCreated = $false
$vhd = Join-Path $base 'mount-fixture.vhd'
$vhdAttached = $false
$mount = Join-Path (Join-Path $base 'readable') 'mounted'
try {
    $account = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    New-SmbShare -Name $share -Path $base -FullAccess $account | Out-Null
    $shared = $true
    net.exe use $driveName "\\localhost\$share" /persistent:no
    if ($LASTEXITCODE -ne 0) { throw 'Loopback SMB mapping failed.' }
    $mapped = $true
    [IO.File]::WriteAllBytes((Join-Path $base 'mapped.txt'), [Text.Encoding]::ASCII.GetBytes('mapped-file'))
    $env:HOST60_MAPPED_FILE = $driveName + '\mapped.txt'
    $env:HOST60_LINK_TO_MAPPED_FILE = Join-Path $base 'link-to-mapped'

    $volume = [IO.Path]::GetPathRoot($base).TrimEnd('\')
    fsutil.exe 8dot3name set 2
    if ($LASTEXITCODE -ne 0) { throw 'Cannot enable per-volume short-name configuration.' }
    fsutil.exe 8dot3name set $volume 0
    if ($LASTEXITCODE -ne 0) { throw "Cannot enable 8.3 aliases on $volume." }
    $env:HOST60_LONG_FILE = Join-Path $base 'Long Attachment Name.txt'
    [IO.File]::WriteAllText($env:HOST60_LONG_FILE, 'short name fixture')
    fsutil.exe file setshortname $env:HOST60_LONG_FILE H60SHORT.TXT
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the distinct H60SHORT.TXT alias.' }
    $env:HOST60_SHORT_FILE = Join-Path $base 'H60SHORT.TXT'
    if (-not [IO.File]::Exists($env:HOST60_SHORT_FILE)) { throw '8.3 alias lookup failed.' }

    $env:HOST60_ACL_FIXTURES = $base
    $userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $worldSid = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
    $adminSid = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    foreach ($kind in 'readable', 'writable', 'foreign') {
        $fixture = New-Item -ItemType Directory -Path (Join-Path $base $kind)
        $acl = [Security.AccessControl.DirectorySecurity]::new()
        $acl.SetAccessRuleProtection($true, $false)
        $acl.SetOwner($(if ($kind -eq 'foreign') { $adminSid } else { $userSid }))
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($userSid, 'FullControl', 'Allow'))
        $rights = if ($kind -eq 'writable') { 'Modify' } else { 'ReadAndExecute' }
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($worldSid, $rights, 'Allow'))
        Set-Acl -LiteralPath $fixture.FullName -AclObject $acl
    }

    $env:HOST60_SUBST_BASE = Join-Path $base 'subst-root'
    $real = Join-Path $env:HOST60_SUBST_BASE 'real'
    $prefix = Join-Path $env:HOST60_SUBST_BASE 'prefix'
    New-Item -ItemType Directory -Force -Path (Join-Path $real 'scope') | Out-Null
    New-Item -ItemType Junction -Path $prefix -Target $real | Out-Null
    $substLetter = (89..69 | ForEach-Object { [char]$_ } | Where-Object { -not (Get-PSDrive -Name $_ -ErrorAction SilentlyContinue) } | Select-Object -First 1)
    if (-not $substLetter) { throw 'No drive letter available for SUBST.' }
    $substName = [string]$substLetter + ':'
    subst.exe $substName (Join-Path $prefix 'scope')
    if ($LASTEXITCODE -ne 0) { throw 'Real SUBST fixture failed.' }
    $substCreated = $true
    $env:HOST60_SUBST_DRIVE = $substName + '\'

    $systemTemp = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Temp'
    $env:HOST60_FILTER_BASE = Join-Path $systemTemp $share
    New-Item -ItemType Directory -Path $env:HOST60_FILTER_BASE | Out-Null
    $env:HOST60_WOF_REFERENCE = Join-Path $base 'reference.bin'
    $env:HOST60_WOF_FILE = Join-Path $env:HOST60_FILTER_BASE 'fixture.exe'
    # Use the system NTFS volume for XPRESS4K; D: on the hosted image rejects compression.
    $bytes = [IO.File]::ReadAllBytes((Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'))
    [IO.File]::WriteAllBytes($env:HOST60_WOF_REFERENCE, $bytes)
    [IO.File]::WriteAllBytes($env:HOST60_WOF_FILE, $bytes)
    compact.exe /c /f /exe:xpress4k $env:HOST60_WOF_FILE
    if ($LASTEXITCODE -ne 0) { throw 'Required XPRESS4K compression failed.' }

    # Prefer an unlettered volume so the real target cannot be addressed by
    # a DOS drive name. Only initialize the newly created, identified RAW VHD.
    try {
        $scriptFile = Join-Path $base 'create-vhd.txt'
        @(('create vdisk file="{0}" maximum=64 type=expandable' -f $vhd), 'exit') | Set-Content -LiteralPath $scriptFile -Encoding ascii
        $createdDisk = diskpart.exe /s $scriptFile
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $vhd)) { throw "diskpart exit=$LASTEXITCODE $createdDisk" }
        $image = Mount-DiskImage -ImagePath $vhd -Access ReadWrite -NoDriveLetter -PassThru -ErrorAction Stop
        $vhdAttached = $true
        $disk = $image | Get-Disk -ErrorAction Stop
        if ($image.ImagePath -ne $vhd -or $disk.Count -ne 1 -or $disk.IsBoot -or $disk.IsSystem -or
            $disk.Size -ne 64MB -or $disk.PartitionStyle -ne 'RAW') { throw 'Unexpected identity or geometry of private VHD.' }
        $disk | Initialize-Disk -PartitionStyle GPT -ErrorAction Stop
        $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -ErrorAction Stop
        Set-Partition -InputObject $part -NoDefaultDriveLetter $true -ErrorAction Stop
        Format-Volume -Partition $part -FileSystem NTFS -AllocationUnitSize 4096 -NewFileSystemLabel H60TEST -Confirm:$false -ErrorAction Stop | Out-Null
        $vol = $part | Get-Volume -ErrorAction Stop
        if ($vol.DriveLetter -or -not $vol.Path) { throw 'Test VHD requires a GUID and no drive letter.' }
        $env:HOST60_VOLUME_GUID = $vol.Path
        $env:HOST60_VOLUME_RELATIVE = 'owned-fixture'
        Write-Host "Using drive-letterless NTFS fixture $($vol.Path)"
    } catch {
        Write-Host "::notice::Private VHD unavailable: $($_.Exception.Message); using the existing second local volume."
        $targetDrive = [IO.Path]::GetPathRoot($systemTemp)
        if ($targetDrive -eq [IO.Path]::GetPathRoot($mount)) { throw "No distinct local fixture volume ($targetDrive)." }
        $guidOutput = mountvol.exe $targetDrive /L
        if ($LASTEXITCODE -ne 0) { throw "mountvol /L exit=$LASTEXITCODE $guidOutput" }
        $env:HOST60_VOLUME_GUID = ($guidOutput -join '').Trim()
        $env:HOST60_VOLUME_RELATIVE = [IO.Path]::GetRelativePath($targetDrive, $env:HOST60_FILTER_BASE)
    }
    cargo test --locked -p aionui-common --lib chat_attachment:: -- --ignored --nocapture
    $integrationsExit = $LASTEXITCODE
    if ($integrationsExit -ne 0) { Write-Warning 'Mandatory mapped-drive, alias or ACL checks failed.' }
    if ($integrationsExit -eq 0) {
        $created = Get-Acl -LiteralPath (Join-Path $base 'created-by-host')
        $rules = $created.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
        if ($created.GetOwner([Security.Principal.SecurityIdentifier]) -ne $userSid -or
            -not $created.AreAccessRulesProtected -or $rules.Count -ne 1 -or
            $rules[0].IdentityReference -ne $userSid) { throw 'New upload root does not have the protected owner-only ACL.' }
    }

    if ($basicExit -ne 0 -or $integrationsExit -ne 0) { throw 'Windows attachment provenance tests failed.' }
} finally {
    $mountItem = Get-Item -LiteralPath $mount -Force -ErrorAction SilentlyContinue
    if ($mountItem -and ($mountItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        mountvol.exe $mount /D
        if ($LASTEXITCODE -ne 0) { throw 'Cannot detach fixture mount; leaving its parent untouched.' }
    }
    if ($vhdAttached) { Dismount-DiskImage -ImagePath $vhd -ErrorAction Stop }
    if ($substCreated) { subst.exe $substName /D }
    if ($env:HOST60_LINK_TO_MAPPED_FILE) { [IO.File]::Delete($env:HOST60_LINK_TO_MAPPED_FILE) }
    if ($mapped) { net.exe use $driveName /delete /y }
    if ($shared) { Remove-SmbShare -Name $share -Force }
    Remove-Item -LiteralPath $base -Recurse -Force
}
