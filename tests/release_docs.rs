use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    fs::read_to_string(repository_root().join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

fn repository_relative_links(markdown: &str) -> Vec<&str> {
    let mut links = Vec::new();
    let mut remaining = markdown;

    while let Some(start) = remaining.find("](") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find(')') else {
            break;
        };
        let target = &remaining[..end];
        if !target.starts_with("http://")
            && !target.starts_with("https://")
            && !target.starts_with("mailto:")
            && !target.starts_with('#')
        {
            links.push(target.split('#').next().unwrap_or(target));
        }
        remaining = &remaining[end + 1..];
    }

    links
}

fn rustdoc_before(source: &str, signature: &str) -> String {
    let prefix = source
        .split_once(signature)
        .unwrap_or_else(|| panic!("missing function signature {signature}"))
        .0;
    let mut lines = prefix
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with("///"))
        .collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

fn assert_commit_failure_guidance(surface: &str, text: &str) {
    let text = text.to_ascii_lowercase().replace("///", " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        text.contains("outcome is uncertain"),
        "{surface} must identify an uncertain commit outcome"
    );
    assert!(
        text.contains("reopen") && text.contains("recovery"),
        "{surface} must require reopen/recovery"
    );
    assert!(
        text.contains("reconcile") && text.contains("before retry"),
        "{surface} must require state reconciliation before retry"
    );
    assert!(
        text.contains("drop the owning") && text.contains("all handles"),
        "{surface} must require dropping the owning Database and all handles"
    );
    assert!(
        text.contains("database::open"),
        "{surface} must require recovery through Database::open"
    );
    assert!(
        text.contains("database::connect") && text.contains("is not recovery"),
        "{surface} must state that Database::connect is not recovery"
    );
}

const RELEASE_PREVIEW_WORKFLOW: &str = r#"name: Release Preview

on:
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: release-preview-${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUSTDOCFLAGS: -D warnings

jobs:
  stable:
    name: Linux stable release preview
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal --component rustfmt
      - name: Check formatting
        run: rustup run stable cargo fmt --all -- --check
      - name: Check all targets
        run: rustup run stable cargo check --all-targets
      - name: Build strict documentation
        run: rustup run stable cargo doc --no-deps
      - name: Run doctests
        run: rustup run stable cargo test --doc
      - name: Run examples
        run: |
          rustup run stable cargo run --example quickstart
          rustup run stable cargo run --example oracle_types
      - name: Run focused public API tests
        run: >-
          rustup run stable cargo test
          --test public_api
          --test phase4c_api
          --test phase4c2_constraint_api
          --test phase5_oracle_types_api
          --test release_docs
      - name: Verify package contents
        shell: pwsh
        run: ./.github/scripts/verify-package.ps1
      - name: Build source package
        run: rustup run stable cargo package

  msrv:
    name: Linux Rust 1.85 release preview
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust 1.85
        run: rustup toolchain install 1.85.0 --profile minimal
      - name: Check all targets
        run: rustup run 1.85.0 cargo check --all-targets
      - name: Run examples
        run: |
          rustup run 1.85.0 cargo run --example quickstart
          rustup run 1.85.0 cargo run --example oracle_types
      - name: Run focused public API tests
        run: >-
          rustup run 1.85.0 cargo test
          --test public_api
          --test phase4c_api
          --test phase4c2_constraint_api
          --test phase5_oracle_types_api
          --test release_docs
"#;

#[test]
fn readme_has_the_release_dependency_and_runnable_examples() {
    let readme = read("README.md");
    assert!(readme.contains(
        "ora_sqllite = { git = \"https://github.com/liu2kaibin/ORA-SQLLite.git\", tag = \"v0.1.0\" }"
    ));
    assert!(readme.contains("cargo run --example quickstart"));
    assert!(readme.contains("cargo run --example oracle_types"));
    assert!(readme.contains("MSRV: Rust 1.85"));
    assert!(readme.contains("on-disk format: 2"));
}

#[test]
fn compatibility_uses_qualified_states_and_names_wire_incompatibility() {
    let matrix = read("docs/oracle-compatibility.md");
    for heading in ["Supported", "Partial", "Unsupported"] {
        assert!(matrix.contains(heading), "missing state {heading}");
    }
    assert!(matrix.contains("not Oracle wire compatible"));
    assert!(matrix.contains("single process"));
    assert!(matrix.contains("fixed offsets"));
}

#[test]
fn commit_failure_guidance_prevents_blind_write_retries() {
    let connection = read("src/api/connection.rs");
    let transaction = read("src/api/transaction.rs");
    let getting_started = read("docs/getting-started.md");
    let compatibility = read("docs/oracle-compatibility.md");

    assert_commit_failure_guidance(
        "Connection::execute rustdoc",
        &rustdoc_before(&connection, "    pub fn execute("),
    );
    assert_commit_failure_guidance(
        "WriteTransaction::commit rustdoc",
        &rustdoc_before(&transaction, "    pub fn commit("),
    );
    let transaction_lifecycle = getting_started
        .split_once("## Use snapshots and explicit writes")
        .expect("missing transaction lifecycle section")
        .1
        .split_once("\n## ")
        .expect("transaction lifecycle section must have a following section")
        .0;
    assert_commit_failure_guidance(
        "getting-started transaction guidance",
        transaction_lifecycle,
    );
    let auto_commit = compatibility
        .lines()
        .find(|line| line.starts_with("| Auto-commit |"))
        .expect("missing Auto-commit compatibility row");
    assert_commit_failure_guidance("compatibility Auto-commit row", auto_commit);
}

#[test]
fn public_documentation_links_resolve() {
    for source in [
        "README.md",
        "docs/getting-started.md",
        "docs/oracle-compatibility.md",
        "docs/releases/v0.1.0.md",
    ] {
        let markdown = read(source);
        let source_directory = Path::new(source).parent().unwrap_or(Path::new(""));
        for target in repository_relative_links(&markdown) {
            let resolved = repository_root().join(source_directory).join(target);
            assert!(
                resolved.exists(),
                "{source} links to missing repository path {target}"
            );
        }
    }
}

#[test]
fn release_preview_is_manual_read_only_and_never_publishes() {
    let workflow = read(".github/workflows/release-preview.yml").replace("\r\n", "\n");
    assert_eq!(
        workflow, RELEASE_PREVIEW_WORKFLOW,
        "release-preview.yml drifted from the approved manual, read-only, non-publishing workflow"
    );
}
