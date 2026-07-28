//! `knaix mcp` — the config block that points an editor at your node.
//!
//! The node speaks the Model Context Protocol, so Claude Code, Claude Desktop
//! and anything else that speaks it can search a knowledge base, ask it grounded
//! questions, and read its documents. What stands between a user and that is a
//! URL and a key in the right JSON shape, which is exactly the kind of thing
//! people get wrong once and give up on.
//!
//! The two node kinds need different help, and pretending otherwise is what
//! would make this command useless:
//!
//! - A **local** node has no control plane to issue a key, so this mints one
//!   here and installs it on the node directly. That path exists because the
//!   node's credential surface is unauthenticated on loopback by design (`knaix
//!   local up` sets `A2A_AUTH_DISABLED`), and it is only ever taken against
//!   127.0.0.1.
//! - A **hosted** node holds keys the control plane issued, and this machine
//!   very likely cannot even reach it — the endpoint is on the tenant's tailnet.
//!   So the block is printed with the real address and a placeholder key, and
//!   the output says plainly what else has to be true.

use anyhow::{anyhow, Context, Result};
use colored::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::nodes::{resolve_target, KnaixContext, Target};

/// Scheme prefix every Kovalent key carries. Mirrors the control plane's.
const TOKEN_PREFIX: &str = "kvk_";

/// What a minted local key may do: everything the MCP tools cover, since the
/// user minting it on their own machine is the tenant.
const LOCAL_SCOPES: [&str; 3] = ["chat", "knowledge:read", "knowledge:write"];

/// The name the config block gives the server. Short, because it prefixes every
/// tool name a model sees.
const SERVER_NAME: &str = "kovalent";

#[derive(Deserialize)]
struct ConnectionResponse {
    urls: ConnectionUrls,
}

#[derive(Deserialize)]
struct ConnectionUrls {
    /// Absent on a node with no runtime deployed to serve it.
    mcp: Option<String>,
}

/// Print the client config for a node, installing a key first when it is local.
pub async fn run(ctx: &KnaixContext, node_id: Option<String>) -> Result<()> {
    let target = resolve_target(ctx, node_id)
        .await?
        .ok_or_else(|| anyhow!("No node selected. Run 'knaix list' to see your nodes."))?;

    match target {
        Target::Local { base, instance_id } => local(ctx, &base, &instance_id).await,
        Target::Remote { uuid } => remote(ctx, &uuid).await,
    }
}

/// A local node: mint a key, install it, and print a block that works as-is.
async fn local(ctx: &KnaixContext, base: &str, instance_id: &str) -> Result<()> {
    let token = mint_token();
    install_key(ctx, base, instance_id, &token).await?;

    let url = format!("{}/mcp", base);
    print_block(&url, &token, ctx.output_format == "json")?;

    if ctx.output_format != "json" {
        println!();
        println!(
            "{} This key was minted here and installed on the local node. Installing\n  \
             a key replaces the node's whole key set, so an earlier one stops working.",
            "Note:".blue()
        );
    }
    Ok(())
}

/// A hosted node: print its real address, and say what a working setup needs.
async fn remote(ctx: &KnaixContext, uuid: &str) -> Result<()> {
    let token = ctx.get_token()?;
    let url = format!("{}/api/nodes/{}/connection", ctx.config.api_url, uuid);

    let response = ctx
        .client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .context("Could not reach the Kovalent API")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Could not read that node's connection details ({}).",
            response.status()
        ));
    }

    let connection: ConnectionResponse = response
        .json()
        .await
        .context("The API returned connection details this version cannot read")?;

    let endpoint = connection.urls.mcp.ok_or_else(|| {
        anyhow!(
            "That node has no MCP endpoint. It is running a build older than the \
             Node Runtime that serves one; provision or upgrade it first."
        )
    })?;

    print_block(&endpoint, "kvk_YOUR_API_KEY", ctx.output_format == "json")?;

    if ctx.output_format != "json" {
        println!();
        println!(
            "{} Issue a key from the dashboard (Keys tab on the node) and paste it in\n  \
             place of the placeholder. Scopes decide what the client can do: {} to ask\n  \
             questions, {} to search and read documents, {} to add them.",
            "Next:".blue(),
            "chat".cyan(),
            "knowledge:read".cyan(),
            "knowledge:write".cyan()
        );
        println!(
            "{} That address is on your tailnet. The machine running the client has to\n  \
             be on it too, or the client will not reach the node.",
            "Note:".blue()
        );
    }
    Ok(())
}

/// Mint a `kvk_` token. Same shape as a control-plane key so the node's guard,
/// which checks the prefix before anything else, treats it identically.
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("failed to read OS randomness");
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("{}{}", TOKEN_PREFIX, hex)
}

/// A v4-shaped id for the key row, which the node's credential route validates.
fn new_key_id() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("failed to read OS randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].join(""),
        h[4..6].join(""),
        h[6..8].join(""),
        h[8..10].join(""),
        h[10..16].join("")
    )
}

/// Digest a token the way the node does, so the two agree on what matches.
/// Must stay byte-identical to `hashToken` on both the node and the control
/// plane: this produces the digest they compare against.
fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// What the node says it did with a pushed key set.
#[derive(Deserialize)]
struct SyncResponse {
    /// False when the node already holds a set at this version or newer, in
    /// which case the pushed key was discarded and does not work.
    applied: bool,
    held_version: u64,
}

