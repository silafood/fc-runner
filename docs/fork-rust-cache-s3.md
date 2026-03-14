# rust-cache-s3: Rust Cache with S3 Support

A fork of [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) with S3-compatible storage support for self-hosted runners.

**Published action:** https://github.com/marketplace/actions/rust-cache-s3
**Repository:** https://github.com/silafood/rust-cache-s3

## What it does

Caches Rust/Cargo build artifacts (target/, registry, git deps) with intelligent cleanup. When `RUNS_ON_S3_BUCKET_CACHE` is set in the environment, caches route to S3 instead of GitHub's Azure storage. Otherwise falls back to GitHub's cache.

```
silafood/rust-cache-s3
    └── CacheProvider
            ├── "github" → @actions/cache (Azure) — default
            ├── "s3"     → S3-compatible (RustFS/MinIO/AWS) — auto-detected
            ├── "buildjet"  → BuildJet
            └── "warpbuild" → WarpBuild
```

## Usage with fc-runner

fc-runner injects S3 credentials via MMDS automatically. No workflow config needed:

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, firecracker]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: silafood/rust-cache-s3@v2
      - run: cargo build --release
```

The action auto-detects S3 because `RUNS_ON_S3_BUCKET_CACHE` is already in the environment.

## Usage on GitHub-hosted runners

Falls back to GitHub's cache automatically (no S3 env vars present):

```yaml
- uses: silafood/rust-cache-s3@v2
```

## Usage with explicit S3 credentials

For runners without MMDS injection:

```yaml
- uses: silafood/rust-cache-s3@v2
  env:
    AWS_ACCESS_KEY_ID: ${{ secrets.S3_ACCESS_KEY }}
    AWS_SECRET_ACCESS_KEY: ${{ secrets.S3_SECRET_KEY }}
    AWS_REGION: us-east-1
    RUNS_ON_S3_BUCKET_CACHE: actions-cache
    RUNS_ON_S3_BUCKET_ENDPOINT: https://s3.example.com
    RUNS_ON_S3_FORCE_PATH_STYLE: "true"
```

## S3 environment variables

| Env var | Purpose |
|---------|---------|
| `RUNS_ON_S3_BUCKET_CACHE` | Bucket name (presence triggers S3 mode) |
| `RUNS_ON_S3_BUCKET_ENDPOINT` | S3 endpoint URL (for self-hosted) |
| `RUNS_ON_S3_FORCE_PATH_STYLE` | `"true"` for self-hosted S3 |
| `AWS_ACCESS_KEY_ID` | S3 access key |
| `AWS_SECRET_ACCESS_KEY` | S3 secret key |
| `AWS_REGION` | S3 region (default: `us-east-1`) |

## What gets cached

Same as Swatinem/rust-cache:
- `~/.cargo/registry/` — downloaded crate sources
- `~/.cargo/git/` — git dependencies
- `target/debug/deps/` — compiled dependencies (keeps fingerprints for incremental builds)
- `target/debug/.fingerprint/` — build fingerprints
- `target/debug/build/` — build script outputs

Cleaned automatically (not cached):
- `target/debug/incremental/` — incremental compilation data (large, not useful in CI)
- Top-level binaries in `target/debug/` — relinked fast from cached deps

## Keeping up with upstream

```bash
cd rust-cache-s3
git remote add upstream https://github.com/Swatinem/rust-cache.git
git fetch upstream
git merge upstream/master
npm run prepare
git add -A && git commit -m "chore: sync with upstream"
git tag -fa v2 -m "Updated v2" && git push origin v2 --force
```
