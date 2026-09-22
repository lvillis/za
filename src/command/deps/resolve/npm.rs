use super::*;

pub(super) fn resolve_npm(query: &NpmPackageQuery) -> Result<NpmResolution> {
    let client = build_http_client(NPM_REGISTRY_BASE)?;
    let path = format!("/{}", encode_npm_package_for_url(&query.package));
    let mut req = client.get(path.clone());
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set npm registry user-agent")?;

    let response = req
        .send_response()
        .with_context(|| format!("request npm registry `{path}`"))?;
    let status = response.status();
    if !status.is_success() {
        let body = text_render::truncate_end(&response.text_lossy(), 200);
        if status.as_u16() == 404 {
            bail!(
                "npm package `{}` was not found. body: {body}",
                query.package
            );
        }
        bail!(
            "npm registry returned status {} for `{}`. body: {}",
            status,
            query.package,
            body
        );
    }

    let parsed = response
        .json::<NpmPackageResponse>()
        .with_context(|| format!("parse npm registry response for `{}`", query.package))?;
    let version = parsed
        .dist_tags
        .get(&query.tag)
        .cloned()
        .or_else(|| {
            parsed
                .versions
                .contains_key(&query.tag)
                .then(|| query.tag.clone())
        })
        .ok_or_else(|| {
            anyhow!(
                "npm package `{}` has no dist-tag or version `{}`",
                query.package,
                query.tag
            )
        })?;
    let package = parsed.name;
    Ok(build_npm_record(package, query.tag.clone(), version))
}
