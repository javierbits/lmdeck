# Releasing lmdeck

The release workflow (`.github/workflows/release.yml`) publishes, on a `v*` tag:

- `lmdeck-<version>-<target>.tar.gz` for Linux x86_64/aarch64 (glibc 2.35+) and
  macOS x86_64/arm64, plus `checksums.txt`;
- `install.sh` and the generated `lmdeck.rb` Homebrew formula as release assets;
- the crate on crates.io, when `CARGO_REGISTRY_TOKEN` is set;
- `Formula/lmdeck.rb` in the `homebrew-lmdeck` tap, when `HOMEBREW_TAP` and
  `HOMEBREW_TAP_TOKEN` are set.

`install.sh` and the release assets are fetched anonymously, so the repository
must be public before the first release. The recipe index already expects that.

## One-time setup

1. Make the repository public:

   ```sh
   gh repo edit javierbits/lmdeck --visibility public \
     --accept-visibility-change-consequences
   ```

2. crates.io: create an API token at <https://crates.io/settings/tokens>, then
   add it to the repository secrets:

   ```sh
   gh secret set CARGO_REGISTRY_TOKEN --repo javierbits/lmdeck
   ```

   Without the secret the `crate` job prints a skip message; run
   `cargo publish` yourself afterwards.

3. Homebrew tap: create the public tap repository and a fine-grained token with
   `Contents: Read and write` on it:

   ```sh
   gh repo create javierbits/homebrew-lmdeck --public --add-readme \
     --description "Homebrew formula for lmdeck"
   gh secret set HOMEBREW_TAP_TOKEN --repo javierbits/lmdeck
   gh variable set HOMEBREW_TAP --repo javierbits/lmdeck \
     --body javierbits/homebrew-lmdeck
   ```

   The formula in the release is also usable without the tap:
   `brew install --formula ./lmdeck.rb`.

## Version

Bump `version` in `Cargo.toml` and `Cargo.lock` (`cargo check` refreshes the
lock), commit it, and wait for CI on `main`. The release workflow rejects a tag
that does not equal `v<Cargo.toml version>`.

## Preflight

Before tagging:

- the CI run for the release commit is green on Linux
  (`gh run list --workflow ci.yml`). The release workflow builds but does not
  run tests; a red `main` ships broken. The macOS job may still fail on
  `add_wraps_a_repo_and_runs_it_detached`, the known intermittent
  "my-server exited during startup" (see `docs/HANDOVER.md`); investigate any
  other macOS failure;
- `Cargo.toml` and `Cargo.lock` carry the release version, committed;
- the local gates pass: `cargo fmt --all --check`, `cargo clippy --all-targets
  --locked -- -D warnings`, `cargo test --locked`;
- `gh secret list` shows `CARGO_REGISTRY_TOKEN` and `HOMEBREW_TAP_TOKEN`, and
  `gh variable list` shows `HOMEBREW_TAP`. Without the tap token the Homebrew
  job prints a skip and the tap keeps the old formula;
- the repository is public, or every channel that fetches release assets and
  raw files anonymously is broken: `curl …/releases/latest/download/install.sh`,
  `cargo binstall`, and `lmdeck browse` (its default index URL is a raw file).

## Release

```sh
git tag -a v0.1.0 -m "lmdeck 0.1.0"
git push origin v0.1.0
```

A tag containing a hyphen is published as a GitHub prerelease. To retry a
failed run without moving the tag, use *Run workflow* with the tag name.

## Verify

```sh
# installer, including checksum verification
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/javierbits/lmdeck/releases/latest/download/install.sh |
  sh -s -- --version 0.1.0 --dir /tmp/lmdeck-bin
/tmp/lmdeck-bin/lmdeck --version

# crate
cargo install lmdeck --version 0.1.0 --root /tmp/lmdeck-cargo
/tmp/lmdeck-cargo/bin/lmdeck --version

# prebuilt through cargo-binstall
cargo binstall lmdeck --no-confirm

# Homebrew
brew install javierbits/lmdeck/lmdeck
lmdeck --version
```

Also check the assets and their checksums:

```sh
gh release view v0.1.0 --repo javierbits/lmdeck
gh release download v0.1.0 --repo javierbits/lmdeck --pattern checksums.txt
```

## If something is wrong

- Never move a published tag. Fix and cut the next patch version.
- Delete a bad release and tag before anyone installs it:
  `gh release delete v0.1.0 --yes --cleanup-tag`, then retag.
- crates.io versions are immutable: `cargo yank --version 0.1.0` hides it, then
  release `0.1.1`.
- Homebrew: revert the tap commit; the release asset `lmdeck.rb` is unchanged.

## Channels deliberately not offered yet

- `npx lmdeck`: `npx` runs a JavaScript package, so a wrapper package would
  download the same release tarball. It adds a second version stream for no
  capability; add it later if the JavaScript audience asks.
- `apt`/`.deb`: the installer covers glibc Linux. A repository adds signing and
  mirroring work; not needed for the first release.
