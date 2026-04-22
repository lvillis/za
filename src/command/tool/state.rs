use super::*;
use graviola::hashing::{Hash, HashContext, Sha256};

pub(super) fn write_manifest(
    home: &ToolHome,
    tool: &ToolRef,
    source: &InstallSource,
) -> Result<()> {
    let install_path = home.install_path(tool);
    let meta = fs::metadata(&install_path)
        .with_context(|| format!("stat installed executable {}", install_path.display()))?;
    let digest = sha256_file(&install_path)?;
    let manifest = ToolManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        name: tool.name.clone(),
        version: tool.version.clone(),
        installed_at_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        source_kind: source.kind.to_string(),
        source_detail: source.detail.clone(),
        sha256: digest,
        size_bytes: meta.len(),
    };

    let manifest_path = home.manifest_path(tool);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(&manifest).context("serialize tool manifest")?;
    fs::write(&manifest_path, content)
        .with_context(|| format!("write manifest {}", manifest_path.display()))?;
    Ok(())
}

pub(super) fn ensure_manifest(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let manifest_path = home.manifest_path(tool);
    if manifest_path.exists() {
        return Ok(());
    }
    let source = InstallSource {
        kind: SOURCE_KIND_SYNTHESIZED,
        detail: "legacy install inferred from store layout".to_string(),
    };
    write_manifest(home, tool, &source)
}

pub(super) fn manifest_source_label(home: &ToolHome, tool: &ToolRef) -> Result<String> {
    let manifest_path = home.manifest_path(tool);
    if !manifest_path.exists() {
        return Ok("unknown".to_string());
    }
    let raw = match fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(_) => return Ok("unreadable".to_string()),
    };
    match serde_json::from_str::<ToolManifest>(&raw) {
        Ok(manifest) => Ok(manifest.source_kind),
        Err(_) => Ok("invalid".to_string()),
    }
}

pub(super) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finish();
    let mut hex = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

pub(super) fn adopt_tool(home: &ToolHome, tool: &str, dry_run: bool) -> Result<()> {
    let mut requested = ToolSpec::from_args(tool, None)?;
    requested.name = canonical_tool_name(&requested.name);
    if is_name_managed(home, &requested.name)? {
        bail!(
            "`{}` is already managed in {} scope; use `za tool update {}` to refresh it",
            requested.name,
            home.scope.label(),
            requested.name
        );
    }

    let installed = install(
        home,
        requested,
        InstallOptions::adopt(za_config::ProxyScope::Tool).dry_run(dry_run),
    )?;
    if !dry_run {
        println!("✅ Adopted {}", installed.tool.image());
    }
    Ok(())
}

pub(super) fn uninstall(home: &ToolHome, mut requested: ToolSpec) -> Result<()> {
    requested.name = canonical_tool_name(&requested.name);
    match requested.version {
        Some(version) => uninstall_version(
            home,
            &ToolRef {
                name: requested.name,
                version: normalize_version(&version),
            },
        ),
        None => uninstall_all_versions(home, &requested.name),
    }
}

fn uninstall_version(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let version_dir = home.version_dir(tool);
    if !version_dir.exists() {
        println!("🗑  Not installed: {}", tool.image());
        return Ok(());
    }

    let was_current = read_current_version(home, &tool.name)?
        .as_deref()
        .is_some_and(|v| v == tool.version);
    fs::remove_dir_all(&version_dir)
        .with_context(|| format!("remove {}", version_dir.display()))?;

    if collect_dir_names(&home.name_dir(&tool.name))?.is_empty() {
        let _ = fs::remove_dir(home.name_dir(&tool.name));
    }

    if was_current {
        remove_file_if_exists(&home.current_file(&tool.name))?;
        remove_active_entry(home, &tool.name)?;
        cleanup_post_uninstall_integrations(home, &tool.name)?;
        println!("🗑  Removed {} and cleared active version", tool.image());
    } else {
        println!("🗑  Removed {}", tool.image());
    }

    Ok(())
}

fn uninstall_all_versions(home: &ToolHome, name: &str) -> Result<()> {
    validate_name(name)?;
    let name_dir = home.name_dir(name);
    if !name_dir.exists() {
        println!("🗑  Not installed: {name}");
        return Ok(());
    }

    let versions = collect_dir_names(&name_dir)?;
    let removed_count = versions.len();
    fs::remove_dir_all(&name_dir).with_context(|| format!("remove {}", name_dir.display()))?;
    remove_file_if_exists(&home.current_file(name))?;
    remove_active_entry(home, name)?;
    cleanup_post_uninstall_integrations(home, name)?;

    println!("🗑  Removed {name} ({removed_count} version(s)) and cleared active entry");
    Ok(())
}

