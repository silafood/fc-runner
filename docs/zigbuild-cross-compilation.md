# Cross-Compilation and Static Linking Guide

A practical guide to building fully static Rust binaries for both **aarch64** (ARM 64-bit) and **x86_64** (AMD 64-bit) using cargo-zigbuild.

---

## Prerequisites

- Rust toolchain installed via [rustup](https://rustup.rs/)
- Internet access to download Zig

---

## Step 1: Install Zig

Zig provides the cross-compilation toolchain. One download covers all architectures.

```bash
ZIG_VERSION="0.14.1"

# Linux x86_64 host:
curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" \
  | tar -xJ -C /opt
ln -s "/opt/zig-linux-x86_64-${ZIG_VERSION}/zig" /usr/local/bin/zig

# Verify:
zig version
```

For other host platforms, replace `linux-x86_64` with:
- `linux-aarch64` (ARM Linux host)
- `macos-aarch64` (Apple Silicon Mac)
- `macos-x86_64` (Intel Mac)

---

## Step 2: Install cargo-zigbuild

```bash
cargo install cargo-zigbuild
```

This gives you the `cargo zigbuild` subcommand -- a drop-in replacement for `cargo build` that uses Zig as the linker.

---

## Step 3: Add Rust Targets

```bash
# Static linking targets (musl)
rustup target add aarch64-unknown-linux-musl
rustup target add x86_64-unknown-linux-musl

# Dynamic linking targets (glibc) -- optional
rustup target add aarch64-unknown-linux-gnu
rustup target add x86_64-unknown-linux-gnu
```

---

## Step 4: Build

### Static aarch64 (ARM 64-bit) binary

```bash
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

Output: `target/aarch64-unknown-linux-musl/release/<your-binary>`

### Static x86_64 (AMD 64-bit) binary

```bash
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Output: `target/x86_64-unknown-linux-musl/release/<your-binary>`

### Build both at once

```bash
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

---

## Step 5: Verify the Binary

```bash
# Check it's static (no dynamic dependencies):
file target/aarch64-unknown-linux-musl/release/my-app
# my-app: ELF 64-bit LSB executable, ARM aarch64, statically linked

ldd target/aarch64-unknown-linux-musl/release/my-app
# not a dynamic executable

file target/x86_64-unknown-linux-musl/release/my-app
# my-app: ELF 64-bit LSB executable, x86-64, statically linked
```

A statically linked binary has zero runtime dependencies. It runs on any Linux distribution without needing matching libraries installed.

---

## How It Works

```
cargo zigbuild --target aarch64-unknown-linux-musl
       |
       v
  cargo-zigbuild intercepts the linker call
       |
       v
  Zig cc (replaces gcc/ld)
  - Bundles musl libc headers for all architectures
  - Cross-compiles and links natively
  - Statically embeds musl into the binary
       |
       v
  Fully static aarch64 binary
  No runtime dependencies, runs on any Linux
```

Zig bundles pre-built libc implementations (musl, glibc) for all major targets inside its own binary. No system packages needed -- no `gcc-aarch64-linux-gnu`, no `musl-tools`, no environment variables.

---

## Target Reference

| Target | Arch | Libc | Static? | Use case |
|---|---|---|---|---|
| `aarch64-unknown-linux-musl` | ARM 64 | musl | Yes (automatic) | Deploy anywhere, containers, embedded |
| `x86_64-unknown-linux-musl` | x86 64 | musl | Yes (automatic) | Deploy anywhere, containers |
| `aarch64-unknown-linux-gnu` | ARM 64 | glibc | Dynamic | When glibc is required |
| `x86_64-unknown-linux-gnu` | x86 64 | glibc | Dynamic | Standard Linux builds |
| `aarch64-unknown-linux-gnu.2.17` | ARM 64 | glibc 2.17 | Dynamic | Pin to specific glibc version |

**musl targets produce fully static binaries automatically.** No extra flags needed.

---

## CI Workflow Examples

### GitHub Actions: Build both architectures

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, firecracker]
    strategy:
      matrix:
        target:
          - x86_64-unknown-linux-musl
          - aarch64-unknown-linux-musl
    steps:
      - uses: actions/checkout@v4
      - run: cargo zigbuild --release --target ${{ matrix.target }}
      - uses: actions/upload-artifact@v4
        with:
          name: my-app-${{ matrix.target }}
          path: target/${{ matrix.target }}/release/my-app
```

### GitHub Actions: Both in one job

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, firecracker]
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo zigbuild --release --target x86_64-unknown-linux-musl
          cargo zigbuild --release --target aarch64-unknown-linux-musl
      - uses: actions/upload-artifact@v4
        with:
          name: binaries
          path: |
            target/x86_64-unknown-linux-musl/release/my-app
            target/aarch64-unknown-linux-musl/release/my-app
