# Copy a pre-extracted MSI payload without calling the Windows Installer service.
# These file hashes were obtained by administrative extraction of the pinned
# pkgconf-x64-3.0.6.msi (SHA-256 below), not from an ambient pkgconf installation.
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Destination
)
$ErrorActionPreference = 'Stop'
if ($Target -ne 'x86_64-pc-windows-msvc') {
    throw "No pre-extracted pkgconf payload is pinned for $Target"
}
# MSI SHA-256: 5604cf25ef38bb6a09520cff25ae9f0ecd8c2443053b15e40df4ad1eae0e4405
$Hashes = @{
    'bomtool.exe' = '3dfca05f6b165c091f6854468acf8c72193bbd2f9305941219d285d82f48cd94'
    'pccritic.exe' = 'b932d06698fe9cb2ee36eb54d217a41740e297cd5972664cc4f4edc10ca0723e'
    'pkg-config.exe' = '9219d3ca22215c2f61094e663db358ea1718b177202d35c74df5abf4bb4831da'
    'pkgconf.exe' = '9219d3ca22215c2f61094e663db358ea1718b177202d35c74df5abf4bb4831da'
    'spdxtool.exe' = 'c71f7cd7e79082fd9b6c99964481b7eb1f2e842cb3410df1cdfc22e2c1225820'
}
$SourceItem = Get-Item -LiteralPath $Source -Force
if (-not $SourceItem.PSIsContainer) { throw 'pkgconf image must be a directory.' }
for ($Item = $SourceItem; $null -ne $Item; $Item = $Item.Parent) {
    if ($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'pkgconf image cannot pass through reparse points.' }
}
$Entries = @(Get-ChildItem -LiteralPath $SourceItem.FullName -Force)
if ($Entries.Count -ne $Hashes.Count) { throw 'pkgconf image has an unexpected file set.' }
foreach ($Entry in $Entries) {
    if ($Entry.PSIsContainer -or ($Entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -or -not $Hashes.ContainsKey($Entry.Name)) {
        throw "Unexpected pkgconf image entry: $($Entry.Name)"
    }
    if ((Get-FileHash -LiteralPath $Entry.FullName -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Hashes[$Entry.Name]) {
        throw "Pinned pkgconf payload mismatch: $($Entry.Name)"
    }
}
if (Test-Path -LiteralPath $Destination) { throw 'Native pkgconf destination must be fresh.' }
New-Item -ItemType Directory -Path $Destination | Out-Null
foreach ($Name in $Hashes.Keys) {
    $Copy = Join-Path $Destination $Name
    Copy-Item -LiteralPath (Join-Path $SourceItem.FullName $Name) -Destination $Copy
    # Verify the copy too: changes between the first hash and copying must fail
    # before the caller executes pkgconf or exposes the tree to Bazel.
    if ((Get-FileHash -LiteralPath $Copy -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Hashes[$Name]) {
        throw "Copied pkgconf payload mismatch: $Name"
    }
}
