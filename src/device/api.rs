//! The control-plane routes a device uses (`apps/api/src/routes/compute.ts`).

use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Context, Result};
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a device says about itself, at enrolment and on every heartbeat.
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub backend: String,
    pub total_memory_bytes: u64,
    pub usable_memory_bytes: u64,
    pub llama_build: Option<String>,
    /// `<tailnet ip>:<gateway port>`, or None while the gateway is not serving.
    pub rpc_endpoint: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EnrollBody<'a> {
    name: &'a str,
    platform: &'a str,
    arch: &'a str,
    #[serde(flatten)]
    report: &'a Report,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TicketKey {
    pub key_id: String,
    pub public_key_pem: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Enrolled {
    pub device: Value,
    pub token: String,
    pub ticket_key: Option<TicketKey>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Beat {
    pub next_heartbeat_seconds: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub ticket_key_id: Option<String>,
}

/// What the control plane said when it would not mint a join key.
#[derive(Debug, PartialEq)]
pub enum JoinKey {
    /// A key for Kovalent's tailnet, carrying the compute tag.
    Managed { auth_key: String, tag: String },
    /// The account uses its own tailnet: the owner joins the device to it.
    OwnTailnet,
}

/// The most specific message an error body carries.
pub fn error_message(body: &Value) -> Option<String> {
    for key in ["details", "message", "error"] {
        if let Some(s) = body.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

async fn failure(resp: reqwest::Response, what: &str) -> anyhow::Error {
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or_default();
    let detail = error_message(&body).unwrap_or_else(|| format!("HTTP {status}"));
    let message = if status == 403 && detail.contains("plan") {
        "Your plan does not include pooled inference. Compute devices are available on Personal and above.".to_string()
    } else {
        format!("{what}: {detail}")
    };
    let failed: Result<()> = Err(anyhow::Error::new(Failure(message)));
    failed.coded(Code::for_status(status)).unwrap_err()
}

/// A plain message, so `anyhow` renders it without a "caused by" line.
#[derive(Debug)]
pub struct Failure(pub String);
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Failure {}

pub async fn enroll(
    client: &reqwest::Client,
    api_url: &str,
    session: &str,
    name: &str,
    report: &Report,
) -> Result<Enrolled> {
    let body = EnrollBody {
        name,
        platform: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        report,
    };
    let resp = client
        .post(format!("{api_url}/api/compute/devices"))
        .header(AUTHORIZATION, format!("Bearer {session}"))
        .json(&body)
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    if !resp.status().is_success() {
        return Err(failure(resp, "Enrolling this machine failed").await);
    }
    resp.json()
        .await
        .context("The enrolment answer was not understood")
}

pub async fn heartbeat(
    client: &reqwest::Client,
    api_url: &str,
    device_token: &str,
    report: &Report,
) -> Result<Beat> {
    let resp = client
        .post(format!("{api_url}/api/compute/devices/heartbeat"))
        .header(AUTHORIZATION, format!("Bearer {device_token}"))
        .json(report)
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    if resp.status().as_u16() == 401 {
        return Err(anyhow!(
            "The control plane no longer recognizes this device. It was removed from the account; run `knaix device join` to enrol it again."
        ))
        .coded(Code::Auth);
    }
    if !resp.status().is_success() {
        return Err(failure(resp, "The heartbeat was refused").await);
    }
    Ok(resp.json().await.unwrap_or_default())
}

pub async fn list(client: &reqwest::Client, api_url: &str, session: &str) -> Result<Value> {
    let resp = client
        .get(format!("{api_url}/api/compute/devices"))
        .header(AUTHORIZATION, format!("Bearer {session}"))
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    if !resp.status().is_success() {
        return Err(failure(resp, "Listing devices failed").await);
    }
    resp.json()
        .await
        .context("The device list was not understood")
}

pub async fn remove(
    client: &reqwest::Client,
    api_url: &str,
    session: &str,
    id: &str,
) -> Result<()> {
    let resp = client
        .delete(format!("{api_url}/api/compute/devices/{id}"))
        .header(AUTHORIZATION, format!("Bearer {session}"))
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    // Already gone is the outcome leaving wants.
    if resp.status().is_success() || resp.status().as_u16() == 404 {
        return Ok(());
    }
    Err(failure(resp, "Removing the device from your account failed").await)
}

pub async fn join_key(client: &reqwest::Client, api_url: &str, session: &str) -> Result<JoinKey> {
    let resp = client
        .post(format!("{api_url}/api/compute/join-key"))
        .header(AUTHORIZATION, format!("Bearer {session}"))
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    let status = resp.status().as_u16();
    if status == 409 {
        let body: Value = resp.json().await.unwrap_or_default();
        let detail = error_message(&body).unwrap_or_default();
        if detail.contains("own tailnet") {
            return Ok(JoinKey::OwnTailnet);
        }
        return Err(anyhow!(Failure(format!(
            "No tailnet is set up for this account yet: {detail}"
        ))))
        .coded(Code::Precondition);
    }
    if !(200..300).contains(&status) {
        return Err(failure(resp, "Getting a tailnet key failed").await);
    }
    let body: Value = resp
        .json()
        .await
        .context("The join key answer was not understood")?;
    let auth_key = body["authKey"].as_str().unwrap_or_default().to_string();
    let tag = body["tag"].as_str().unwrap_or_default().to_string();
    if auth_key.is_empty() || !tag.starts_with("tag:c-") {
        return Err(anyhow!(
            "The control plane returned a join key without the compute tag"
        ));
    }
    Ok(JoinKey::Managed { auth_key, tag })
}

pub async fn ticket_key(client: &reqwest::Client, api_url: &str) -> Result<TicketKey> {
    let resp = client
        .get(format!("{api_url}/api/compute/ticket-key"))
        .send()
        .await
        .context("Could not reach the Kovalent API")?;
    if resp.status().as_u16() == 503 {
        return Err(anyhow!(
            "The control plane is not issuing tunnel tickets right now, so pools cannot use any device. Try again later."
        ))
        .coded(Code::Unavailable);
    }
    if !resp.status().is_success() {
        return Err(failure(resp, "Fetching the ticket key failed").await);
    }
    resp.json()
        .await
        .context("The ticket key answer was not understood")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_uses_the_api_field_names() {
        let r = Report {
            backend: "metal".into(),
            total_memory_bytes: 2,
            usable_memory_bytes: 1,
            llama_build: Some("11669-620d222".into()),
            rpc_endpoint: Some("100.64.0.9:47100".into()),
        };
        let v = serde_json::to_value(EnrollBody {
            name: "studio",
            platform: "macos",
            arch: "aarch64",
            report: &r,
        })
        .unwrap();
        for key in [
            "name",
            "platform",
            "arch",
            "backend",
            "totalMemoryBytes",
            "usableMemoryBytes",
            "llamaBuild",
            "rpcEndpoint",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
    }

    #[test]
    fn the_most_specific_error_wins() {
        let body = serde_json::json!({"error": "Validation failed (Code: KV-VAL-001)", "message": "Validation failed", "details": "An account holds at most 32 devices"});
        assert_eq!(
            error_message(&body).as_deref(),
            Some("An account holds at most 32 devices")
        );
        assert_eq!(
            error_message(&serde_json::json!({"error": "device_unauthorized"})).as_deref(),
            Some("device_unauthorized")
        );
        assert_eq!(error_message(&serde_json::json!({})), None);
    }

    #[test]
    fn a_heartbeat_answer_reads_the_key_id() {
        let b: Beat = serde_json::from_str(r#"{"success":true,"nextHeartbeatSeconds":15,"warnings":[],"ticketKeyId":"c1d40ab1863aca6e"}"#).unwrap();
        assert_eq!(b.ticket_key_id.as_deref(), Some("c1d40ab1863aca6e"));
        assert_eq!(b.next_heartbeat_seconds, Some(15));
    }
}
