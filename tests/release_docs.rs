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

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn indented_section<'a>(document: &'a str, header: &str, header_indent: usize) -> Vec<&'a str> {
    let expected_header = format!("{}{header}:", " ".repeat(header_indent));
    let mut lines = document.lines();
    lines
        .find(|line| *line == expected_header)
        .unwrap_or_else(|| panic!("missing YAML section {header}"));
    lines
        .take_while(|line| line.trim().is_empty() || indentation(line) > header_indent)
        .collect()
}

fn mapping_keys<'a>(
    document: &'a str,
    header: &str,
    header_indent: usize,
    key_indent: usize,
) -> Vec<&'a str> {
    indented_section(document, header, header_indent)
        .into_iter()
        .filter(|line| indentation(line) == key_indent)
        .filter_map(|line| line.trim().split_once(':').map(|(key, _)| key))
        .collect()
}

fn named_steps<'a>(document: &'a str, job: &str) -> Vec<&'a str> {
    indented_section(document, job, 2)
        .into_iter()
        .filter_map(|line| line.strip_prefix("      - name: "))
        .collect()
}

fn step_count(document: &str, job: &str) -> usize {
    indented_section(document, job, 2)
        .into_iter()
        .filter(|line| line.starts_with("      - "))
        .count()
}

fn run_commands(document: &str, job: &str) -> Vec<String> {
    let lines = indented_section(document, job, 2);
    let mut commands = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let Some(value) = line.strip_prefix("        run: ") else {
            continue;
        };
        if value != "|" && value != ">-" {
            commands.push((*value).to_owned());
            continue;
        }

        let separator = if value == "|" { "\n" } else { " " };
        let command = lines[index + 1..]
            .iter()
            .take_while(|line| indentation(line) > 8)
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(separator);
        commands.push(command);
    }

    commands
}

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
    let workflow = read(".github/workflows/release-preview.yml");

    assert_eq!(mapping_keys(&workflow, "on", 0, 2), ["workflow_dispatch"]);
    assert_eq!(mapping_keys(&workflow, "permissions", 0, 2), ["contents"]);
    assert_eq!(
        indented_section(&workflow, "permissions", 0)
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .map(str::trim)
            .collect::<Vec<_>>(),
        ["contents: read"]
    );
    assert_eq!(
        workflow
            .lines()
            .filter(|line| line.trim() == "permissions:")
            .collect::<Vec<_>>(),
        ["permissions:"]
    );

    assert_eq!(mapping_keys(&workflow, "jobs", 0, 2), ["stable", "msrv"]);
    for job in ["stable", "msrv"] {
        assert_eq!(
            mapping_keys(&workflow, job, 2, 4),
            ["name", "runs-on", "timeout-minutes", "steps"]
        );
    }
    assert_eq!(
        named_steps(&workflow, "stable"),
        [
            "Install stable Rust",
            "Check formatting",
            "Check all targets",
            "Build strict documentation",
            "Run doctests",
            "Run examples",
            "Run focused public API tests",
            "Verify package contents",
            "Build source package",
        ]
    );
    assert_eq!(
        named_steps(&workflow, "msrv"),
        [
            "Install Rust 1.85",
            "Check all targets",
            "Run examples",
            "Run focused public API tests",
        ]
    );
    assert_eq!(step_count(&workflow, "stable"), 10);
    assert_eq!(step_count(&workflow, "msrv"), 5);
    assert_eq!(
        workflow
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("- uses: ")
                    .or_else(|| line.trim().strip_prefix("uses: "))
            })
            .collect::<Vec<_>>(),
        ["actions/checkout@v4", "actions/checkout@v4"]
    );
    assert_eq!(
        run_commands(&workflow, "stable"),
        [
            "rustup toolchain install stable --profile minimal --component rustfmt",
            "rustup run stable cargo fmt --all -- --check",
            "rustup run stable cargo check --all-targets",
            "rustup run stable cargo doc --no-deps",
            "rustup run stable cargo test --doc",
            "rustup run stable cargo run --example quickstart\nrustup run stable cargo run --example oracle_types",
            "rustup run stable cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase5_oracle_types_api --test release_docs",
            "./.github/scripts/verify-package.ps1",
            "rustup run stable cargo package",
        ]
        .map(str::to_owned)
    );
    assert_eq!(
        run_commands(&workflow, "msrv"),
        [
            "rustup toolchain install 1.85.0 --profile minimal",
            "rustup run 1.85.0 cargo check --all-targets",
            "rustup run 1.85.0 cargo run --example quickstart\nrustup run 1.85.0 cargo run --example oracle_types",
            "rustup run 1.85.0 cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase5_oracle_types_api --test release_docs",
        ]
        .map(str::to_owned)
    );

    for line in workflow.lines() {
        let setting = line.trim().to_ascii_lowercase();
        assert!(
            setting != "write-all" && !setting.ends_with(": write"),
            "write permission is forbidden: {setting}"
        );
    }

    let lowercase = workflow.to_ascii_lowercase();
    for forbidden in [
        "pull_request_target",
        "workflow_call",
        "schedule:",
        "secrets",
        "actions/upload-artifact",
        "--ignored",
        "--include-ignored",
        "deep ci",
        "deep-ci",
        "deep_ci",
        "cargo publish",
        "cargo login",
        "npm publish",
        "docker push",
        "softprops/action-gh-release",
        "actions/create-release",
        "gh release",
        "gh api repos/",
        "git tag",
        "git push",
        "/releases",
        "crates.io",
    ] {
        assert!(
            !lowercase.contains(forbidden),
            "forbidden release capability: {forbidden}"
        );
    }
}
