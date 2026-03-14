# fc-runner Documentation

fc-runner is a Rust-based orchestrator that polls GitHub for queued workflow jobs and boots an ephemeral Firecracker microVM per job on a Linux host.

## Table of Contents

- [Architecture](architecture.md)
- [Guest Kernel Configuration](guest-kernel-config.md)
- [Setup Guide](setup.md)
- [Configuration Reference](configuration.md)
- [Cache Service](configuration.md#cache-service)
- [rust-cache-s3](fork-rust-cache-s3.md) — Rust build cache with S3 support
- [Troubleshooting](troubleshooting.md)
