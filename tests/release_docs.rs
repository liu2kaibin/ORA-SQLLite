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
