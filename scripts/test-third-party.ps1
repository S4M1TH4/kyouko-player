#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$root = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path $root ("target/legal-checks-" + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $fixture
foreach ($name in @('Cargo.toml', 'Cargo.lock', 'build.rs', 'build', 'src', 'vendor',
                    'LICENSE', 'THIRD_PARTY_NOTICES.md', 'licenses')) {
    Copy-Item -LiteralPath (Join-Path $root $name) -Destination $fixture -Recurse
}
$checker = Join-Path $PSScriptRoot 'check-third-party.ps1'
function Invoke-Checker([string[]]$Extra = @()) {
    $result = & pwsh -NoProfile -File $checker -ProjectRoot $fixture @Extra 2>&1
    return @{ Code = $LASTEXITCODE; Text = ($result -join "`n") }
}
$baseline = Invoke-Checker
if ($baseline.Code -ne 0) { throw "Clean inventory failed: $($baseline.Text)" }
$inventory = Get-Content -LiteralPath (Join-Path $fixture 'licenses/dependencies.json') -Raw |
    ConvertFrom-Json -AsHashtable
$cases = @(
    @{ Name = 'corrupt license'; File = 'licenses/crates/atomic-wait-1.1.0/LICENSE'; Error = 'License/source file changed' },
    @{ Name = 'missing MPL source'; File = 'licenses/sources/symphonia-core-0.5.5.crate'; Error = 'Required legal file is missing'; Remove = $true },
    @{ Name = 'missing compound notice'; File = 'licenses/crates/unicode-ident-1.0.26/LICENSE-UNICODE'; Error = 'Required legal file is missing'; Remove = $true },
    @{ Name = 'lockfile drift'; File = 'Cargo.lock'; Error = 'Cargo.lock changed' },
    @{ Name = 'vendor drift'; File = 'vendor/cpal/src/lib.rs'; Error = 'Vendored CPAL changed' }
)
foreach ($case in $cases) {
    $path = Join-Path $fixture $case.File
    $bytes = [IO.File]::ReadAllBytes($path)
    try {
        if ($case.ContainsKey('Remove') -and $case['Remove']) {
            Remove-Item -LiteralPath $path
        } else { [IO.File]::AppendAllText($path, "`n ") }
        $result = Invoke-Checker
        if ($result.Code -eq 0 -or $result.Text -notmatch $case.Error) {
            throw "Expected rejection for $($case.Name): $($result.Text)"
        }
        Write-Output "Rejected $($case.Name) as expected."
    } finally { [IO.File]::WriteAllBytes($path, $bytes) }
}
$result = Invoke-Checker @('-StageDirectory', 'target/staged')
if ($result.Code -ne 0) { throw "Staging failed: $($result.Text)" }
foreach ($file in $inventory.files.GetEnumerator()) {
    $staged = Join-Path $fixture ("target/staged/" + $file.Key)
    if ((Get-FileHash -LiteralPath $staged).Hash.ToLowerInvariant() -ne $file.Value) {
        throw "Staged hash differs: $($file.Key)"
    }
}
$result = Invoke-Checker @('-StageDirectory', 'target/staged')
if ($result.Code -eq 0 -or $result.Text -notmatch 'must be a new directory') {
    throw 'Staging overwrote an existing directory.'
}
$result = Invoke-Checker @('-StageDirectory', '../outside-legal-stage')
if ($result.Code -eq 0 -or $result.Text -notmatch 'must be a new directory') {
    throw 'Staging did not reject an out-of-scope path.'
}
Write-Output 'Verified complete staging, overwrite protection and path containment.'
Write-Output "Test fixtures retained under $fixture"
