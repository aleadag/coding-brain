# Coding Brain Crate Namespace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Rename every active Rust package, crate, and installation surface from `codexctl` to the independently publishable Coding Brain namespace.

**Architecture:** Preserve the existing dependency direction while renaming all three workspace identities together: `coding-brain -> coding-brain-tui -> coding-brain-core`. Keep repository URLs and historical release notes unchanged; this work changes build-time and current user-facing names, not runtime behavior or persistent data.

**Tech Stack:** Rust 2024, Cargo workspace metadata, GitHub Actions, Markdown documentation, Jujutsu.

## Global Constraints

- The three Cargo packages are exactly `coding-brain`, `coding-brain-core`, and `coding-brain-tui`.
- The corresponding Rust crate identifiers are exactly `coding_brain`, `coding_brain_core`, and `coding_brain_tui`.
- Core and TUI remain independently publishable, with explicit versions on local path dependencies.
- The executable remains `coding-brain`; do not add compatibility packages, crate aliases, binary aliases, or codenames.
- Preserve `coding-brain -> coding-brain-tui -> coding-brain-core` and do not introduce upward dependencies.
- Do not rename the GitHub repository or rewrite historical `CHANGELOG.md` entries.
- Do not change persistent paths or runtime behavior.

---

### Task 1: Rename the Rust workspace and crate imports