pub(crate) fn prune_non_active_versions(home: &ToolHome, active: &ToolRef) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for version in stale_versions_to_prune(home, active)? {
        let stale = ToolRef {
            name: active.name.clone(),
            version: version.clone(),
        };
        let stale_dir = home.version_dir(&stale);
        if stale_dir.exists() {
            fs::remove_dir_all(&stale_dir)
                .with_context(|| format!("remove stale version {}", stale_dir.display()))?;
            removed.push(version);
        }
    }
    removed.sort();
    Ok(removed)
}

pub(super) fn stale_versions_to_prune(home: &ToolHome, active: &ToolRef) -> Result<Vec<String>> {
    let name_dir = home.name_dir(&active.name);
    if !name_dir.exists() {
        return Ok(Vec::new());
    }

    let active_version = normalize_version(&active.version);
    let mut stale = collect_dir_names(&name_dir)?
        .into_iter()
        .filter(|version| normalize_version(version) != active_version)
        .collect::<Vec<_>>();
    stale.sort();
    Ok(stale)
}

pub(crate) fn command_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if let Some(stripped) = name.strip_suffix("-cli")
        && !stripped.is_empty()
    {
        out.push(stripped.to_string());
    }
    if let Some(stripped) = name.strip_suffix("_cli")
        && !stripped.is_empty()
    {
        out.push(stripped.to_string());
    }
    out.sort();
    out.dedup();
    out
}

pub(super) fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(super) fn copy_executable(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = dst.with_extension(format!("tmp-copy-{}", std::process::id()));
    remove_file_if_exists(&tmp)?;
    fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;

    #[cfg(unix)]
    {
        let src_mode = fs::metadata(src)?.permissions().mode();
        let mode = src_mode | 0o111;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }

    if let Err(err) = fs::rename(&tmp, dst) {
        #[cfg(windows)]
        {
            if err.kind() == io::ErrorKind::AlreadyExists {
                remove_file_if_exists(dst)?;
                fs::rename(&tmp, dst).with_context(|| {
                    format!(
                        "replace executable {} with {}",
                        dst.display(),
                        tmp.display()
                    )
                })?;
            } else {
                let _ = remove_file_if_exists(&tmp);
                return Err(err)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = remove_file_if_exists(&tmp);
            return Err(err)
                .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()));
        }
    }
    Ok(())
}

pub(super) fn read_current_version(home: &ToolHome, name: &str) -> Result<Option<String>> {
    let p = home.current_file(name);
    if !p.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&p)
        .with_context(|| format!("read current version file {}", p.display()))?;
    let version = content.trim().to_string();
    if version.is_empty() {
        return Ok(None);
    }
    Ok(Some(version))
}

pub(super) fn print_active_managed_path(home: &ToolHome, tool: &str) -> Result<()> {
    let name = canonical_tool_name(&ToolSpec::from_args(tool, None)?.name);
    let Some(_version) = read_current_version(home, &name)? else {
        bail!(
            "`{}` has no active managed version in {} scope",
            name,
            home.scope.label()
        );
    };
    let path = home.active_path(&name);
    if !path.exists() {
        bail!(
            "active managed path for `{}` is missing at {}; repair with `za tool update {}`",
            name,
            path.display(),
            name
        );
    }
    println!("{}", path.display());
    Ok(())
}

pub(super) fn activate_tool(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let previous_active = read_current_version(home, &tool.name)?;
    sync_active_entry(home, tool)?;

    if let Err(err) = set_current_version(home, tool) {
        let restore_res = restore_active_entry(home, &tool.name, previous_active.as_deref());
        let err = err.context("persist active tool version");
        if let Err(restore_err) = restore_res {
            return Err(err.context(format!("rollback active entry failed: {restore_err}")));
        }
        return Err(err);
    }

    Ok(())
}

fn restore_active_entry(home: &ToolHome, name: &str, previous_version: Option<&str>) -> Result<()> {
    match previous_version {
        Some(version) => {
            let previous = ToolRef {
                name: name.to_string(),
                version: version.to_string(),
            };
            if home.install_path(&previous).exists() {
                sync_active_entry(home, &previous)?;
            } else {
                remove_active_entry(home, name)?;
            }
        }
        None => {
            remove_active_entry(home, name)?;
        }
    }
    Ok(())
}

