param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$')]
    [string] $Tag,

    [switch] $Push,
    [switch] $DryRun
)

$ErrorActionPreference = "Stop"

function Run-Git {
    param([string[]] $GitArgs)

    if ($DryRun) {
        Write-Host "git $($GitArgs -join ' ')"
        return
    }

    & git @GitArgs
    if ($LASTEXITCODE -ne 0) {
        throw "git $($GitArgs -join ' ') failed"
    }
}

function Run-Cargo {
    param([string[]] $CargoArgs)

    if ($DryRun) {
        Write-Host "cargo $($CargoArgs -join ' ')"
        return
    }

    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArgs -join ' ') failed"
    }
}

function Run-CheckedOutput {
    param(
        [string] $Expected,
        [Parameter(ValueFromRemainingArguments = $true)][string[]] $Command
    )

    if ($DryRun) {
        Write-Host "$($Command -join ' ')"
        return
    }

    $rawOutput = & $Command[0] @($Command | Select-Object -Skip 1)
    if ($LASTEXITCODE -ne 0) {
        throw "$($Command -join ' ') failed"
    }
    $output = ($rawOutput | Out-String).Trim()
    if ($output -ne $Expected) {
        throw "Expected '$Expected' but got '$output'."
    }
}

$repoRoot = (& git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($repoRoot)) {
    throw "This script must be run inside a git repository."
}

Set-Location $repoRoot

& git diff --quiet --exit-code
if ($LASTEXITCODE -ne 0) {
    throw "Tracked files have unstaged changes. Commit or stash them before releasing."
}

& git diff --cached --quiet --exit-code
if ($LASTEXITCODE -ne 0) {
    throw "Tracked files have staged changes. Commit or stash them before releasing."
}

$existingTag = (& git tag --list $Tag)
if ($existingTag) {
    throw "Tag '$Tag' already exists."
}

$version = $Tag.Substring(1)
$cargoTomlPath = Join-Path $repoRoot "Cargo.toml"
$cargoLockPath = Join-Path $repoRoot "Cargo.lock"

$cargoToml = Get-Content -LiteralPath $cargoTomlPath -Raw
$newCargoToml = [regex]::Replace(
    $cargoToml,
    '(?ms)^(\[package\]\s+.*?^version\s*=\s*")[^"]+(")',
    "`${1}$version`${2}",
    1
)
if ($newCargoToml -eq $cargoToml) {
    throw "Could not update [package].version in Cargo.toml."
}

$cargoLock = Get-Content -LiteralPath $cargoLockPath -Raw
$newCargoLock = [regex]::Replace(
    $cargoLock,
    '(?ms)^(name\s*=\s*"meatshell"\s*)(\r?\n)(version\s*=\s*")[^"]+(")',
    "`${1}`${2}`${3}$version`${4}",
    1
)
if ($newCargoLock -eq $cargoLock) {
    throw "Could not update meatshell version in Cargo.lock."
}

if ($DryRun) {
    Write-Host "Would update desktop Cargo manifests/locks to $version."
} else {
    # Windows PowerShell 5 uses the active ANSI code page for Set-Content by
    # default, which corrupts non-ASCII comments and makes Cargo reject the
    # manifests as invalid UTF-8. Write explicit UTF-8 without a BOM instead.
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($cargoTomlPath, $newCargoToml, $utf8NoBom)
    [System.IO.File]::WriteAllText($cargoLockPath, $newCargoLock, $utf8NoBom)
}

Run-Cargo -CargoArgs @("check", "--locked")
Run-CheckedOutput -Expected "meatshell $version" -Command @(
    "cargo", "run", "--locked", "--", "--version"
)

Run-Git -GitArgs @(
    "add",
    "Cargo.toml",
    "Cargo.lock"
)
Run-Git -GitArgs @("commit", "-m", "Release $Tag")
Run-Git -GitArgs @("tag", "-a", $Tag, "-m", "Release $Tag")

if ($Push) {
    Run-Git -GitArgs @("push", "origin", "HEAD")
    Run-Git -GitArgs @("push", "origin", $Tag)
    Write-Host "Released $Tag and pushed branch + tag."
} else {
    Write-Host "Created release commit and tag $Tag."
    Write-Host "Push with: git push origin HEAD && git push origin $Tag"
}
