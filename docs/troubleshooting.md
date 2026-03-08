# Troubleshooting

## Common Issues

### KVM not available

```
Error: KVM not available
```

**Cause:** Missing hardware virtualization or kernel module not loaded.

**Fix:**
```bash
sudo modprobe kvm_intel   # Intel CPUs
sudo modprobe kvm_amd     # AMD CPUs
ls /dev/kvm               # Should exist after loading
```

Nested virtualization (running inside a VM) requires the host hypervisor to expose virtualization extensions.

---

### Golden rootfs build fails

**Cause:** The auto-provisioning process failed during rootfs build. Common reasons include network issues during package download or insufficient disk space.

**Diagnose:**
```bash
# Check fc-runner logs for the specific error
sudo journalctl -u fc-runner --since "10 minutes ago" | grep -i error

# Check disk space
df -h /opt/fc-runner/
```

**Fix:**
- If network issues: verify DNS and internet connectivity from the host
- If disk space: free up space in `/opt/fc-runner/`
- To retry: delete partial build artifacts and restart:
  ```bash
  sudo rm -f /opt/fc-runner/runner-rootfs-golden.ext4
  sudo rm -f /opt/fc-runner/cloud-base.img /opt/fc-runner/cloud-raw.img
  sudo systemctl restart fc-runner
  ```

---

### VM boots but runner never registers

**Cause:** Bad JIT token, expired token, or no network connectivity from the VM.

**Diagnose:**
```bash
# Check fc-runner logs (includes guest log dump after VM exit)
sudo journalctl -u fc-runner --since "5 minutes ago"

# Look for [guest-log] lines showing what happened inside the VM
sudo journalctl -u fc-runner | grep guest-log

# Check NAT rules
sudo iptables -t nat -L POSTROUTING -v
sudo iptables -L FORWARD -v
```

---

### Mount fails: loop device errors

```
mount: /dev/loop*: failed to set up loop device
```

**Cause:** Loop devices exhausted.

**Fix:**
```bash
# Check current loop devices
losetup -l

# Increase max loop devices
sudo modprobe loop max_loop=64
```

---

### `cp --reflink` fails

**Cause:** The `work_dir` filesystem doesn't support reflinks (e.g., tmpfs or ext4).

**Fix:** This is not fatal — `cp --reflink=auto` falls back to a full copy on unsupported filesystems. For faster copies, use btrfs or xfs for the `work_dir` mount.

```bash
# Check filesystem type
df -Th /var/lib/fc-runner/vms
```

---

### GitHub API 422 on JIT config

**Cause:** Wrong `runner_group_id`, PAT missing required permissions, or PAT doesn't have access to one of the configured repos.

**Fix:**
- Verify `runner_group_id = 1` in config (1 is the default group)
- For classic PATs: ensure the `repo` scope is selected
- For fine-grained PATs: ensure `Actions` (R/W) and `Administration` (R/W) permissions are granted for **all** repos listed in `repo`/`repos`
- Check if each repository has self-hosted runners enabled in Settings > Actions > Runners

---

### Jobs dispatched twice

**Cause:** Poll interval shorter than VM startup time, and the dedup set was cleared.

**Fix:** The `HashSet` dedup in the orchestrator prevents this under normal operation. If you see duplicates:
- Increase `poll_interval_secs`
- Check logs for errors that might cause premature job ID removal from the seen set

---

### Rootfs runs out of space during a job

**Cause:** The golden image is too small for large build artifacts.

**Fix:** Rebuild the golden rootfs. The auto-provisioning creates a minimal image with headroom. For larger builds, you may need to customize `setup.rs` to increase the rootfs size, or use the manual `build-v611-linux.sh` script with a custom size.

```bash
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

---

### VM killed by timeout

```
Error: VM execution timed out
```

**Cause:** The job exceeded `vm_timeout_secs` (default: 3600 seconds).

**Fix:**
- Increase `runner.vm_timeout_secs` in config for long-running jobs
- Investigate why the job is taking so long (large builds, network issues)

---

### Config file permission warning

```
config file is world-readable — contains secrets!
```

**Cause:** The config file at `/etc/fc-runner/config.toml` has permissions allowing other users to read it (it contains the GitHub PAT).

**Fix:**
```bash
sudo chmod 0600 /etc/fc-runner/config.toml
```

---

### Symlink rejected on critical path

```
Error: path is a symlink (security risk)
```

**Cause:** A critical path (`kernel_path`, `rootfs_golden`, `binary_path`) is a symlink. fc-runner rejects symlinks on these paths to prevent path traversal attacks.

**Fix:** Use direct paths instead of symlinks for these config values.

---

### Rate limit exhausted

```
ERROR: GitHub API rate limit nearly exhausted, backing off
```

**Cause:** The GitHub REST API rate limit (5,000 requests/hour) is nearly depleted.

**Fix:**
- Increase `runner.poll_interval_secs` to reduce API calls
- With multi-repo configs, each repo adds ~2 API calls per poll cycle — reduce the number of repos or increase the interval
- Check if multiple instances of fc-runner are sharing the same PAT

---

### Guest VM enters emergency mode

**Cause:** Typically caused by invalid `/etc/fstab` entries (e.g., EFI mount units from the cloud image) or missing systemd services.

**Fix:** The auto-provisioning in `setup.rs` handles this by:
- Writing a clean `/etc/fstab` with only `/dev/vda`
- Masking `boot-efi.mount` and `systemd-gpt-auto-generator`
- Removing `/etc/fstab.d/` snippets

If you still see this, force a rootfs rebuild:
```bash
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner
```

---

### GitHub App authentication fails

```
Error: failed to generate installation token
```

**Cause:** Invalid App credentials, expired JWT, or the App isn't installed on the target org/repos.

**Diagnose:**
```bash
# Check fc-runner logs for the specific error
sudo journalctl -u fc-runner --since "5 minutes ago" | grep -i "app\|jwt\|installation"
```

**Fix:**
- Verify `app_id` and `installation_id` in config match the GitHub App settings
- Ensure the private key file exists and is readable: `ls -la /etc/fc-runner/app-key.pem`
- Verify the App is installed on the correct org/user and has access to the configured repos
- Check that the private key hasn't been rotated — re-download from GitHub if needed
- Ensure the host clock is accurate (JWT validation is time-sensitive): `timedatectl status`

---

### MMDS metadata not available in guest

**Cause:** Guest can't reach the MMDS endpoint at `169.254.169.254`, or the MMDS PUT failed.

**Diagnose:**
```bash
# Check fc-runner logs for MMDS errors
sudo journalctl -u fc-runner | grep -i mmds

