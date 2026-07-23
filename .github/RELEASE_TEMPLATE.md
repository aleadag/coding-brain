## What's new

<!-- 1-2 sentence hook: what's the headline user-facing win? -->

## Highlights

<!-- 3-5 bullet points, each starting with a verb. Focus on what users can DO now, not what changed internally. -->

- **Feature**: ...
- **Improved**: ...
- **Fixed**: ...

## Demo

<!-- Optional: GIF, screenshot, or asciinema link showing the highlight in action -->

## Install / Upgrade

```bash
brew upgrade coding-brain                 # Homebrew
cargo install coding-brain                # crates.io
curl -fsSL https://raw.githubusercontent.com/aleadag/coding-brain/main/install.sh | sh
```

For hooks managed declaratively through Home Manager, rebuild Home Manager,
restart every configured provider, inspect `/hooks` when Codex is configured,
and run `coding-brain doctor`. For imperatively managed hooks, rerun
`coding-brain init <provider>` for the exact providers you manage, restart
them, and run `coding-brain doctor`.

## Changelog

<!-- Link to full diff: https://github.com/aleadag/coding-brain/compare/vX.Y.Z...vA.B.C -->

<details>
<summary>Full changelog</summary>

- commit description
- commit description

</details>

---

Questions? [Start a Discussion](https://github.com/aleadag/coding-brain/discussions). Found a bug? [Open an issue](https://github.com/aleadag/coding-brain/issues/new).
