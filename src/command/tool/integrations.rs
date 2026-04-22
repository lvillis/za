use super::*;

pub(super) fn ensure_post_activation_integrations(
    home: &ToolHome,
    tool: &ToolRef,
    emit_stages: bool,
) -> Result<()> {
    match tool.name.as_str() {
        "starship" => ensure_starship_bash_init(emit_stages),
        "ble.sh" => ensure_blesh_bash_init(home, tool, emit_stages),
        _ => Ok(()),
    }
}

pub(super) fn preview_post_activation_integrations(
    home: &ToolHome,
    tool: &ToolRef,
    emit_stages: bool,
) -> Result<()> {
    match tool.name.as_str() {
        "starship" => preview_starship_bash_init(emit_stages),
        "ble.sh" => preview_blesh_bash_init(home, tool, emit_stages),
        _ => Ok(()),
    }
}

fn ensure_starship_bash_init(emit_stages: bool) -> Result<()> {
    let rc_path = resolve_home_dir()?.join(".bashrc");
    let change = upsert_managed_block(
        &rc_path,
        STARSHIP_BASH_INIT_START_MARKER,
        STARSHIP_BASH_INIT_END_MARKER,
        ManagedBlockPosition::BeforeMarker(BLESH_BASH_INIT_BOTTOM_START_MARKER),
        starship_bash_init_block(),
    )
    .with_context(|| format!("configure starship bash init in `{}`", rc_path.display()))?;
    print_tool_stage_if(
        emit_stages,
        "next",
        format!(
            "starship bash init {} in {}; open a new JetBrains bash shell or `source {}`",
            change.label(),
            rc_path.display(),
            rc_path.display()
        ),
    );
    Ok(())
}

fn preview_starship_bash_init(emit_stages: bool) -> Result<()> {
    let rc_path = resolve_home_dir()?.join(".bashrc");
    let change = preview_managed_block(
        &rc_path,
        STARSHIP_BASH_INIT_START_MARKER,
        STARSHIP_BASH_INIT_END_MARKER,
        ManagedBlockPosition::BeforeMarker(BLESH_BASH_INIT_BOTTOM_START_MARKER),
        starship_bash_init_block(),
    )?;
    print_tool_stage_if(
        emit_stages,
        "next",
        format!(
            "starship bash init would be {} in {}",
            change.label(),
            rc_path.display()
        ),
    );
    Ok(())
}

fn resolve_home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve home directory: set `HOME`"))
}

fn ensure_blesh_bash_init(home: &ToolHome, tool: &ToolRef, emit_stages: bool) -> Result<()> {
    let rc_path = resolve_home_dir()?.join(".bashrc");
    let active_path = home.active_path(&tool.name);
    let top_change = upsert_managed_block(
        &rc_path,
        BLESH_BASH_INIT_TOP_START_MARKER,
        BLESH_BASH_INIT_TOP_END_MARKER,
        ManagedBlockPosition::Top,
        &blesh_bash_init_top_block(&active_path),
    )
    .with_context(|| format!("configure ble.sh bash prelude in `{}`", rc_path.display()))?;
    let bottom_change = upsert_managed_block(
        &rc_path,
        BLESH_BASH_INIT_BOTTOM_START_MARKER,
        BLESH_BASH_INIT_BOTTOM_END_MARKER,
        ManagedBlockPosition::Bottom,
        blesh_bash_init_bottom_block(),
    )
    .with_context(|| {
        format!(
            "configure ble.sh bash attach hook in `{}`",
            rc_path.display()
        )
    })?;
    print_tool_stage_if(
        emit_stages,
        "next",
        format!(
            "ble.sh bash init top={} bottom={} in {}; open a new JetBrains bash shell or `source {}`",
            top_change.label(),
            bottom_change.label(),
            rc_path.display(),
            rc_path.display()
        ),
    );
    Ok(())
}

fn preview_blesh_bash_init(home: &ToolHome, tool: &ToolRef, emit_stages: bool) -> Result<()> {
    let rc_path = resolve_home_dir()?.join(".bashrc");
    let active_path = home.active_path(&tool.name);
    let top_change = preview_managed_block(
        &rc_path,
        BLESH_BASH_INIT_TOP_START_MARKER,
        BLESH_BASH_INIT_TOP_END_MARKER,
        ManagedBlockPosition::Top,
        &blesh_bash_init_top_block(&active_path),
    )?;
    let bottom_change = preview_managed_block(
        &rc_path,
        BLESH_BASH_INIT_BOTTOM_START_MARKER,
        BLESH_BASH_INIT_BOTTOM_END_MARKER,
        ManagedBlockPosition::Bottom,
        blesh_bash_init_bottom_block(),
    )?;
    print_tool_stage_if(
        emit_stages,
        "next",
        format!(
            "ble.sh bash init would set top={} bottom={} in {}",
            top_change.label(),
            bottom_change.label(),
            rc_path.display()
        ),
    );
    Ok(())
}

