use std::time::Duration;

use eyre::WrapErr as _;
use google_cloud_compute_v1::client::Instances;
use google_cloud_lro::Poller as _;

const METADATA_BASE_URL: &str = "http://metadata.google.internal/computeMetadata/v1";
const METADATA_HEADER: (&str, &str) = ("Metadata-Flavor", "Google");

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GceIdentity {
    pub project_id: String,
    pub zone: String,
    pub instance_name: String,
}

fn parse_zone(raw: &str) -> eyre::Result<String> {
    // Example: "projects/123456/zones/europe-west1-b"
    let trimmed = raw.trim();
    let zone = trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("invalid metadata zone value: {raw:?}"))?;
    Ok(zone.to_string())
}

async fn metadata_get(client: &reqwest::Client, path: &str) -> eyre::Result<String> {
    let url = format!("{METADATA_BASE_URL}/{path}");
    let resp = client
        .get(&url)
        .header(METADATA_HEADER.0, METADATA_HEADER.1)
        .send()
        .await
        .wrap_err_with(|| format!("failed to query GCE metadata {url:?}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        eyre::bail!("metadata request failed (status={status}): {body}");
    }

    Ok(body.trim().to_string())
}

pub async fn detect_identity(client: &reqwest::Client) -> eyre::Result<GceIdentity> {
    let project_id = metadata_get(client, "project/project-id")
        .await
        .wrap_err("failed to read project id from metadata")?;
    let zone_raw = metadata_get(client, "instance/zone")
        .await
        .wrap_err("failed to read zone from metadata")?;
    let instance_name = metadata_get(client, "instance/name")
        .await
        .wrap_err("failed to read instance name from metadata")?;

    Ok(GceIdentity {
        project_id,
        zone: parse_zone(&zone_raw)?,
        instance_name,
    })
}

async fn suspend_instance(
    identity: &GceIdentity,
    discard_local_ssd: bool,
    request_id: uuid::Uuid,
) -> eyre::Result<()> {
    let client = Instances::builder()
        .build()
        .await
        .wrap_err("failed to build Compute Engine instances client")?;

    tracing::info!(
        target: "reth::cli",
        project_id = %identity.project_id,
        zone = %identity.zone,
        instance = %identity.instance_name,
        discard_local_ssd,
        request_id = %request_id,
        "beth-idle: calling Compute Engine instances.suspend"
    );

    client
        .suspend()
        .set_project(identity.project_id.clone())
        .set_zone(identity.zone.clone())
        .set_instance(identity.instance_name.clone())
        .set_request_id(request_id.to_string())
        .set_discard_local_ssd(discard_local_ssd)
        .poller()
        .until_done()
        .await
        .wrap_err("Compute Engine instances.suspend operation failed")?;

    tracing::info!(
        target: "reth::cli",
        request_id = %request_id,
        "beth-idle: Compute Engine suspend operation completed"
    );

    Ok(())
}

pub async fn suspend_self(discard_local_ssd: bool, request_id: uuid::Uuid) -> eyre::Result<()> {
    let http = reqwest::Client::builder()
        .user_agent("beth-idle")
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .build()
        .wrap_err("failed to build reqwest client")?;

    let identity = detect_identity(&http).await?;

    tracing::info!(
        target: "reth::cli",
        project_id = %identity.project_id,
        zone = %identity.zone,
        instance = %identity.instance_name,
        discard_local_ssd,
        request_id = %request_id,
        "beth-idle: resolved GCE identity; suspending self"
    );

    suspend_instance(&identity, discard_local_ssd, request_id).await
}

#[cfg(test)]
mod tests {
    use super::parse_zone;

    #[test]
    fn parses_zone_from_metadata_path() {
        let zone = parse_zone("projects/123456/zones/europe-west1-b").unwrap();
        assert_eq!(zone, "europe-west1-b");
    }

    #[test]
    fn parses_plain_zone() {
        let zone = parse_zone("europe-west1-b").unwrap();
        assert_eq!(zone, "europe-west1-b");
    }

    #[test]
    fn rejects_empty_zone() {
        assert!(parse_zone("").is_err());
        assert!(parse_zone("projects/123456/zones/").is_err());
    }
}