# Inside the guest (if accessible), check MMDS connectivity
curl -s -H "X-metadata-token: $(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-metadata-token-ttl-seconds: 21600')" http://169.254.169.254/latest/meta-data/
```

**Fix:**
- Ensure `secret_injection = "mmds"` (or omit it — MMDS is the default)
- Check that the Firecracker API socket is being created (MMDS mode uses `--api-sock`, not `--no-api`)
- Fall back to mount mode temporarily: `secret_injection = "mount"`
- The guest-side script `fetch-mmds-env.sh` retries 30 times with 1s intervals — check if it's present in the rootfs

---

### Metrics endpoint not responding

```bash
curl: (7) Failed to connect to localhost port 9090
```

**Cause:** HTTP server disabled or listen address misconfigured.

**Fix:**
- Check `[server]` config: ensure `enabled` is not set to `false`
- Verify `listen_addr` — default is `0.0.0.0:9090`
- Check if something else is using port 9090: `ss -tlnp | grep 9090`
- Check logs: `sudo journalctl -u fc-runner | grep "management\|server"`

---

### Pool VMs not starting

**Cause:** Pool configuration issue or no slots available.

**Diagnose:**
```bash
sudo journalctl -u fc-runner | grep -i pool
```

**Fix:**
- Ensure `[[pool]]` sections have valid `repos` that exist in `github.repos`
- Check that `max_ready` across all pools doesn't exceed `max_concurrent_jobs`
- Verify each pool has at least one slot allocated (check "allocating slots to pool" log messages)

---

### Org-level runner registration fails

```
Error: failed to generate org JIT config
```

**Cause:** The PAT or GitHub App doesn't have organization-level permissions, or the `organization` config value is incorrect.

**Fix:**
- Verify `github.organization` matches the actual GitHub org name
- For PATs: ensure the token has `admin:org` scope (classic) or org-level `Actions` + `Administration` permissions (fine-grained)
- For GitHub Apps: ensure the App is installed at the org level with appropriate permissions
- Check that the org allows self-hosted runners: **Organization settings > Actions > Runners > Allow self-hosted runners**

---

### Pool pause/resume/scale not working

**Cause:** Pool management requires the management API to be running and accessible.

**Diagnose:**
```bash
# Check pool status via CLI
fc-runner pools list --endpoint http://localhost:9090

# Or via API
curl -s http://localhost:9090/api/v1/pools | jq .
```

**Fix:**
- Ensure `[server]` is enabled (default: `true`) and the correct `listen_addr` is set
- If using `api_key`, ensure the CLI or curl request includes the correct key
- Pool commands only work when the server is running in pool mode (`[[pool]]` sections configured)
- Check logs: `sudo journalctl -u fc-runner | grep pool`

---

## Useful Commands

```bash
# Validate config
fc-runner validate --config /etc/fc-runner/config.toml

# Service status
sudo systemctl status fc-runner

# Live logs
sudo journalctl -u fc-runner -f

# List running VMs via CLI
fc-runner ps --endpoint http://localhost:9090

# Pool management via CLI
fc-runner pools list --endpoint http://localhost:9090
fc-runner pools scale default --min-ready 3 --endpoint http://localhost:9090
fc-runner pools pause default --endpoint http://localhost:9090
fc-runner pools resume default --endpoint http://localhost:9090

# Check for running VMs
pgrep -a firecracker

# Prometheus metrics
curl -s http://localhost:9090/metrics

# Management API — server status
curl -s http://localhost:9090/api/v1/status | jq .

# Management API — list active VMs
curl -s http://localhost:9090/api/v1/vms | jq .
# With API key:
curl -s -H "X-Api-Key: your-key" http://localhost:9090/api/v1/vms | jq .

# Management API — list pools
curl -s -H "X-Api-Key: your-key" http://localhost:9090/api/v1/pools | jq .

# Health check
curl -s http://localhost:9090/healthz

# List registered runners via GitHub API
curl -s \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/{owner}/{repo}/actions/runners \
  | jq '.runners[] | {id, name, status}'

# List org-level runners
curl -s \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/orgs/{org}/actions/runners \
  | jq '.runners[] | {id, name, status}'

# Rebuild and redeploy
cargo build --release
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl restart fc-runner

# Force rootfs rebuild
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo systemctl restart fc-runner

```