pub(super) fn cleanup_post_uninstall_integrations(_home: &ToolHome, name: &str) -> Result<()> {
    if name != "ble.sh" {
        return Ok(());
    }

    let rc_path = resolve_home_dir()?.join(".bashrc");
    remove_managed_block(
        &rc_path,
        BLESH_BASH_INIT_TOP_START_MARKER,
        BLESH_BASH_INIT_TOP_END_MARKER,
    )?;
    remove_managed_block(
        &rc_path,
        BLESH_BASH_INIT_BOTTOM_START_MARKER,
        BLESH_BASH_INIT_BOTTOM_END_MARKER,
    )?;
    print_tool_stage(
        "next",
        format!("removed ble.sh bash init from {}", rc_path.display()),
    );
    Ok(())
}

pub(crate) fn starship_bash_init_block() -> &'static str {
    r#"if [ "${TERMINAL_EMULATOR-}" = "JetBrains-JediTerm" ]; then
  command -v starship >/dev/null 2>&1 && eval "$(starship init bash)"
fi"#
}

pub(crate) fn blesh_bash_init_top_block(active_path: &Path) -> String {
    format!(
        r#"if [ "${{TERMINAL_EMULATOR-}}" = "JetBrains-JediTerm" ] && [[ $- == *i* ]]; then
  if source -- "{}" --attach=none; then
    bleopt prompt_command_changes_layout=1
    bleopt internal_suppress_bash_output=
  fi
fi"#,
        active_path.display()
    )
}

pub(crate) fn blesh_bash_init_bottom_block() -> &'static str {
    r#"if [ "${TERMINAL_EMULATOR-}" = "JetBrains-JediTerm" ] && [[ ${BLE_VERSION-} ]]; then
  VSCODE_INJECTION=1 ble-attach
fi"#
}

pub(crate) fn upsert_managed_block(
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
    position: ManagedBlockPosition,
    body: &str,
) -> Result<ManagedFileChange> {
    let (updated, change) =
        compute_upsert_managed_block(target_path, start_marker, end_marker, position, body)?;
    if !matches!(change, ManagedFileChange::Unchanged) {
        fs::write(target_path, updated)
            .with_context(|| format!("write `{}`", target_path.display()))?;
    }
    Ok(change)
}

fn preview_managed_block(
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
    position: ManagedBlockPosition,
    body: &str,
) -> Result<ManagedFileChange> {
    let (_, change) =
        compute_upsert_managed_block(target_path, start_marker, end_marker, position, body)?;
    Ok(change)
}

fn compute_upsert_managed_block(
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
    position: ManagedBlockPosition,
    body: &str,
) -> Result<(String, ManagedFileChange)> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let block = format!("{start_marker}\n{body}\n{end_marker}");
    let (remaining, existed) =
        remove_managed_block_from_content(&existing, target_path, start_marker, end_marker)?;
    let updated = insert_managed_block(&remaining, &block, position);
    let change = if updated == existing {
        ManagedFileChange::Unchanged
    } else if existed {
        ManagedFileChange::Updated
    } else {
        ManagedFileChange::Created
    };
    Ok((updated, change))
}

fn remove_managed_block(target_path: &Path, start_marker: &str, end_marker: &str) -> Result<bool> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let (updated, removed) =
        remove_managed_block_from_content(&existing, target_path, start_marker, end_marker)?;
    if removed {
        fs::write(target_path, updated)
            .with_context(|| format!("write `{}`", target_path.display()))?;
    }
    Ok(removed)
}

fn remove_managed_block_from_content(
    existing: &str,
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
) -> Result<(String, bool)> {
    let Some(start) = existing.find(start_marker) else {
        return Ok((existing.to_string(), false));
    };
    let end = existing[start..]
        .find(end_marker)
        .map(|offset| start + offset + end_marker.len())
        .ok_or_else(|| {
            anyhow!(
                "found `{start_marker}` in `{}` without matching `{end_marker}`",
                target_path.display()
            )
        })?;
    let prefix = existing[..start].trim_end_matches('\n');
    let suffix = existing[end..].trim_start_matches('\n');
    let updated = match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{suffix}\n"),
        (false, true) => format!("{prefix}\n"),
        (false, false) => format!("{prefix}\n\n{suffix}\n"),
    };
    Ok((updated, true))
}

fn insert_managed_block(content: &str, block: &str, position: ManagedBlockPosition) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return format!("{block}\n");
    }

    match position {
        ManagedBlockPosition::Top => format!("{block}\n\n{}\n", content.trim()),
        ManagedBlockPosition::Bottom => format!("{}\n\n{block}\n", content.trim_end()),
        ManagedBlockPosition::BeforeMarker(marker) => {
            insert_managed_block_before_marker(content, block, marker)
        }
    }
}

fn insert_managed_block_before_marker(content: &str, block: &str, marker: &str) -> String {
    let trimmed = content.trim_end();
    let Some(marker_start) = trimmed.find(marker) else {
        return format!("{trimmed}\n\n{block}\n");
    };

    let prefix = trimmed[..marker_start].trim_end_matches('\n');
    let suffix = trimmed[marker_start..].trim_start_matches('\n');
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => format!("{block}\n"),
        (true, false) => format!("{block}\n\n{suffix}\n"),
        (false, true) => format!("{prefix}\n\n{block}\n"),
        (false, false) => format!("{prefix}\n\n{block}\n\n{suffix}\n"),
    }
}
