use super::*;

pub(super) fn resolve_action(spec: &ActionSpec) -> Result<ActionResolution> {
    if is_full_commit_sha(&spec.ref_name) {
        return Ok(build_action_record(spec, spec.ref_name.clone(), "input"));
    }
    let api = crate::command::deps::api::ApiClient::new(false)?;
    let sha = api.resolve_github_ref(&spec.owner, &spec.repo, &spec.ref_name)?;
    Ok(build_action_record(spec, sha, "github"))
}
