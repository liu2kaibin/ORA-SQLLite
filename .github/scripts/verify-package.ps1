param([switch]$AllowDirty)

$arguments = @('package', '--list')
if ($AllowDirty) { $arguments += '--allow-dirty' }
$files = @(& cargo @arguments) | ForEach-Object { $_ -replace '\\', '/' }
if ($LASTEXITCODE -ne 0) { throw 'cargo package --list failed' }

$required = @(
  'README.md', 'CHANGELOG.md', 'LICENSE-MIT', 'LICENSE-APACHE',
  'examples/quickstart.rs', 'examples/oracle_types.rs',
  'docs/getting-started.md', 'docs/oracle-compatibility.md', 'docs/releases/v0.1.0.md',
  'src/lib.rs'
)

foreach ($path in $required) {
  if ($path -notin $files) { throw "required package file is missing: $path" }
}

$allowedRoots = @('src/', 'examples/')
foreach ($path in $files) {
  $knownRoot = $allowedRoots | Where-Object { $path.StartsWith($_) }
  $knownExact = $path -in $required -or $path -in @('Cargo.lock', 'Cargo.toml', 'Cargo.toml.orig', '.cargo_vcs_info.json')
  if (-not $knownRoot -and -not $knownExact) { throw "unexpected package file: $path" }
  if ($path -match '(^|/)(target|\.worktrees|\.superpowers|tests|docs/superpowers)(/|$)|\.db(-wal)?$') {
    throw "forbidden package file: $path"
  }
}
$files | Sort-Object
