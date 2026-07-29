# homebrew-core Submission

This directory holds the formula draft that will land in
[`Homebrew/homebrew-core`](https://github.com/Homebrew/homebrew-core) — *not*
the tap formula at `aleadag/homebrew-tap`, which keeps shipping
prebuilt-binary tarballs as the fast install path.

The core formula is source-built, runs through Homebrew's CI bottle pipeline,
and is what `brew install coding-brain` resolves to once accepted.

## Submitting

1. Bump `url` and `sha256` in `coding-brain.rb` to the release you want to ship.
   ```sh
   curl -fsSL https://github.com/aleadag/coding-brain/archive/refs/tags/vX.Y.Z.tar.gz \
     | shasum -a 256
   ```
2. Locally:
   ```sh
   brew install --build-from-source ./packaging/homebrew-core/coding-brain.rb
   brew test coding-brain
   brew audit --strict --new --online coding-brain
   ```
   All three must pass cleanly before opening the PR. `brew test coding-brain`
   is the formula test command; the installed executable it exercises is
   `cbrain`.
3. Fork `Homebrew/homebrew-core`, drop the file at `Formula/c/coding-brain.rb`,
   and open a PR following the
   [Adding Software to Homebrew](https://docs.brew.sh/Adding-Software-to-Homebrew)
   checklist. Mention this repo and a recent release in the description.

## What this formula does differently from the tap

| Aspect                | Tap (`homebrew-tap`)             | Core (`homebrew-core`)                 |
| --------------------- | -------------------------------- | -------------------------------------- |
| Source                | Prebuilt release tarballs        | GitHub source tarball, built via Cargo |
| Bottles               | None                             | Built by Homebrew CI                   |
| Test                  | `cbrain --version` smoke test    | `cbrain` version, help, and man page    |
| Auto-version tracking | Manual via `release.yml`         | `livecheck` block (`:github_latest`)   |
| Completions / man     | Not installed                    | Installed for bash/zsh/fish + `man1`   |

## When updating

After the formula is accepted into core, Homebrew's auto-bumper handles version
updates on each tag via the `livecheck` block. We only touch the formula here
if the install layout or test surface changes.