fn set_current_version(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    fs::create_dir_all(&home.current_dir)?;
    let p = home.current_file(&tool.name);
    let tmp = p.with_extension(format!("tmp-current-{}", std::process::id()));
    let mut f = File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    writeln!(f, "{}", tool.version)?;
    f.flush()
        .with_context(|| format!("flush {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &p) {
        let _ = remove_file_if_exists(&tmp);
        return Err(err).with_context(|| {
            format!(
                "replace current version {} -> {}",
                p.display(),
                tmp.display()
            )
        });
    }
    Ok(())
}

fn sync_active_entry(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let src = home.install_path(tool);
    if !src.exists() {
        bail!("tool version not installed: {}", tool.image());
    }

    match tool_layout_for_name(&tool.name) {
        ToolLayout::Binary => {
            let dst = home.bin_path(&tool.name);
            if let Err(err) = link_executable(&src, &dst) {
                copy_executable(&src, &dst).with_context(|| {
                    format!(
                        "activate {} via copy fallback after link failed: {err}",
                        tool.image()
                    )
                })?;
            }
            Ok(())
        }
        ToolLayout::Package => {
            let src_dir = home.package_payload_dir(tool);
            let dst_dir = home.current_package_path(&tool.name);
            link_directory(&src_dir, &dst_dir).with_context(|| {
                format!(
                    "activate {} package payload {}",
                    tool.image(),
                    src_dir.display()
                )
            })
        }
    }
}

fn remove_active_entry(home: &ToolHome, name: &str) -> Result<()> {
    remove_path_if_exists(&home.bin_path(name))?;
    remove_path_if_exists(&home.current_package_path(name))?;
    Ok(())
}

#[cfg(unix)]
fn link_executable(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let src = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let tmp = dst.with_extension(format!("tmp-link-{}", std::process::id()));
    remove_file_if_exists(&tmp)?;
    symlink(&src, &tmp)
        .with_context(|| format!("symlink {} -> {}", tmp.display(), src.display()))?;
    if let Err(err) = fs::rename(&tmp, dst) {
        let _ = remove_file_if_exists(&tmp);
        return Err(err)
            .with_context(|| format!("activate link {} -> {}", dst.display(), src.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn link_executable(_src: &Path, _dst: &Path) -> Result<()> {
    bail!("symlink activation is not supported on this platform")
}

#[cfg(unix)]
fn link_directory(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let src = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let tmp = dst.with_extension(format!("tmp-link-{}", std::process::id()));
    remove_path_if_exists(&tmp)?;
    symlink(&src, &tmp)
        .with_context(|| format!("symlink {} -> {}", tmp.display(), src.display()))?;
    if let Err(err) = fs::rename(&tmp, dst) {
        let _ = remove_path_if_exists(&tmp);
        return Err(err)
            .with_context(|| format!("activate link {} -> {}", dst.display(), src.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn link_directory(src: &Path, dst: &Path) -> Result<()> {
    copy_dir_recursive(src, dst)
}

pub(super) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
            } else {
                fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(not(unix))]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_path_if_exists(dst)?;
    fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;

    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copy {} -> {}", src_path.display(), dst_path.display())
            })?;
            #[cfg(unix)]
            {
                let mode = fs::metadata(&src_path)?.permissions().mode();
                fs::set_permissions(&dst_path, fs::Permissions::from_mode(mode))?;
            }
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&src_path)
                    .with_context(|| format!("read link {}", src_path.display()))?;
                std::os::unix::fs::symlink(&target, &dst_path).with_context(|| {
                    format!("symlink {} -> {}", dst_path.display(), target.display())
                })?;
            }
            #[cfg(not(unix))]
            {
                bail!(
                    "cannot copy symbolic link {} on this platform",
                    src_path.display()
                );
            }
        }
    }

    Ok(())
}

pub(super) fn collect_dir_names(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    Ok(out)
}

fn collect_current_state_names(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_current_state_file_name(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

pub(crate) fn collect_managed_tool_names(home: &ToolHome) -> Result<Vec<String>> {
    let mut names: HashSet<String> = collect_dir_names(&home.store_dir)?.into_iter().collect();
    for file in collect_current_state_names(&home.current_dir)? {
        names.insert(file);
    }
    let mut out = names.into_iter().collect::<Vec<_>>();
    out.sort();
    Ok(out)
}

fn is_current_state_file_name(name: &str) -> bool {
    name != LOCK_FILE
        && !name.starts_with(SELF_UPDATE_BACKUP_PREFIX)
        && !name.contains(CURRENT_TMP_FILE_MARKER)
}

pub(crate) fn cleanup_legacy_current_dir_artifacts(home: &ToolHome) -> Result<()> {
    let mut removed = 0usize;

    removed += cleanup_legacy_files_in_dir(&home.current_dir)?;
    removed += cleanup_legacy_files_in_dir(&home.self_update_backup_dir())?;

    if removed > 0 {
        eprintln!("🧹 Cleaned {removed} legacy tool state artifact(s)");
    }
    Ok(())
}

fn cleanup_legacy_files_in_dir(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("read {}", root.display())),
    };

    let mut removed = 0usize;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_legacy_current_artifact_name(&name) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "remove legacy tool state artifact {}",
                        entry.path().display()
                    )
                });
            }
        }
    }

    if root.file_name().and_then(|name| name.to_str()) == Some(SELF_UPDATE_BACKUP_DIR) {
        match fs::remove_dir(root) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("remove legacy backup directory {}", root.display()));
            }
        }
    }

    Ok(removed)
}

fn is_legacy_current_artifact_name(name: &str) -> bool {
    name.starts_with(SELF_UPDATE_BACKUP_PREFIX) || name.contains(CURRENT_TMP_FILE_MARKER)
}
