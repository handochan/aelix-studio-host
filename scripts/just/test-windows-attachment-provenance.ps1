# Provision real NTFS and SMB fixtures on the disposable hosted Windows runner.
$ErrorActionPreference = 'Stop'
if ($env:HOST60_RUNNER_ENVIRONMENT -ne 'github-hosted') { throw 'These machine-level fixtures require a disposable GitHub-hosted runner.' }
$base = Join-Path (Join-Path $env:GITHUB_WORKSPACE target) ("h60-" + [guid]::NewGuid().ToString('N'))
$share = Split-Path -Leaf $base
New-Item -ItemType Directory -Path $base -Force | Out-Null
$letter = (90..68 | ForEach-Object { [char]$_ } | Where-Object { -not (Get-PSDrive -Name $_ -ErrorAction SilentlyContinue) } | Select-Object -First 1)
if (-not $letter) { throw 'No drive letter available for the SMB fixture.' }
$driveName = [string]$letter + ':'
$mapped = $false
$shared = $false
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

    $env:HOST60_WOF_FILE = Join-Path $base 'wof.txt'
    [IO.File]::WriteAllText($env:HOST60_WOF_FILE, ('W' * 65536), [Text.Encoding]::ASCII)
    compact.exe /c /f /exe:lzx $env:HOST60_WOF_FILE
    if ($LASTEXITCODE -ne 0) { throw 'WOF compression failed.' }
    $tag = fsutil.exe reparsepoint query $env:HOST60_WOF_FILE
    if ($LASTEXITCODE -ne 0 -or ($tag -join ' ') -notmatch '0x80000017') { throw 'WOF non-name-surrogate tag not present.' }

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

    cargo test --locked -p aionui-common --lib chat_attachment:: -- --include-ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw 'Windows attachment provenance tests failed.' }
    $created = Get-Acl -LiteralPath (Join-Path $base 'created-by-host')
    $rules = $created.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    if ($created.GetOwner([Security.Principal.SecurityIdentifier]) -ne $userSid -or
        -not $created.AreAccessRulesProtected -or $rules.Count -ne 1 -or
        $rules[0].IdentityReference -ne $userSid) { throw 'New upload root does not have the protected owner-only ACL.' }
} finally {
    if ($env:HOST60_LINK_TO_MAPPED_FILE) { [IO.File]::Delete($env:HOST60_LINK_TO_MAPPED_FILE) }
    if ($mapped) { net.exe use $driveName /delete /y }
    if ($shared) { Remove-SmbShare -Name $share -Force }
    Remove-Item -LiteralPath $base -Recurse -Force
}
