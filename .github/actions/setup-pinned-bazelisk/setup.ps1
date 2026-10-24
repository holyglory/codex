$ErrorActionPreference = 'Stop'

$version = '1.28.1'
$platforms = @{ Windows = 'windows'; Linux = 'linux'; macOS = 'darwin' }
$architectures = @{ X64 = 'amd64'; ARM64 = 'arm64' }
if (-not $platforms.ContainsKey($env:RUNNER_OS) -or -not $architectures.ContainsKey($env:RUNNER_ARCH)) {
    throw "Unsupported Bazelisk host: $env:RUNNER_OS/$env:RUNNER_ARCH"
}
$asset = "bazelisk-$($platforms[$env:RUNNER_OS])-$($architectures[$env:RUNNER_ARCH])"
$executable = 'bazel'
if ($env:RUNNER_OS -eq 'Windows') {
    $asset += '.exe'
    $executable += '.exe'
}
# SHA256 digests published for the v1.28.1 release assets.
$digests = @{
    'bazelisk-darwin-amd64' = '023225736cea5dc88f2b0807d5b1af4eb0f69a4ed45e3994b2c18c263bc80e48'
    'bazelisk-darwin-arm64' = 'dea3f3f5de2dbc5e269e0132cdd369d5efe738f7b973d5d4eb2b4f7055a97b39'
    'bazelisk-linux-amd64' = '22e7d3a188699982f661cf4687137ee52d1f24fec1ec893d91a6c4d791a75de8'
    'bazelisk-linux-arm64' = '8ded44b58a0d9425a4178af26cf17693feac3b87bdcfef0a2a0898fcd1afc9f2'
    'bazelisk-windows-amd64.exe' = 'b9d65a1f7c2d7af885a96a4fd5aa36b40fb41816d30944390569eef908bdc954'
    'bazelisk-windows-arm64.exe' = '85ba3d92a8bdcbecc06657b8c0ae30f4307b552d601d9d6246f8a98aec36c346'
}
$expected = $digests[$asset]
if (-not $env:RUNNER_TEMP -or -not [IO.Path]::IsPathFullyQualified($env:RUNNER_TEMP) -or
    -not $env:GITHUB_PATH -or -not [IO.Path]::IsPathFullyQualified($env:GITHUB_PATH)) {
    throw 'GitHub runner temporary directory and PATH file are required.'
}
$directory = Join-Path $env:RUNNER_TEMP "codex-bazelisk-$version-$asset"
$null = New-Item -ItemType Directory -Path $directory -Force
$destination = Join-Path $directory $executable
if (-not (Test-Path -LiteralPath $destination -PathType Leaf) -or
    (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ine $expected) {
    $download = Join-Path $directory ([guid]::NewGuid().ToString('N') + '.download')
    try {
        $url = "https://github.com/bazelbuild/bazelisk/releases/download/v$version/$asset"
        Invoke-WebRequest -Uri $url -OutFile $download -MaximumRetryCount 3 -RetryIntervalSec 2 -TimeoutSec 120
        if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ine $expected) {
            throw "Bazelisk checksum mismatch for $asset"
        }
        Move-Item -LiteralPath $download -Destination $destination -Force
    } finally {
        if (Test-Path -LiteralPath $download) { Remove-Item -LiteralPath $download }
    }
}
if ($env:RUNNER_OS -ne 'Windows') {
    & chmod 755 $destination
    if ($LASTEXITCODE -ne 0) { throw 'Unable to make Bazelisk executable.' }
}
Add-Content -LiteralPath $env:GITHUB_PATH -Value $directory -Encoding utf8
Write-Host "Verified Bazelisk $version ($asset) at $destination"