```

---

## Testing Cross-Compiled Binaries

You built an aarch64 binary on an x86_64 host. How do you test it?

### Option 1: QEMU user-mode emulation

```bash
# Install (one-time):
apt-get install -y qemu-user-static

# Run aarch64 binary on x86_64:
qemu-aarch64-static target/aarch64-unknown-linux-musl/release/my-app

# Run tests:
cargo zigbuild --release --target aarch64-unknown-linux-musl --tests
qemu-aarch64-static target/aarch64-unknown-linux-musl/release/deps/my_app-*
```

This is the same mechanism Docker buildx uses. Performance is 5-10x slower than native, but works for functional testing.

### Option 2: Deploy and test on actual ARM hardware

Copy the static binary to an ARM machine and run it directly. Since it's statically linked, it needs nothing else installed.

---

## Handling C/C++ Dependencies

Most pure-Rust projects work with zigbuild out of the box. If your project links C libraries:

| Dependency | Solution |
|---|---|
| OpenSSL | Use `openssl = { features = ["vendored"] }` in Cargo.toml |
| zlib | Use `flate2` with `rust_backend` feature instead of system zlib |
| libsqlite3 | Use `rusqlite = { features = ["bundled"] }` |
| General C deps | Add vendored/bundled feature flags so Rust compiles them from source |

The key: use **vendored** builds so the C code is compiled from source by Zig, not linked against host system libraries.

---

## Installing in fc-runner Golden Rootfs

To make zigbuild available inside Firecracker VMs, add to the rootfs build script:

```bash
# --- Zig toolchain ---
ZIG_VERSION="0.14.1"
curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" \
  | tar -xJ -C /opt
ln -s "/opt/zig-linux-x86_64-${ZIG_VERSION}/zig" /usr/local/bin/zig

# --- cargo-zigbuild ---
cargo install cargo-zigbuild

# --- Rust cross-compilation targets ---
rustup target add aarch64-unknown-linux-musl
rustup target add x86_64-unknown-linux-musl

# --- Optional: QEMU for testing cross-compiled binaries ---
apt-get install -y qemu-user-static
```

### Space cost

| Component | Size |
|---|---|
| Zig + bundled libc | ~400-500 MB |
| cargo-zigbuild binary | ~5-10 MB |
| Rust targets | ~50-100 MB |
| **Total** | **~500-600 MB** |

---

## Limitations

| Limitation | Detail | Workaround |
|---|---|---|
| No static glibc | Zig cannot produce fully static glibc binaries | Use musl targets for static builds |
| `--target` required | Without it, zigbuild silently falls back to regular cargo | Always pass `--target` |
| C++ deps may need config | Complex C++ libraries may need vendored builds | Use `vendored`/`bundled` feature flags |
| Only Linux/macOS targets | Cannot cross-compile to Windows or BSD | Use traditional toolchain for those |
| bindgen + Zig 0.15+ | Zig 0.15 bundles libc++ 19, needs clang 18+ for bindgen | Stay on Zig 0.14.x or install clang-18 |

---

## Quick Reference

```bash
# Setup (one-time):
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

# Build static ARM 64-bit:
cargo zigbuild --release --target aarch64-unknown-linux-musl

# Build static x86 64-bit:
cargo zigbuild --release --target x86_64-unknown-linux-musl

# Test ARM binary on x86 host:
qemu-aarch64-static target/aarch64-unknown-linux-musl/release/my-app
```
