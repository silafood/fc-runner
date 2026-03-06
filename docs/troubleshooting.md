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

### VM boots but runner never registers

**Cause:** Bad JIT token, expired token, or no network connectivity from the VM.

**Diagnose:**
```bash
# Check fc-runner logs
sudo journalctl -u fc-runner --since "5 minutes ago"

# Check VM log (if still present)
ls /var/lib/fc-runner/vms/*.log

# Verify TAP interface is up
ip addr show tap-fc0

# Verify NAT rules
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

**Cause:** Wrong `runner_group_id` or PAT missing `repo` scope.

**Fix:**
- Verify `runner_group_id = 1` in config (1 is the default group)
- Re-issue the PAT with `repo` scope
- Check if the repository has self-hosted runners enabled in Settings > Actions > Runners

---

### Jobs dispatched twice

**Cause:** Poll interval shorter than VM startup time, and the dedup set was cleared.

**Fix:** The `HashSet` dedup in the orchestrator prevents this under normal operation. If you see duplicates:
- Increase `poll_interval_secs`
- Check logs for errors that might cause premature job ID removal from the seen set

---

### Rootfs runs out of space during a job

**Cause:** The 4 GiB default golden image is too small for large build artifacts.

**Fix:** Rebuild the golden rootfs with a larger size. In `install.sh`, change:
```bash
ROOTFS_SIZE_MIB=4096   # Increase to 8192 or more
```
Then delete the existing golden image and re-run:
```bash
sudo rm /opt/fc-runner/runner-rootfs-golden.ext4
sudo bash install.sh
```

---

## Useful Commands

```bash
# Service status
sudo systemctl status fc-runner

# Live logs
sudo journalctl -u fc-runner -f

# Check for running VMs
pgrep -a firecracker

# List registered runners via GitHub API
curl -s \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/{owner}/{repo}/actions/runners \
  | jq '.runners[] | {id, name, status}'

# Rebuild and redeploy
cargo build --release
sudo install -m 0755 target/release/fc-runner /usr/local/bin/fc-runner
sudo systemctl restart fc-runner
```
