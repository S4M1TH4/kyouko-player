#Requires -Version 7.2
[CmdletBinding()]
param(
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$StageDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ProjectRoot = (Resolve-Path -LiteralPath $ProjectRoot).Path
$rootPrefix = $ProjectRoot + [IO.Path]::DirectorySeparatorChar
function Get-ProjectPath([string]$Relative) {
    if ([IO.Path]::IsPathRooted($Relative)) { throw "Absolute inventory path: $Relative" }
    $path = [IO.Path]::GetFullPath((Join-Path $ProjectRoot $Relative))
    if (-not $path.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Inventory path escapes project: $Relative"
    }
    return $path
}
function Get-Hash([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Required legal file is missing: $Path" }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Get-TextHash([string]$Path) {
    $text = [IO.File]::ReadAllText($Path).Replace("`r`n", "`n")
    $bytes = [Text.Encoding]::UTF8.GetBytes($text)
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
}
function Assert-SameSet($Actual, $Expected, [string]$Label) {
    if ((@($Actual | Sort-Object) -join "`n") -cne (@($Expected | Sort-Object) -join "`n")) {
        throw "$Label differs from the reviewed license inventory; audit and update licenses/."
    }
}
$inventory = Get-Content -LiteralPath (Get-ProjectPath 'licenses/dependencies.json') -Raw |
    ConvertFrom-Json -AsHashtable
if ($inventory.schema -ne 1 -or $inventory.target -ne 'x86_64-pc-windows-msvc') {
    throw 'Unsupported license inventory schema or target.'
}
if ((Get-Hash (Get-ProjectPath 'Cargo.lock')) -ne $inventory.cargo_lock_sha256) {
    throw 'Cargo.lock changed; re-audit third-party licenses before release.'
}
$rustc = (& rustc -vV) -join "`n"
if ($LASTEXITCODE -ne 0 -or $rustc.Trim() -ne $inventory.rustc) {
    throw 'Rust compiler changed; refresh the standard-library notices before release.'
}
$metadataText = & cargo metadata --manifest-path (Get-ProjectPath 'Cargo.toml') --locked --offline `
    --filter-platform $inventory.target --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed; fetch locked dependencies first.' }
$metadata = ($metadataText -join "`n") | ConvertFrom-Json -AsHashtable
$nodes = @{}
foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$pending = [Collections.Generic.Stack[string]]::new()
$pending.Push($metadata.resolve.root)
$active = [Collections.Generic.HashSet[string]]::new()
while ($pending.Count) {
    $id = $pending.Pop()
    if (-not $active.Add($id)) { continue }
    foreach ($dependency in $nodes[$id].dependencies) { $pending.Push($dependency) }
}
$actual = @($metadata.packages | Where-Object {
    $active.Contains($_.id) -and $_.id -ne $metadata.resolve.root
})
Assert-SameSet @($actual | ForEach-Object { "$($_.name)@$($_.version)" }) `
    @($inventory.packages | ForEach-Object { "$($_.name)@$($_.version)" }) 'Dependency graph'
$root = @($metadata.packages | Where-Object id -eq $metadata.resolve.root)[0]
if ($root.version -ne $inventory.app_version -or $root.license -ne 'MIT') {
    throw 'Application version/license changed; update the distribution notices.'
}
Assert-SameSet $nodes[$root.id].features $inventory.features 'Application features'
foreach ($package in $inventory.packages) {
    $resolved = @($actual | Where-Object { $_.name -eq $package.name -and $_.version -eq $package.version })[0]
    if ($resolved.license -ne $package.declared_license -or $resolved.source -ne $package.source) {
        throw "Package license/origin changed: $($package.name)"
    }
    Assert-SameSet $nodes[$resolved.id].features $package.features "Features of $($package.name)"
    if (-not $package.license_files.Count) { throw "Missing license: $($package.name)" }
    foreach ($file in $package.license_files) {
        if (-not $inventory.files.ContainsKey($file)) { throw "Untracked notice: $file" }
    }
    if ($package.declared_license -eq 'MPL-2.0') {
        if (-not $package.source_archive -or
            $inventory.files[$package.source_archive] -ne $package.archive_sha256) {
            throw "Missing or mismatched MPL source archive: $($package.name)"
        }
    }
}
foreach ($file in $inventory.files.GetEnumerator()) {
    if ((Get-Hash (Get-ProjectPath $file.Key)) -ne $file.Value) {
        throw "License/source file changed: $($file.Key)"
    }
}
$legalFiles = @(Get-ChildItem -LiteralPath (Get-ProjectPath 'licenses') -Recurse -File -Force |
    ForEach-Object { [IO.Path]::GetRelativePath($ProjectRoot, $_.FullName).Replace('\', '/') })
Assert-SameSet $legalFiles @(@($inventory.files.Keys | Where-Object { $_.StartsWith('licenses/') }) +
    'licenses/dependencies.json') 'Legal files'
$vendorFiles = @(Get-ChildItem -LiteralPath (Get-ProjectPath 'vendor/cpal') -Recurse -File -Force |
    ForEach-Object { [IO.Path]::GetRelativePath($ProjectRoot, $_.FullName).Replace('\', '/') })
Assert-SameSet $vendorFiles @($inventory.vendor_files.Keys) 'Vendored CPAL files'
foreach ($file in $inventory.vendor_files.GetEnumerator()) {
    if ((Get-TextHash (Get-ProjectPath $file.Key)) -ne $file.Value) {
        throw "Vendored CPAL changed: $($file.Key); review its modification notices."
    }
}
foreach ($file in $inventory.modified_cpal_files) {
    $path = Get-ProjectPath "vendor/cpal/$file"
    if ([IO.File]::ReadAllText($path) -notmatch '\AModified for kyouko-player|\A(?://|#|>) Modified for kyouko-player') {
        throw "Missing Apache modification notice: $file"
    }
}
if ($StageDirectory) {
    $stage = [IO.Path]::GetFullPath((Join-Path $ProjectRoot $StageDirectory))
    $targetPrefix = (Get-ProjectPath 'target') + [IO.Path]::DirectorySeparatorChar
    if (-not $stage.StartsWith($targetPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        (Test-Path -LiteralPath $stage)) {
        throw 'StageDirectory must be a new directory under this project target/.'
    }
    $null = New-Item -ItemType Directory -Path $stage
    foreach ($relative in @($inventory.files.Keys) + 'licenses/dependencies.json') {
        $destination = Join-Path $stage $relative
        $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination)
        Copy-Item -LiteralPath (Get-ProjectPath $relative) -Destination $destination
        if ((Get-Hash $destination) -ne (Get-Hash (Get-ProjectPath $relative))) {
            throw "Staged file mismatch: $relative"
        }
    }
    Write-Output "Staged complete legal materials: $stage"
}
Write-Output "Verified $($inventory.packages.Count) dependency packages, license/notice hashes, MPL sources and Rust library notices."
