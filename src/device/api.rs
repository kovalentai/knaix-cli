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
        // A blank field says nothing, so the next one speaks.
        assert_eq!(
            error_message(&serde_json::json!({"details": "  ", "message": "Too many devices"}))
                .as_deref(),
            Some("Too many devices")
        );
    }

    #[test]
    fn a_heartbeat_answer_reads_the_key_id() {
        let b: Beat = serde_json::from_str(r#"{"success":true,"nextHeartbeatSeconds":15,"warnings":[],"ticketKeyId":"c1d40ab1863aca6e"}"#).unwrap();
        assert_eq!(b.ticket_key_id.as_deref(), Some("c1d40ab1863aca6e"));
        assert_eq!(b.next_heartbeat_seconds, Some(15));
    }

    use crate::exit::code_of;
    use crate::testing::{fake_api, Call};
    use serde_json::{json, Value};

    fn report() -> Report {
        Report {
            backend: "cuda".into(),
            total_memory_bytes: 24,
            usable_memory_bytes: 20,
            llama_build: None,
            rpc_endpoint: Some("100.64.0.9:47100".into()),
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn enrolment_sends_this_machine_with_the_session_and_reads_the_answer() {
        let api = fake_api(vec![(
            "POST /api/compute/devices",
            201,
            json!({"device": {"id": "d1"}, "token": "kvd_t",
                   "ticketKey": {"keyId": "k1", "publicKeyPem": "pem"}, "warnings": ["w"]}),
        )])
        .await;

        let e = enroll(&client(), &api.url, "sess", "studio", &report())
            .await
            .unwrap();

        assert_eq!(
            (e.device["id"].as_str(), e.token.as_str()),
            (Some("d1"), "kvd_t")
        );
        assert_eq!(
            e.ticket_key,
            Some(TicketKey {
                key_id: "k1".into(),
                public_key_pem: "pem".into()
            })
        );
        assert_eq!(e.warnings, ["w"]);
        let call = &api.calls()[0];
        assert_eq!(call.auth.as_deref(), Some("Bearer sess"));
        assert_eq!(call.body["name"], "studio");
        assert_eq!(call.body["platform"], std::env::consts::OS);
        assert_eq!(call.body["usableMemoryBytes"], 20);
    }

    #[tokio::test]
    async fn a_refused_enrolment_says_why_and_a_plan_refusal_says_which_plans() {
        let refused = |status, body: Value| async move {
            let api = fake_api(vec![("POST /api/compute/devices", status, body)]).await;
            enroll(&client(), &api.url, "s", "n", &report())
                .await
                .unwrap_err()
        };

        let err = refused(403, json!({"error": "Your plan does not include this"})).await;
        assert_eq!(code_of(&err), Code::Auth);
        assert!(err
            .to_string()
            .starts_with("Your plan does not include pooled inference."));

        let err = refused(
            400,
            json!({"details": "An account holds at most 32 devices"}),
        )
        .await;
        assert_eq!(code_of(&err), Code::Error);
        assert_eq!(
            err.to_string(),
            "Enrolling this machine failed: An account holds at most 32 devices"
        );

        let err = refused(500, Value::Null).await;
        assert_eq!(err.to_string(), "Enrolling this machine failed: HTTP 500");

        let err = refused(201, json!({"unexpected": true})).await;
        assert_eq!(err.to_string(), "The enrolment answer was not understood");
    }

    #[tokio::test]
    async fn a_heartbeat_is_signed_by_the_device_and_a_removed_device_is_told() {
        let api = fake_api(vec![(
            "POST /api/compute/devices/heartbeat",
            200,
            json!({"nextHeartbeatSeconds": 15}),
        )])
        .await;
        let beat = heartbeat(&client(), &api.url, "kvd_t", &report())
            .await
            .unwrap();
        assert_eq!(beat.next_heartbeat_seconds, Some(15));
        assert_eq!(
            api.calls(),
            [Call {
                route: "POST /api/compute/devices/heartbeat".into(),
                auth: Some("Bearer kvd_t".into()),
                body: serde_json::to_value(report()).unwrap(),
            }]
        );

        let gone = fake_api(vec![(
            "POST /api/compute/devices/heartbeat",
            401,
            Value::Null,
        )])
        .await;
        let err = heartbeat(&client(), &gone.url, "kvd_t", &report())
            .await
            .unwrap_err();
        assert_eq!(code_of(&err), Code::Auth);
        assert!(err
            .to_string()
            .contains("run `knaix device join` to enrol it again"));

        let down = fake_api(vec![(
            "POST /api/compute/devices/heartbeat",
            500,
            Value::Null,
        )])
        .await;
        let err = heartbeat(&client(), &down.url, "kvd_t", &report())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "The heartbeat was refused: HTTP 500");

        // An answer it cannot read is still a heartbeat that landed.
        let odd = fake_api(vec![(
            "POST /api/compute/devices/heartbeat",
            200,
            json!("ok"),
        )])
        .await;
        let beat = heartbeat(&client(), &odd.url, "kvd_t", &report())
            .await
            .unwrap();
        assert_eq!(beat.next_heartbeat_seconds, None);
    }

    #[tokio::test]
    async fn listing_reads_the_accounts_devices_or_says_why_not() {
        let api = fake_api(vec![(
            "GET /api/compute/devices",
            200,
            json!({"devices": []}),
        )])
        .await;
        assert_eq!(
            list(&client(), &api.url, "s").await.unwrap(),
            json!({"devices": []})
        );
        assert_eq!(api.calls()[0].auth.as_deref(), Some("Bearer s"));

        let api = fake_api(vec![(
            "GET /api/compute/devices",
            401,
            json!({"error": "expired"}),
        )])
        .await;
        let err = list(&client(), &api.url, "s").await.unwrap_err();
        assert_eq!(
            (code_of(&err), err.to_string()),
            (Code::Auth, "Listing devices failed: expired".into())
        );

        let api = fake_api(vec![("GET /api/compute/devices", 200, Value::Null)]).await;
        let err = list(&client(), &api.url, "s").await.unwrap_err();
        assert_eq!(err.to_string(), "The device list was not understood");
    }

    #[tokio::test]
    async fn removing_a_device_that_is_already_gone_is_success() {
        let api = fake_api(vec![("DELETE /api/compute/devices/d1", 204, Value::Null)]).await;
        remove(&client(), &api.url, "s", "d1").await.unwrap();
        assert_eq!(api.calls()[0].auth.as_deref(), Some("Bearer s"));

        let api = fake_api(vec![]).await;
        remove(&client(), &api.url, "s", "d1").await.unwrap();

        let api = fake_api(vec![("DELETE /api/compute/devices/d1", 500, Value::Null)]).await;
        let err = remove(&client(), &api.url, "s", "d1").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Removing the device from your account failed: HTTP 500"
        );
    }

    async fn join_key_answer(status: u16, body: Value) -> Result<JoinKey> {
        let api = fake_api(vec![("POST /api/compute/join-key", status, body)]).await;
        join_key(&client(), &api.url, "s").await
    }

    #[tokio::test]
    async fn a_join_key_must_carry_the_compute_tag() {
        assert_eq!(
            join_key_answer(200, json!({"authKey": "tskey-1", "tag": "tag:c-acme"}))
                .await
                .unwrap(),
            JoinKey::Managed {
                auth_key: "tskey-1".into(),
                tag: "tag:c-acme".into()
            }
        );
        for body in [
            json!({"authKey": "tskey-1", "tag": "tag:admin"}),
            json!({"authKey": "", "tag": "tag:c-acme"}),
            json!({}),
        ] {
            let err = join_key_answer(200, body).await.unwrap_err();
            assert_eq!(
                err.to_string(),
                "The control plane returned a join key without the compute tag"
            );
        }
        let err = join_key_answer(200, Value::Null).await.unwrap_err();
        assert_eq!(err.to_string(), "The join key answer was not understood");
    }

    #[tokio::test]
    async fn an_account_on_its_own_tailnet_gets_no_key() {
        assert_eq!(
            join_key_answer(409, json!({"error": "This account uses its own tailnet"}))
                .await
                .unwrap(),
            JoinKey::OwnTailnet
        );
        let err = join_key_answer(409, json!({"error": "Tailnet not provisioned"}))
            .await
            .unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            "No tailnet is set up for this account yet: Tailnet not provisioned"
        );
        let err = join_key_answer(500, Value::Null).await.unwrap_err();
        assert_eq!(err.to_string(), "Getting a tailnet key failed: HTTP 500");
    }

    #[tokio::test]
    async fn the_ticket_key_is_public_and_its_outage_is_explained() {
        let api = fake_api(vec![(
            "GET /api/compute/ticket-key",
            200,
            json!({"keyId": "k1", "publicKeyPem": "pem"}),
        )])
        .await;
        let key = ticket_key(&client(), &api.url).await.unwrap();
        assert_eq!(
            key,
            TicketKey {
                key_id: "k1".into(),
                public_key_pem: "pem".into()
            }
        );
        assert_eq!(
            api.calls()[0].auth,
            None,
            "the key is fetched without a session"
        );

        let off = fake_api(vec![("GET /api/compute/ticket-key", 503, Value::Null)]).await;
        let err = ticket_key(&client(), &off.url).await.unwrap_err();
        assert_eq!(code_of(&err), Code::Unavailable);
        assert!(err
            .to_string()
            .starts_with("The control plane is not issuing tunnel tickets"));

        let missing = fake_api(vec![]).await;
        let err = ticket_key(&client(), &missing.url).await.unwrap_err();
        assert_eq!(
            (code_of(&err), err.to_string()),
            (
                Code::NotFound,
                "Fetching the ticket key failed: HTTP 404".into()
            )
        );

        let odd = fake_api(vec![("GET /api/compute/ticket-key", 200, json!({}))]).await;
        let err = ticket_key(&client(), &odd.url).await.unwrap_err();
        assert_eq!(err.to_string(), "The ticket key answer was not understood");
    }

    #[tokio::test]
    async fn an_api_that_is_not_listening_is_unavailable() {
        let dead = "http://127.0.0.1:9";
        let errors = [
            enroll(&client(), dead, "s", "n", &report()).await.err(),
            heartbeat(&client(), dead, "t", &report()).await.err(),
            list(&client(), dead, "s").await.err(),
            remove(&client(), dead, "s", "d").await.err(),
            join_key(&client(), dead, "s").await.err(),
            ticket_key(&client(), dead).await.err(),
        ];
        for err in errors.into_iter().map(Option::unwrap) {
            assert_eq!(code_of(&err), Code::Unavailable);
            assert_eq!(err.to_string(), "Could not reach the Kovalent API");
        }
    }
}
