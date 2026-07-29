# AUR Packaging

This directory contains repo-side templates for the `coding-brain-bin` AUR package.

The actual AUR package must live in its own AUR git repository, but these files
keep the package definition reproducible from the main repo.

## Update flow

1. Fetch the x86_64 Linux release digest for the new tag.
2. Compute the SHA256 of `LICENSE`.
3. Re-render the package files:

```bash
./scripts/render-aur-bin-files.sh <version> <linux_x86_64_sha256> <license_sha256> packaging/aur/coding-brain-bin
```

4. Copy `packaging/aur/coding-brain-bin/PKGBUILD` and `.SRCINFO` into the AUR repo.
5. Build and install the package in a clean test environment, then run
   `cbrain --version`. The AUR package remains `coding-brain-bin`; it installs
   only the `cbrain` executable.
6. Commit and push the AUR repo update.
