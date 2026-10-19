# Dedicated Windows runners use an existing local build directory. Fail closed
# when it is invalid rather than falling through to hosted-runner provisioning.
if ($env:CODEX_WINDOWS_BUILD_ROOT) {
    if ($env:CODEX_WINDOWS_BUILD_ROOT -match "[\r\n]") {
        throw "CODEX_WINDOWS_BUILD_ROOT must be a single filesystem path."
    }

    $BuildRoot = Get-Item -LiteralPath $env:CODEX_WINDOWS_BUILD_ROOT -ErrorAction Stop
    if ($BuildRoot.PSProvider.Name -ne 'FileSystem' -or -not $BuildRoot.PSIsContainer) {
        throw "CODEX_WINDOWS_BUILD_ROOT must be an existing filesystem directory."
    }
    if ($BuildRoot.FullName -notmatch '^[A-Za-z]:\\.+') {
        throw "CODEX_WINDOWS_BUILD_ROOT must be a directory below a local drive root."
    }

    $BuildVolume = [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($BuildRoot.FullName))
    if ($BuildVolume.DriveType -ne [System.IO.DriveType]::Fixed) {
        throw "CODEX_WINDOWS_BUILD_ROOT must be on a fixed local volume."
    }

    for ($Parent = $BuildRoot; $null -ne $Parent; $Parent = $Parent.Parent) {
        if ($Parent.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "CODEX_WINDOWS_BUILD_ROOT must not traverse a reparse point."
        }
    }

    # Forward slashes also work with the Git Bash steps that consume this value.
    $BuildPath = $BuildRoot.FullName.Replace('\', '/')
    "CI_BUILD_ROOT=$BuildPath" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append
    Write-Output "Using pre-provisioned Windows build directory at $BuildPath"
    return
}

# Configure a fast drive for Windows CI jobs.
#
# GitHub-hosted Windows runners do not always expose a secondary D: volume. When
# they do not, create a Dev Drive VHD. CI depends on this path for its
# build directories where CI spends significant time doing I/O, so fail the
# job if no real Dev Drive is available.

function Test-DevDrive {
    param([string]$Drive)

    & fsutil devdrv query $Drive *> $null
    return $LASTEXITCODE -eq 0
}

function Invoke-BestEffort {
    param([scriptblock]$Script, [string]$Description)

    try {
        & $Script
    } catch {
        Write-Warning "$Description failed: $($_.Exception.Message)"
    }
}

if ((Test-Path "D:\") -and (Test-DevDrive "D:")) {
    Write-Output "Using existing Dev Drive at D:"
    $Drive = "D:"
} else {
    if (Test-Path "D:\") {
        Write-Output "Existing D: volume is not a Dev Drive; provisioning a new Dev Drive VHD."
    }

    try {
        $VhdPath = Join-Path $env:RUNNER_TEMP "codex-dev-drive.vhdx"
        $SizeBytes = 64GB

        if (Test-Path $VhdPath) {
            Remove-Item -Path $VhdPath -Force
        }

        New-VHD -Path $VhdPath -SizeBytes $SizeBytes -Dynamic -ErrorAction Stop | Out-Null
        $Mounted = Mount-VHD -Path $VhdPath -Passthru -ErrorAction Stop
        $Disk = $Mounted | Get-Disk -ErrorAction Stop
        $Disk | Initialize-Disk -PartitionStyle GPT -ErrorAction Stop
        $Partition = $Disk | New-Partition -AssignDriveLetter -UseMaximumSize -ErrorAction Stop
        $Volume = $Partition | Format-Volume -FileSystem ReFS -NewFileSystemLabel "CodexDevDrive" -DevDrive -Confirm:$false -Force -ErrorAction Stop

        $Drive = "$($Volume.DriveLetter):"

        if (-not (Test-DevDrive $Drive)) {
            throw "Provisioned volume at $Drive did not pass Dev Drive verification."
        }

        Invoke-BestEffort { fsutil devdrv trust $Drive } "Trusting Dev Drive $Drive"
        Invoke-BestEffort { fsutil devdrv enable /disallowAv } "Disabling AV filter attachment for Dev Drives"

        Write-Output "Using Dev Drive at $Drive"
    } catch {
        throw "Failed to create Dev Drive: $($_.Exception.Message)"
    }
}

"CI_BUILD_ROOT=$Drive" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append