/// Install the key on the local node.
///
/// A push only lands if its version is strictly newer than the one the node
/// holds; anything older or equal is discarded as stale, and the node reports
/// that in `applied` rather than as an error. So this reads the answer instead
/// of trusting the status code, and retries once just past whatever the node
/// turned out to be holding. Without that, two runs in the same instant printed
/// a second key that had never been installed and answered 401 on first use.
async fn install_key(ctx: &KnaixContext, base: &str, instance_id: &str, token: &str) -> Result<()> {
    let first = push_key(ctx, base, instance_id, token, now_millis()).await?;
    if first.applied {
        return Ok(());
    }

    let retry = push_key(ctx, base, instance_id, token, first.held_version + 1).await?;
    if retry.applied {
        return Ok(());
    }
    Err(anyhow!(
        "The local node kept a newer key set (version {}) and did not install this key. \
         Run the command again.",
        retry.held_version
    ))
}

/// One credential push. The set is a replacement, so this is also what removes
/// whatever key the node held before.
async fn push_key(
    ctx: &KnaixContext,
    base: &str,
    instance_id: &str,
    token: &str,
    version: u64,
) -> Result<SyncResponse> {
    let url = format!("{}/api/credentials/sync", base);
    let body = serde_json::json!({
        "instance_id": instance_id,
        "version": version,
        "keys": [{
            "id": new_key_id(),
            "token_hash": hash_token(token),
            "scopes": LOCAL_SCOPES,
        }],
    });

    let response = ctx
        .client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| {
            format!(
                "Could not reach the local node at {}. Is it running? Try '{}'.",
                base,
                crate::brand::cmd("local up")
            )
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "The local node refused the key ({}): {}",
            status,
            detail.trim()
        ));
    }

    response
        .json::<SyncResponse>()
        .await
        .context("The local node answered the key push in a shape this version cannot read")
}

/// Milliseconds since the epoch, used as the key-set version. Requires no state
/// on this machine, which matters because the node is the only thing that knows
/// what version it currently holds. Milliseconds rather than seconds because two
/// runs in the same second is an ordinary thing to do and produced a collision.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The config every MCP client takes, and the one line that configures Claude
/// Code without editing a file at all.
fn print_block(url: &str, token: &str, json_only: bool) -> Result<()> {
    let block = serde_json::json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {}", token) }
            }
        }
    });
    let rendered = serde_json::to_string_pretty(&block)?;

    if json_only {
        println!("{}", rendered);
        return Ok(());
    }

    println!("{}", "Claude Code".bold());
    println!(
        "  claude mcp add --transport http {} {} --header \"Authorization: Bearer {}\"",
        SERVER_NAME, url, token
    );
    println!();
    println!(
        "{}",
        "Claude Desktop, Cursor, and anything else that reads a config file".bold()
    );
    println!("{}", rendered);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden vector asserted on the node and in the control plane. If this
    /// drifts, a key minted here hashes to something the node will never match
    /// and the printed config fails with an invalid-key error that names nothing
    /// useful.
    #[test]
    fn hashes_a_token_the_way_the_node_does() {
        assert_eq!(
            hash_token("kvk_golden_vector"),
            hex_sha256("kvk_golden_vector")
        );
        // Known-answer check, independent of the implementation above.
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn hex_sha256(input: &str) -> String {
        Sha256::digest(input.as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    #[test]
    fn mints_a_token_the_node_guard_will_accept() {
        let token = mint_token();
        assert!(token.starts_with(TOKEN_PREFIX));
        // 32 random bytes, hex-encoded.
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 64);
        assert_ne!(token, mint_token());
    }

    #[test]
    fn mints_key_ids_the_node_route_accepts_as_uuids() {
        let id = new_key_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().nth(14), Some('4'));
        assert!(matches!(id.chars().nth(19), Some('8' | '9' | 'a' | 'b')));
    }

    /// The staleness rule the node applies is `version <= held`, so two runs in
    /// the same instant used to produce a second key that was never installed
    /// and answered 401 on first use -- the worst shape for the bug to take,
    /// because the key the user just copied was the dead one. This pins both
    /// halves of the fix: the version is millisecond-scaled, and a discarded
    /// push is retried just past what the node turned out to be holding.
    #[test]
    fn versions_a_push_in_milliseconds_not_seconds() {
        // Seconds since the epoch passed 2 billion in 2033; milliseconds passed
        // it in 1970. Anything below this is the old seconds-scaled value.
        assert!(now_millis() > 1_600_000_000_000);
    }

    #[tokio::test]
    async fn retries_past_the_version_the_node_turned_out_to_hold() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        // First push is discarded as stale, second must carry held + 1.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let recorded = seen.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for (i, stream) in listener.incoming().enumerate().take(2) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let version = request
                    .split("\"version\":")
                    .nth(1)
                    .and_then(|rest| rest.split(&[',', '}'][..]).next())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap();
                recorded.lock().unwrap().push(version);

                let body = if i == 0 {
                    r#"{"accepted":1,"applied":false,"held_version":9000}"#
                } else {
                    r#"{"accepted":1,"applied":true,"held_version":9001}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let ctx = KnaixContext::new("text".to_string());
        let base = format!("http://127.0.0.1:{}", port);
        install_key(
            &ctx,
            &base,
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "kvk_test",
        )
        .await
        .expect("the retry should install the key");

        let versions = seen.lock().unwrap().clone();
        assert_eq!(versions.len(), 2, "a discarded push must be retried");
        assert_eq!(versions[1], 9001, "the retry must clear the version held");
    }
}
