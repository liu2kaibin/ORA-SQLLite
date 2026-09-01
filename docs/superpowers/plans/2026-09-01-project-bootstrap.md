# ORA-SQLLite Project Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a minimal, buildable Rust library repository named ORA-SQLLite, preserve the approved architecture specification, and publish the initial commit to a private GitHub repository.

**Architecture:** Start with one Rust library crate named `ora_sqllite`; do not create database modules before the phase-1 implementation plan. Repository documentation carries the approved architecture, while `src/lib.rs` establishes only safety and public crate documentation.

**Tech Stack:** Rust stable with edition 2024 and MSRV 1.85, Cargo, Git, GitHub CLI.

**Spec:** `docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md`

## Global Constraints

- Project directory is exactly `C:\Users\liukaibin\codex\oracle\ORA-SQLLite`.
- GitHub repository name is exactly `ORA-SQLLite` and starts private.
- Crate name is `ora_sqllite`; Rust package version starts at `0.1.0`.
- Use Rust edition 2024 with `rust-version = "1.85"`.
- Keep the bootstrap dependency-free and forbid unsafe Rust.
- Do not publish a license until the repository owner selects one.
- Do not implement database behavior during bootstrap.
- Never overwrite an existing target directory or GitHub repository.

---

### Task 1: Create the buildable repository skeleton

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `README.md`
- Create: `src/lib.rs`
- Create: `docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md`
- Existing plan: `docs/superpowers/plans/2026-09-01-project-bootstrap.md`

**Interfaces:**
- Consumes: The approved architecture specification from the parent workspace.
- Produces: A dependency-free library crate named `ora_sqllite` that builds and exposes no database API yet.

- [ ] **Step 1: Confirm the target is safe**

Run:

```powershell
Test-Path -LiteralPath 'C:\Users\liukaibin\codex\oracle\ORA-SQLLite\.git'
Get-ChildItem -Recurse -File 'C:\Users\liukaibin\codex\oracle\ORA-SQLLite' |
    Select-Object -ExpandProperty FullName
```

Expected: `.git` is `False`, and the only existing file is this bootstrap plan. Any other content must be inspected rather than overwritten.

- [ ] **Step 2: Create the manifest and toolchain policy**

Create `Cargo.toml`:

```toml
[package]
name = "ora_sqllite"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
description = "A lightweight embedded database with an Oracle-compatible CRUD dialect"
repository = "https://github.com/liu2kaibin/ORA-SQLLite"

[dependencies]
```

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
profile = "minimal"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Create the minimal crate**

Create `src/lib.rs`:

```rust
#![forbid(unsafe_code)]
#![doc = "ORA-SQLLite: a lightweight embedded database with an Oracle-compatible CRUD dialect."]
```

Create `.gitignore`:

```gitignore
/target/
*.db
*.db-wal
*.db-shm
.superpowers/
```

- [ ] **Step 4: Add the project README and approved specification**

The README must state the selected boundaries: pure Rust, in-process library, single database file plus WAL, B+Tree storage, multi-reader/single-writer concurrency, Oracle CRUD compatibility, and no PL/SQL or network server.

Copy the approved specification verbatim to the repository path named in the plan header.

- [ ] **Step 5: Verify formatting and build**

Run:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Expected: all three commands exit with code 0. If Cargo is unavailable, record the task as blocked and do not claim a verified Rust bootstrap.

### Task 2: Initialize local Git history

**Files:**
- Add: all files created by Task 1

**Interfaces:**
- Consumes: The verified repository skeleton from Task 1.
- Produces: A local Git repository on branch `main` with one initial commit.

- [ ] **Step 1: Initialize Git on main**

Run from the project directory:

```powershell
git init -b main
```

Expected: `.git` is created and `git branch --show-current` prints `main`.

- [ ] **Step 2: Inspect exactly what will be committed**

Run:

```powershell
git status --short
git diff --no-index -- NUL .gitignore
```

Expected: only the bootstrap files, approved spec, and bootstrap plan are untracked. No database files, credentials, build outputs, or visualization files may be included.

- [ ] **Step 3: Create the initial commit**

Run:

```powershell
git add .gitignore Cargo.toml rust-toolchain.toml README.md src docs
git commit -m "chore: initialize ORA-SQLLite project"
```

Expected: commit succeeds and `git status --short` is empty.

### Task 3: Publish the private GitHub repository

**Files:**
- Modify: `.git/config` through Git remote configuration

**Interfaces:**
- Consumes: A clean local `main` branch and an authenticated GitHub CLI session for `liu2kaibin`.
- Produces: Private repository `liu2kaibin/ORA-SQLLite`, remote `origin`, and pushed `main` branch.

- [ ] **Step 1: Verify authentication and repository absence**

Run:

```powershell
gh auth status
gh repo view liu2kaibin/ORA-SQLLite
```

Expected: authentication succeeds; repository lookup reports not found. If the repository exists, stop rather than attach or overwrite it.

- [ ] **Step 2: Create and push the private repository**

Run from the project directory:

```powershell
gh repo create ORA-SQLLite --private --source . --remote origin --push
```

Expected: GitHub reports repository creation and push success.

- [ ] **Step 3: Verify local and remote state**

Run:

```powershell
git remote -v
git status --short --branch
gh repo view liu2kaibin/ORA-SQLLite --json nameWithOwner,visibility,url,defaultBranchRef
```

Expected: `origin` points to `liu2kaibin/ORA-SQLLite`, visibility is `PRIVATE`, default branch is `main`, and the local branch tracks `origin/main` with a clean worktree.
