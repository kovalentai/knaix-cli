//! `knaix mcp`: the config block that points an MCP client at your node.
//!
//! The node speaks the Model Context Protocol and knows nothing about any
//! particular client, so any of them can search a knowledge base, ask it
//! grounded questions, and read its documents. What stands between a user and
//! that is a URL and a key in the right shape, which is exactly the kind of
//! thing people get wrong once and give up on.
//!
//! So the output is organised by the three shapes a client asks for its config
//! in -- a command, an HTTP server object, a stdio bridge -- with clients named
//! only as examples. A list of vendors would misdescribe what the node does and
//! would be wrong the week a new client appears.
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
//!   very likely cannot even reach it, since the endpoint is on the tenant's
//!   tailnet.
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
///
/// The surface is checked before the key is minted, and the order is the point.
/// Installing a key replaces the node's entire key set, so doing it first and
/// discovering afterwards that the node cannot serve MCP would destroy a working
/// key in exchange for nothing.
async fn local(ctx: &KnaixContext, base: &str, instance_id: &str) -> Result<()> {
    require_mcp_surface(ctx, base).await?;

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

/// Refuse early if the node cannot serve MCP, and say which reason it is.
///
/// A `GET` on the endpoint is the whole probe. It changes nothing, needs no key,
/// and the three answers a node can give are already distinct:
///
/// - **405**: served. The endpoint is stateless and offers no SSE stream, so
///   the spec has it refuse a GET; reaching that refusal proves the route is
///   mounted. A node that later grows a stream would answer 200, which is just
///   as good an answer to the only question being asked.
/// - **404**: no MCP surface. The node predates it. This is the case worth
///   catching, because the credential push it would otherwise be followed by
///   succeeds on these nodes, leaving a printed config that looks ready and
///   fails in the user's editor instead.
/// - **503**: mounted but refusing, because the node is not bound to an
///   instance. Telling this user to pull a newer image would send them to fix
///   something that is not broken.
///
/// Anything else is treated as served: the question is whether the route exists,
/// and a node answering 401 or 400 has answered it.
async fn require_mcp_surface(ctx: &KnaixContext, base: &str) -> Result<()> {
    let url = format!("{}/mcp", base);
    let response = ctx.client.get(&url).send().await.with_context(|| {
        format!(
            "Could not reach the local node at {}. Is it running? Try '{}'.",
            base,
            crate::brand::cmd("local up")
        )
    })?;

    match response.status().as_u16() {
        404 => Err(anyhow!(
            "This node does not serve MCP. Its image predates the endpoint, so there \
             is nothing for a client to connect to yet.\n       Fetch the current \
             runtime with '{}', then run this again.",
            crate::brand::cmd("local up --pull")
        )),
        503 => Err(anyhow!(
            "This node is running but not bound to an instance, so it refuses every \
             route including MCP.\n       Restart it with '{}'.",
            crate::brand::cmd("local up")
        )),
        _ => Ok(()),
    }
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
/// of trusting the status code, and pushes again one past whatever the node
/// turned out to be holding. Without that, two runs in the same instant printed
/// a second key that had never been installed and answered 401 on first use.
///
/// The first push deliberately carries version 0, the lowest a node can accept.
/// A timestamp would be simpler and is wrong: the control plane versions its own
/// pushes with a per-instance counter that increments by one, so a key set
/// stamped with the current epoch would sit billions of versions above anything
/// it can ever send, and every later push it made -- including a revocation --
/// would be discarded while it reported success. Staying in the counter's number
/// space costs one extra round trip against loopback and cannot poison it.
async fn install_key(ctx: &KnaixContext, base: &str, instance_id: &str, token: &str) -> Result<()> {
    let first = push_key(ctx, base, instance_id, token, 0).await?;
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

/// The three shapes a client asks for its config in.
///
/// Named by shape, not by vendor. The node implements the protocol and nothing
/// else, so what a user needs is the shape their client takes: a command, an
/// HTTP server object, or a stdio bridge for a client that only launches local
/// processes. Every client falls into one, including ones that do not exist yet.
///
/// Two facts are all any of them carry -- the node's URL and the key as an
/// `Authorization` header -- so a client whose format is none of these three is
/// still one substitution away.
///
/// The wrapper key is the one thing that genuinely differs between config-file
/// clients: most take `mcpServers`, and VS Code takes `servers`. It is called
/// out rather than picked, because guessing wrong produces a file the client
/// silently ignores.
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

    println!(
        "{}",
        "A command, for a client that registers servers itself".bold()
    );
    println!("  {} Claude Code", "·".dimmed());
    println!(
        "  claude mcp add --transport http {} {} --header \"Authorization: Bearer {}\"",
        SERVER_NAME, url, token
    );
    println!();
    println!("{}", "An HTTP server object, for a config file".bold());
    println!(
        "  {} Cursor, Windsurf and most others use {}; VS Code uses {}",
        "·".dimmed(),
        "\"mcpServers\"".cyan(),
        "\"servers\"".cyan()
    );
    println!("{}", rendered);
    println!();
    println!(
        "{}",
        "A stdio bridge, for a client that cannot dial HTTP".bold()
    );
    println!(
        "  {} Claude Desktop, and anything else that only launches local processes",
        "·".dimmed()
    );
    println!("{}", bridge_block(url, token)?);
    Ok(())
}

/// A stdio bridge to the node's HTTP endpoint, for a client that launches
/// servers as local processes and cannot dial one.
///
/// The header argument carries no space around the colon and its value lives in
/// `env`: several clients mangle spaces inside `args` when they invoke npx, and
/// the token would arrive truncated.
fn bridge_block(url: &str, token: &str) -> Result<String> {
    let block = serde_json::json!({
        "mcpServers": {
            SERVER_NAME: {
                "command": "npx",
                "args": ["-y", "mcp-remote", url, "--header", "Authorization:${AUTH_HEADER}"],
                "env": { "AUTH_HEADER": format!("Bearer {}", token) }
            }
        }
    });
    Ok(serde_json::to_string_pretty(&block)?)
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

    /// Serve one canned status on a loopback port and return its base URL.
    /// Records how many requests arrived, which is how the ordering assertion
    /// below tells a probe that refused from one that went on to push a key.
    fn node_answering(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<usize>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let counted = hits.clone();

        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                *counted.lock().unwrap() += 1;
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        (format!("http://127.0.0.1:{}", port), hits)
    }

    /// The answer a node that serves MCP actually gives a GET: the endpoint is
    /// stateless, so the spec has it refuse the stream. Verified against a real
    /// Node Runtime, which is where this status comes from.
    #[tokio::test]
    async fn treats_the_spec_s_refusal_of_a_get_as_proof_the_route_is_mounted() {
        let (base, _) = node_answering(
            "405 Method Not Allowed",
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"This endpoint offers no SSE stream."}}"#,
        );
        let ctx = KnaixContext::new("text".to_string());
        assert!(require_mcp_surface(&ctx, &base).await.is_ok());
    }

    /// The case this probe exists for. A node whose image predates MCP still
    /// accepts the credential push, so without the probe the command installs a
    /// key, prints a config, and fails only once the user opens their editor.
    #[tokio::test]
    async fn names_the_remedy_when_the_node_predates_the_endpoint() {
        let (base, _) = node_answering("404 Not Found", r#"{"error":"not found"}"#);
        let ctx = KnaixContext::new("text".to_string());

        let err = require_mcp_surface(&ctx, &base)
            .await
            .expect_err("a node without the route must not be printed a config");
        let message = err.to_string();
        assert!(message.contains("predates"), "{}", message);
        assert!(
            message.contains("--pull"),
            "the remedy must be named: {}",
            message
        );
    }

    /// An unbound node has the route and refuses it. Telling this user to pull a
    /// newer image would send them to fix something that is not broken.
    #[tokio::test]
    async fn tells_an_unbound_node_apart_from_an_old_one() {
        let (base, _) = node_answering(
            "503 Service Unavailable",
            r#"{"error":"node is not bound to an instance (KB_INSTANCE_ID unset); refusing routes"}"#,
        );
        let ctx = KnaixContext::new("text".to_string());

        let message = require_mcp_surface(&ctx, &base)
            .await
            .expect_err("an unbound node cannot serve MCP either")
            .to_string();
        assert!(message.contains("not bound"), "{}", message);
        assert!(
            !message.contains("--pull"),
            "an unbound node is not an old one: {}",
            message
        );
    }

    /// The whole reason the probe runs before the mint: installing a key
    /// replaces the node's entire key set, so a command that cannot succeed must
    /// not have touched it.
    #[tokio::test]
    async fn refuses_before_it_can_replace_the_node_s_key_set() {
        let (base, hits) = node_answering("404 Not Found", r#"{"error":"not found"}"#);
        let ctx = KnaixContext::new("text".to_string());

        local(&ctx, &base, "3f2504e0-4f89-41d3-9a0c-0305e82c3301")
            .await
            .expect_err("a node without MCP must not get a key installed");

        assert_eq!(
            *hits.lock().unwrap(),
            1,
            "only the probe should have been sent; a credential push would be a \
             second request and would have replaced the node's key set"
        );
    }

    /// The staleness rule the node applies is `version <= held`, so a push has
    /// to clear what the node holds -- and must not overshoot it, because the
    /// control plane versions its own pushes with a counter that steps by one
    /// and could never catch up to a timestamp. Both halves are pinned here:
    /// the first push starts at 0, and a discarded one is retried at held + 1.
    #[tokio::test]
    async fn retries_one_past_the_version_the_node_turned_out_to_hold() {
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
        assert_eq!(versions[0], 0, "the probe must start at the lowest version");
        assert_eq!(
            versions[1], 9001,
            "the retry must clear the held version by exactly one, staying in \
             the control plane's counter space"
        );
    }
}