**Files:**
- Move: `crates/codexctl-core/` to `crates/coding-brain-core/`
- Move: `crates/codexctl-tui/` to `crates/coding-brain-tui/`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/coding-brain-core/Cargo.toml`
- Modify: `crates/coding-brain-core/src/lib.rs`
- Modify: `crates/coding-brain-core/src/health.rs`
- Modify: `crates/coding-brain-core/tests/lifecycle_store.rs`
- Modify: `crates/coding-brain-tui/Cargo.toml`
- Modify: `crates/coding-brain-tui/src/**/*.rs`
- Modify: `src/**/*.rs`
- Modify: `tests/*.rs`
- Modify: `tests/fixtures/codex-running-shell-pane.txt`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: The existing `codexctl -> codexctl-tui -> codexctl-core` dependency graph.
- Produces: The same APIs under `coding_brain`, `coding_brain_tui`, and `coding_brain_core`, with no behavior change.

**Acceptance Criteria:**
- `cargo metadata` reports exactly the three approved package names.
- Root, TUI, and Core compile under their new crate identifiers.
- CI standalone checks address the renamed packages and directories.
- No active Rust source, test, manifest, CI check, or fixture requires `codexctl`, `codexctl_core`, `codexctl_tui`, `codexctl-core`, or `codexctl-tui` as a package/crate identity.

- [ ] **Step 1: Start the named jj changeset**

Run:

```bash
jj new -m "🏷️ refactor: rename Coding Brain Rust crates"
```

Expected: a new empty working-copy changeset on top of the accepted design.

- [ ] **Step 2: Prove the current package-name contract fails**

Run:

```bash
cargo metadata --no-deps --format-version 1 | jq -e '[.packages[].name] | sort == ["coding-brain", "coding-brain-core", "coding-brain-tui"]'
```

Expected before the rename: exit 1 because Cargo still reports `codexctl`, `codexctl-core`, and `codexctl-tui`.

- [ ] **Step 3: Rename directories and manifests**

Move both workspace directories, then update the root manifest to this shape:

```toml
[workspace]
members = ["crates/coding-brain-core", "crates/coding-brain-tui"]

[package]
name = "coding-brain"

[[bin]]
name = "coding-brain"
path = "src/main.rs"

[lib]
name = "coding_brain"
path = "src/lib.rs"

[dependencies]
coding-brain-core = { path = "crates/coding-brain-core", version = "0.53.0" }
coding-brain-tui = { path = "crates/coding-brain-tui", version = "0.53.0" }
```

Update the child manifests to these identities and dependency path:

```toml
# crates/coding-brain-core/Cargo.toml
[package]
name = "coding-brain-core"

[lib]
name = "coding_brain_core"
```

```toml
# crates/coding-brain-tui/Cargo.toml
[package]
name = "coding-brain-tui"

[lib]
name = "coding_brain_tui"

[dependencies]
coding-brain-core = { path = "../coding-brain-core", version = "0.53.0" }
```

Preserve every existing version, edition, MSRV, category, and non-workspace dependency.

- [ ] **Step 4: Rename Rust identifiers without changing APIs**

Apply these exact identifier mappings throughout active Rust source and tests:

```text
codexctl          -> coding_brain
codexctl_core     -> coding_brain_core
codexctl_tui      -> coding_brain_tui
```

Update crate-level prose and the compilation fixture with the same package names. Do not rename Codex-domain types such as `CodexSession` or repository URLs containing `aleadag/codexctl`.

- [ ] **Step 5: Align CI package and layering checks**

Update `.github/workflows/ci.yml` so standalone commands use:

```yaml
cargo build -p coding-brain-core
cargo clippy -p coding-brain-core --all-targets -- -D warnings
cargo test -p coding-brain-core
cargo build -p coding-brain-tui
cargo clippy -p coding-brain-tui --all-targets -- -D warnings
cargo test -p coding-brain-tui
```

Point the layering grep at `crates/coding-brain-core/src/` and update current job names and explanatory comments.

- [ ] **Step 6: Regenerate metadata and verify the renamed workspace**

Run:

```bash
cargo metadata --no-deps --format-version 1 | jq -e '[.packages[].name] | sort == ["coding-brain", "coding-brain-core", "coding-brain-tui"]'
cargo check --workspace --all-targets
cargo test -p coding-brain-core
cargo test -p coding-brain-tui
```

Expected: all commands exit 0; Cargo refreshes `Cargo.lock` with only the new package names.

- [ ] **Step 7: Review the atomic changeset**

Run:

```bash
jj --no-pager diff --git
jj --no-pager st
```

Expected: only workspace identity, imports, CI checks, fixture text, and lockfile identities change; no runtime logic changes.

---

### Task 2: Align publication and current documentation

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `README.md`
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/contributing.md`
- Modify: `docs/index.md`
- Modify: `docs/llms.txt`
- Modify: `docs/quickstart.md`
- Modify: `docs/decisions/ADR-0002-coding-brain-product-boundary.md`
- Modify: `LAUNCH_POSTS.md`
- Modify: `blog/posts.md`
- Modify: `.internal/specs/2026-07-20-coding-brain-crate-namespace-design.md`

**Interfaces:**
- Consumes: The package names and directory layout produced by Task 1.
- Produces: A release workflow that publishes Core, then TUI, then the root package, plus current documentation that installs and describes Coding Brain consistently.

**Acceptance Criteria:**
- The release workflow checks and publishes `coding-brain-core`, `coding-brain-tui`, then `coding-brain` in dependency order.
- Current installation instructions say `cargo install coding-brain`.
- Current architecture documentation uses the renamed dependency graph and directories.
- `AGENTS.md` and `CLAUDE.md` remain substantively mirrored.
- Repository checkout paths and URLs may remain `codexctl`; historical changelog entries remain untouched.
- Each package passes Cargo's package-file validation independently.

- [ ] **Step 1: Start the publication/documentation changeset**

Run:

```bash
jj new -m "📦 build: publish Coding Brain crates"
```

Expected: a new empty working-copy changeset on top of Task 1.

- [ ] **Step 2: Prove active publication names are stale**

Run:

```bash
rg -n 'cargo install codexctl|codexctl-core|codexctl-tui|codexctl_core|codexctl_tui|name = "codexctl"' README.md AGENTS.md CLAUDE.md docs LAUNCH_POSTS.md blog/posts.md .github/workflows
```

Expected before updates: matches in current installation, architecture, CI, and release text. Matches in historical decision context may be retained only when they explicitly describe the former state.

- [ ] **Step 3: Rename release publication jobs**

Update `.github/workflows/release.yml` so it reads versions from `crates/coding-brain-core/Cargo.toml` and `crates/coding-brain-tui/Cargo.toml`, queries the corresponding crates.io API names, and runs:

```yaml
cargo publish -p coding-brain-core --locked --token "$CARGO_REGISTRY_TOKEN"
cargo publish -p coding-brain-tui --locked --token "$CARGO_REGISTRY_TOKEN"
cargo publish -p coding-brain --locked --token "$CARGO_REGISTRY_TOKEN"
```

Rename current job and step labels to Coding Brain. Keep the checkout directory `codexctl` where it refers to the unchanged GitHub repository clone.

- [ ] **Step 4: Update current installation and architecture prose**

Apply these exact current-facing mappings:

```text
cargo install codexctl                         -> cargo install coding-brain
codexctl -> codexctl-tui -> codexctl-core     -> coding-brain -> coding-brain-tui -> coding-brain-core
crates/codexctl-core                           -> crates/coding-brain-core
crates/codexctl-tui                            -> crates/coding-brain-tui
```

Update ADR-0002's deferred-crate sentence to point to the accepted namespace design and state that the later rename is now adopted. Preserve historical `CHANGELOG.md` entries and repository URLs.

- [ ] **Step 5: Validate independent package contents and public naming**

Run:

```bash
cargo package --list -p coding-brain-core --allow-dirty
cargo package --list -p coding-brain-tui --allow-dirty
cargo package --list -p coding-brain --allow-dirty
rg -n 'cargo install codexctl|codexctl-core|codexctl-tui|codexctl_core|codexctl_tui|name = "codexctl"' README.md AGENTS.md CLAUDE.md docs LAUNCH_POSTS.md blog/posts.md .github/workflows Cargo.toml crates
```

Expected: all three package listings succeed. The final search has no active old crate or install references; any retained match is inspected and justified as historical context or an unchanged repository identifier.

- [ ] **Step 6: Run the complete repository gate**

Run in the declared development environment:

```bash
nix develop -c cargo fmt --check
nix develop -c cargo test
nix develop -c cargo clippy -- -D warnings
nix develop -c cargo build
```

Expected: every command exits 0.

- [ ] **Step 7: Review final scope**

Run:

```bash
jj --no-pager diff --git -r @-
jj --no-pager diff --git -r @
jj --no-pager log -r '@--::@' --no-graph
jj --no-pager st
```

Expected: Task 1 contains the atomic Rust workspace rename; Task 2 contains release and current-documentation alignment; the working copy has no accidental unrelated changes.

