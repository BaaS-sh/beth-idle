use std::time::Duration;

use eyre::WrapErr as _;

use crate::retry::{retry_with_backoff, RetryError};

const METADATA_BASE_URL: &str = "http://metadata.google.internal/computeMetadata/v1";
const METADATA_HEADER: (&str, &str) = ("Metadata-Flavor", "Google");

const GCE_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

const DEFAULT_MAX_RETRIES: usize = 5;
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(1);

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

fn compute_suspend_url(identity: &GceIdentity, discard_local_ssd: bool, request_id: &str) -> String {
    let base = if discard_local_ssd {
        // v1 supports discarding local SSD data.
        "https://compute.googleapis.com/compute/v1"
    } else {
        // `discardLocalSsd=false` is currently documented on the beta endpoint.
        "https://compute.googleapis.com/compute/beta"
    };

    format!(
        "{base}/projects/{}/zones/{}/instances/{}/suspend?discardLocalSsd={}&requestId={}",
        identity.project_id,
        identity.zone,
        identity.instance_name,
        discard_local_ssd,
        request_id
    )
}

async fn suspend_instance(
    client: &reqwest::Client,
    identity: &GceIdentity,
    token: &str,
    discard_local_ssd: bool,
    request_id: &str,
) -> Result<(), RetryError> {
    let url = compute_suspend_url(identity, discard_local_ssd, request_id);
    let resp = client.post(&url).bearer_auth(token).send().await.map_err(|e| {
        RetryError::retryable(eyre::eyre!("failed to send compute suspend request: {e}"))
    })?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        tracing::info!(
            target: "reth::cli",
            status = %status,
            "beth-idle: GCE suspend request accepted"
        );
        if !body.trim().is_empty() {
            tracing::debug!(target: "reth::cli", body = %body, "beth-idle: GCE suspend response body");
        }
        return Ok(());
    }

    // If the instance is already suspending/suspended, treat this as success (idempotent behavior).
    if status.as_u16() == 409 {
        tracing::warn!(
            target: "reth::cli",
            status = %status,
            body = %body,
            "beth-idle: suspend returned conflict; assuming instance is already suspending/suspended"
        );
        return Ok(());
    }

    let report = eyre::eyre!("compute suspend failed (status={status}): {body}");
    if status.is_server_error() || status.as_u16() == 429 {
        Err(RetryError::retryable(report))
    } else {
        Err(RetryError::permanent(report))
    }
}

pub async fn suspend_self(discard_local_ssd: bool, request_id: uuid::Uuid) -> eyre::Result<()> {
    let http = reqwest::Client::builder()
        .user_agent("beth-idle")
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .build()
        .wrap_err("failed to build reqwest client")?;

    let identity = detect_identity(&http).await?;

    let provider = gcp_auth::provider()
        .await
        .wrap_err("failed to initialize gcp_auth provider")?;

    let token = provider
        .token(&[GCE_SCOPE])
        .await
        .wrap_err("failed to fetch GCE access token via metadata/ADC")?;

    tracing::info!(
        target: "reth::cli",
        project_id = %identity.project_id,
        zone = %identity.zone,
        instance = %identity.instance_name,
        discard_local_ssd,
        request_id = %request_id,
        "beth-idle: resolved GCE identity; suspending instance"
    );

    retry_with_backoff(
        |attempt| {
            let http = http.clone();
            let identity = identity.clone();
            let token = token.clone();
            let request_id = request_id.to_string();
            async move {
                tracing::info!(
                    target: "reth::cli",
                    attempt,
                    "beth-idle: calling Compute Engine instances.suspend"
                );
                suspend_instance(
                    &http,
                    &identity,
                    token.as_str(),
                    discard_local_ssd,
                    &request_id,
                )
                .await
            }
        },
        DEFAULT_MAX_RETRIES,
        DEFAULT_HTTP_TIMEOUT,
        DEFAULT_BACKOFF_BASE,
    )
    .await
}
