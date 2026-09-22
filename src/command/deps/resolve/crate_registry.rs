use super::*;

pub(super) fn resolve_crate(name: &str) -> Result<CrateResolution> {
    let api = crate::command::deps::api::ApiClient::new(false)?;
    let snapshot = api.fetch_crate(name)?;
    let version = semver::Version::parse(&snapshot.max_version)?;
    if !version.pre.is_empty() {
        bail!("crate `{name}` has no stable release");
    }
    api.flush_cache()?;
    Ok(build_crate_record(name.to_string(), snapshot.max_version))
}
