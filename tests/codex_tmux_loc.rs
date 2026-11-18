#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::{PermissionsExt, symlink},
    process::Command,
    time::SystemTime,
};

#[test]
fn tmux_status_prefers_system_tokei_and_falls_back_without_jq() -> anyhow::Result<()> {
    let id = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let dir = env::temp_dir().join(format!("za-tmux-loc-{}-{id}", std::process::id()));
    let workspace = dir.join("sample project");
    let bin = dir.join("user's bin");
    let runtime = dir.join("runtime");
    fs::create_dir_all(&workspace)?;
    fs::create_dir(&bin)?;
    // The attach command only needs successful tmux option writes to generate its helper.
    let path = env::var_os("PATH").unwrap_or_default();
    for (name, target) in [
        ("tmux", "true"),
        ("awk", "awk"),
        ("date", "date"),
        ("mkdir", "mkdir"),
        ("mv", "mv"),
    ] {
        let executable = env::split_paths(&path)
            .map(|dir| dir.join(target))
            .find(|p| p.is_file())
            .unwrap();
        symlink(executable, bin.join(name))?;
    }
    symlink(env!("CARGO_BIN_EXE_za"), bin.join("za"))?;
    let setup = Command::new(bin.join("za"))
        .args(["codex", "attach"])
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", &dir)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()?;
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let helper = fs::read_dir(runtime.join("codex-status"))?
        .next()
        .unwrap()?
        .path();
    let cache = helper.with_extension("loc");
    let status = |ttl: &str| -> anyhow::Result<String> {
        let result = Command::new(&helper)
            .arg(&workspace)
            .arg(&cache)
            .arg(ttl)
            .current_dir(&dir)
            .env_clear()
            .env("PATH", &bin)
            .env("HOME", &dir)
            .output()?;
        assert!(result.status.success());
        Ok(String::from_utf8(result.stdout)?.trim_end().to_owned())
    };

    let rust = workspace.join("main.rs");
    let web = workspace.join("ui.ts");
    fs::write(&rust, "// comment\n\nfn main() {}\n")?;
    fs::write(&web, "/* comment */\nconst x = 1;\n")?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    fs::write(&cache, format!("{now} 99 99 0\n"))?;
    let measured = "[diff +0 -0]  loc 2 | rust 1 | web 1";
    assert_eq!(status("60")?, measured);

    let tokei = bin.join("tokei");
    fs::write(
        &tokei,
        r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = --output ] && [ "$2" = json ] && [ "$3" = . ] && [ -f main.rs ] || exit 1
IFS= read -r report < "$HOME/report" || exit 1
printf '%s\n' "$report"
[ ! -f "$HOME/failed" ]
"#,
    )?;
    fs::set_permissions(&tokei, fs::Permissions::from_mode(0o755))?;
    let report = dir.join("report");
    let external = r#"{"Total":{"code":12},"Rust":{"code":7},"TSX":{"code":3},"CSS":{"code":2}}"#;
    fs::write(&report, format!("{external}\n"))?;
    assert_eq!(status("0")?, "[diff +0 -0]  loc 12 | rust 7 | web 5");
    fs::write(dir.join("failed"), "")?;
    assert_eq!(status("0")?, measured);
    fs::remove_file(dir.join("failed"))?;
    for invalid in [
        "invalid json",
        "{}",
        r#"{"Total":{"code":0,"inaccurate":true}}"#,
    ] {
        fs::write(&report, format!("{invalid}\n"))?;
        assert_eq!(status("0")?, measured, "{invalid}");
    }
    fs::remove_file(tokei)?;
    fs::remove_file(rust)?;
    fs::write(web, "// comment only\n")?;
    assert_eq!(status("60")?, measured);
    assert_eq!(status("0")?, "[diff +0 -0]  loc 0 | rust 0 | web 0");
    fs::remove_dir_all(dir)?;
    Ok(())
}
