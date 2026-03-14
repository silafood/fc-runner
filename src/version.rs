/// Build-time version string.
///
/// Priority:
/// 1. `FC_RUNNER_VERSION` env var (runtime override)
/// 2. Git tag if HEAD is tagged (e.g. `v1.0.0`)
/// 3. `branch/sha` for untagged builds (e.g. `master/a1b2c3d`)
/// 4. Cargo.toml version as final fallback
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // Runtime override
        if let Ok(v) = std::env::var("FC_RUNNER_VERSION") {
            if !v.is_empty() {
                return v;
            }
        }

        let tag = env!("FC_GIT_TAG");
        let sha = env!("FC_GIT_SHA");
        let branch = env!("FC_GIT_BRANCH");
        let dirty = env!("FC_GIT_DIRTY");

        if !tag.is_empty() {
            format!("{tag}{dirty}")
        } else if !sha.is_empty() {
            format!("{branch}/{sha}{dirty}")
        } else {
            env!("CARGO_PKG_VERSION").to_string()
        }
    })
}
