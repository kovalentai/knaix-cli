use crate::config::{load_config, load_stored_config, save_config};
use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Extension, Query},
    response::Html,
    routing::get,
    Router,
};
use colored::*;
use serde::Deserialize;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// How long to wait for the browser before giving up. Long enough for an MFA
/// prompt and a password manager; short enough that a forgotten terminal does
/// not sit on a listening socket all day.
const LOGIN_TIMEOUT_SECS: u64 = 300;

#[derive(Deserialize)]
pub struct CallbackParams {
    token: String,
    username: String,
    #[serde(default)]
    knaix_state: String,
}

#[derive(Clone)]
struct AppState {
    completed: Arc<Mutex<bool>>,
    done: Arc<Notify>,
    expected_state: String,
}

/// Page shown when a callback arrives with a missing or mismatched state
/// nonce. The token is never stored in that case.
const REJECTED_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"UTF-8\"><title>Knaix</title></head><body style=\"font-family:sans-serif;text-align:center;padding:64px;color:#0f172a\"><h1>Authentication rejected</h1><p>This login callback did not match the request started by your terminal. You can close this window and run <code>knaix login</code> again.</p></body></html>";

/// The hosted dashboard that serves the CLI sign-in page in production.
const DEFAULT_AUTH_BASE: &str = "https://app.kovalentai.com/cli-auth";

/// Where to send the browser to sign in, derived from the API the CLI is
/// pointed at.
///
/// Login has to follow `api_url`, because the token the browser hands back is
/// only valid against the control plane that issued it. Sending someone at a
/// local stack to the production dashboard returns a token their API will
/// reject, which reads as a broken login rather than a misconfiguration. Only
/// the production API keeps the hosted dashboard; anything else serves its own
/// page on its own origin.
fn auth_base(api_url: &str) -> String {
    match url::Url::parse(api_url) {
        Ok(u) if u.host_str() == Some("api.kovalentai.com") => DEFAULT_AUTH_BASE.to_string(),
        Ok(mut u) => {
            u.set_path("/cli-auth");
            u.set_query(None);
            u.to_string()
        }
        Err(_) => DEFAULT_AUTH_BASE.to_string(),
    }
}

/// Generates a 256-bit random state token, hex-encoded, from the OS CSPRNG.
fn new_state_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("failed to read OS randomness");
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Constant-time string comparison, so a mismatched state nonce cannot be
/// recovered byte-by-byte from response timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn handle_callback(
    Query(params): Query<CallbackParams>,
    Extension(state): Extension<AppState>,
) -> Html<&'static str> {
    // CSRF protection: only accept a callback that echoes back the exact state
    // nonce this login attempt generated. Without it, any local page could hit
    // our loopback callback and inject an attacker-controlled token.
    if params.knaix_state.is_empty()
        || !constant_time_eq(&params.knaix_state, &state.expected_state)
    {
        eprintln!(
            "\n{} Ignored a login callback with an invalid state token.",
            "Error:".red()
        );
        return Html(REJECTED_HTML);
    }

    // Persist only the disk state plus the new session, so a KNAIX_API_URL set
    // for this shell does not become the user's saved API URL.
    let mut stored = load_stored_config();
    stored.token = Some(params.token.clone());
    stored.username = Some(params.username.clone());
    save_config(&stored).ok();

    println!("\n{} Successfully logged in!", "✓".green());
    println!("  Welcome, {}", params.username.cyan());
    println!(
        "  {} lists your nodes; {} makes one the default.",
        crate::brand::cmd("list"),
        crate::brand::cmd("use <node-id>")
    );

    *state.completed.lock().unwrap() = true;
    state.done.notify_one();

    Html(
        r###"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Kovalent CLI Authentication</title>
            <link rel="preconnect" href="https://fonts.googleapis.com">
            <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
            <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
            <style>
                * {
                    margin: 0;
                    padding: 0;
                    box-sizing: border-box;
                }

                body {
                    font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
                    background-color: #F8FAFC;
                    min-height: 100vh;
                    display: flex;
                    flex-direction: column;
                    align-items: center;
                    justify-content: center;
                    padding: 20px;
                }

                .container {
                    text-align: center;
                    max-width: 600px;
                    background: white;
                    border-radius: 24px;
                    padding: 64px 48px;
                    box-shadow: 0 4px 24px rgba(15, 23, 42, 0.08);
                }

                .logo {
                    width: 140px;
                    height: 140px;
                    margin: 0 auto 32px;
                }

                .logo svg {
                    width: 100%;
                    height: 100%;
                }

                .success-badge {
                    display: inline-flex;
                    align-items: center;
                    gap: 8px;
                    background: linear-gradient(135deg, rgba(245, 158, 11, 0.1) 0%, rgba(71, 197, 217, 0.1) 100%);
                    border: 2px solid transparent;
                    background-clip: padding-box, border-box;
                    background-origin: padding-box, border-box;
                    padding: 12px 24px;
                    border-radius: 100px;
                    margin-bottom: 24px;
                    font-size: 14px;
                    font-weight: 600;
                    background-image:
                        linear-gradient(white, white),
                        linear-gradient(135deg, #F59E0B 0%, #47C5D9 100%);
                }

                .success-icon {
                    font-size: 20px;
                    animation: bounce 0.6s ease-in-out;
                }

                @keyframes bounce {
                    0%, 100% { transform: scale(1); }
                    50% { transform: scale(1.2); }
                }

                h1 {
                    font-size: 42px;
                    font-weight: 700;
                    color: #0f172a;
                    margin-bottom: 16px;
                    line-height: 1.2;
                }

                .gradient-text {
                    background: linear-gradient(135deg, #F59E0B 20%, #47C5D9 80%);
                    -webkit-background-clip: text;
                    -webkit-text-fill-color: transparent;
                    background-clip: text;
                    display: inline-block;
                }

                p {
                    font-size: 18px;
                    color: #64748b;
                    line-height: 1.7;
                    margin-bottom: 16px;
                }

                .username {
                    font-weight: 600;
                    background: linear-gradient(135deg, #F59E0B 20%, #47C5D9 80%);
                    -webkit-background-clip: text;
                    -webkit-text-fill-color: transparent;
                    background-clip: text;
                }

                .close-notice {
                    margin-top: 40px;
                    padding: 20px 24px;
                    background: #f8fafc;
                    border: 1px solid #e2e8f0;
                    border-radius: 16px;
                    font-size: 15px;
                    color: #475569;
                    font-weight: 500;
                }

                .brand {
                    margin-top: 40px;
                    font-size: 13px;
                    color: #94a3b8;
                    font-weight: 500;
                }

                .feature-grid {
                    display: grid;
                    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
                    gap: 16px;
                    margin-top: 32px;
                }

                .feature {
                    padding: 16px;
                    background: #f8fafc;
                    border-radius: 12px;
                    font-size: 13px;
                    color: #64748b;
                }

                .feature-icon {
                    font-size: 24px;
                    margin-bottom: 8px;
                }
            </style>
        </head>
        <body>
            <div class="container">
                <div class="logo">
                    <svg version="1.1" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
                        <defs>
                            <linearGradient id="kovalentGrad" x1="300" y1="300" x2="800" y2="800" gradientUnits="userSpaceOnUse">
                                <stop offset="0%" stop-color="#F59E0B" />
                                <stop offset="100%" stop-color="#47C5D9" />
                            </linearGradient>
                        </defs>
                        <g>
                            <path d="M626.00,256.00 L708.18,303.45 L708.18,403.00 L468.91,541.15 L468.91,577.23 L443.88,562.78 L443.88,530.98 L684.12,392.28 L684.12,318.93 L626.71,285.79 L577.57,314.16 L577.57,364.02 L404.57,463.90 L404.57,555.06 L580.86,656.85 L580.86,705.01 L628.79,732.68 L679.97,703.13 L679.97,635.99 L520.25,543.78 L551.68,525.63 L710.55,617.36 L710.55,721.01 L631.28,766.78 L551.25,720.57 L551.25,676.00 L377.00,575.40 L377.00,450.98 L551.80,350.06 L551.80,301.99 L627.40,258.34 Z" fill="url(#kovalentGrad)"/>
                        </g>
                        <g>
                            <path d="M393.00,255.00 L473.35,301.39 L473.35,376.00 L444.72,392.53 L444.72,318.00 L393.37,288.35 L343.04,317.41 L343.04,705.00 L396.46,735.84 L445.53,707.50 L445.53,630.00 L473.51,646.15 L473.51,721.00 L395.01,766.32 L314.19,719.67 L314.19,302.00 L393.66,256.12 Z" fill="url(#kovalentGrad)"/>
                        </g>
                    </svg>
                </div>

                <div class="success-badge">
                    <span class="success-icon">
                        <svg width="20" height="20" viewBox="0 0 20 20" fill="none" xmlns="http://www.w3.org/2000/svg">
                            <circle cx="10" cy="10" r="9" stroke="url(#checkGrad)" stroke-width="1.5"/>
                            <path d="M6 10.5L8.5 13L14 7.5" stroke="url(#checkGrad)" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/>
                            <defs>
                                <linearGradient id="checkGrad" x1="0" y1="0" x2="20" y2="20" gradientUnits="userSpaceOnUse">
                                    <stop offset="0%" stop-color="#F59E0B"/>
                                    <stop offset="100%" stop-color="#47C5D9"/>
                                </linearGradient>
                            </defs>
                        </svg>
                    </span>
                    <span>Authenticated</span>
                </div>

                <h1>Welcome to <span class="gradient-text">Knaix</span></h1>
                <p>Your terminal is now connected to Kovalent's <strong>Private AI Stack</strong>.</p>
                <p>Hello, <span class="username">@username</span></p>

                <div class="feature-grid">
                    <div class="feature">
                        <div class="feature-icon">
                            <!-- Shield with lock: Zero Trust Mesh -->
                            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                                <defs>
                                    <linearGradient id="featGrad1" x1="0" y1="0" x2="24" y2="24" gradientUnits="userSpaceOnUse">
                                        <stop offset="0%" stop-color="#F59E0B"/>
                                        <stop offset="100%" stop-color="#47C5D9"/>
                                    </linearGradient>
                                </defs>
                                <path d="M12 2L4 6v6c0 5.25 3.5 10.15 8 11.35C16.5 22.15 20 17.25 20 12V6l-8-4z" stroke="url(#featGrad1)" stroke-width="1.5" stroke-linejoin="round" fill="none"/>
                                <rect x="9" y="10" width="6" height="5" rx="1" stroke="url(#featGrad1)" stroke-width="1.4" fill="none"/>
                                <path d="M10 10V8.5a2 2 0 1 1 4 0V10" stroke="url(#featGrad1)" stroke-width="1.4" stroke-linecap="round" fill="none"/>
                            </svg>
                        </div>
                        <div>Zero Trust Mesh</div>
                    </div>
                    <div class="feature">
                        <div class="feature-icon">
                            <!-- Lightning bolt: Low Latency -->
                            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                                <defs>
                                    <linearGradient id="featGrad2" x1="0" y1="0" x2="24" y2="24" gradientUnits="userSpaceOnUse">
                                        <stop offset="0%" stop-color="#F59E0B"/>
                                        <stop offset="100%" stop-color="#47C5D9"/>
                                    </linearGradient>
                                </defs>
                                <path d="M13 2L4.5 13.5H12L11 22L19.5 10.5H12L13 2z" stroke="url(#featGrad2)" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" fill="none"/>
                            </svg>
                        </div>
                        <div>Low Latency</div>
                    </div>
                    <div class="feature">
                        <div class="feature-icon">
                            <!-- Terminal prompt: CLI Ready -->
                            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                                <defs>
                                    <linearGradient id="featGrad3" x1="0" y1="0" x2="24" y2="24" gradientUnits="userSpaceOnUse">
                                        <stop offset="0%" stop-color="#F59E0B"/>
                                        <stop offset="100%" stop-color="#47C5D9"/>
                                    </linearGradient>
                                </defs>
                                <rect x="2" y="3" width="20" height="18" rx="3" stroke="url(#featGrad3)" stroke-width="1.5" fill="none"/>
                                <path d="M7 9l3.5 3.5L7 16" stroke="url(#featGrad3)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                                <path d="M13 16h4" stroke="url(#featGrad3)" stroke-width="1.5" stroke-linecap="round"/>
                            </svg>
                        </div>
                        <div>CLI Ready</div>
                    </div>
                </div>

                <div class="close-notice">
                    You can safely close this window and return to your terminal.
                </div>

                <div class="brand">Kovalent · Private AI Stack</div>
            </div>

            <script>
                // Update username dynamically
                const params = new URLSearchParams(window.location.search);
                const username = params.get('username') || 'user';
                document.querySelectorAll('.username').forEach(el => {
                    el.textContent = '@' + username;
                });
            </script>
        </body>
        </html>
    "###,
    )
}

pub async fn login() -> Result<()> {
    // Bind the callback server to an OS-assigned loopback port. A random port
    // (rather than a fixed 4242) means other local processes cannot predict the
    // callback URL to race or forge it.
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("Could not start the local callback server")?;
    let port = listener
        .local_addr()
        .context("Could not resolve the callback port")?
        .port();

    // Single-use nonce the browser redirect must echo back (CSRF protection).
    let expected_state = new_state_token();
    let done = Arc::new(Notify::new());

    let state = AppState {
        completed: Arc::new(Mutex::new(false)),
        done: done.clone(),
        expected_state: expected_state.clone(),
    };

    let app = Router::new()
        .route("/callback", get(handle_callback))
        .layer(Extension(state.clone()));

    let callback_url = format!(
        "http://127.0.0.1:{}/callback?knaix_state={}",
        port, expected_state
    );

    // Build the auth URL with the callback properly percent-encoded, so the
    // callback's own query string survives being nested as a parameter.
    let auth_url = url::Url::parse(&auth_base(&load_config().api_url))
        .map(|mut u| {
            u.query_pairs_mut().append_pair("callback", &callback_url);
            u.to_string()
        })
        .context("Invalid authentication URL")?;

    println!("{} Opening the browser to sign in.", "Info:".blue());
    println!("  If nothing opened, visit: {}\n", auth_url.dimmed());

    if let Err(e) = open::that(&auth_url) {
        println!(
            "{} Could not open the browser ({}). Visit the URL above to continue.",
            "Warning:".yellow(),
            e
        );
    }

    // axum 0.8 serves from a tokio listener, which must be non-blocking.
    listener
        .set_nonblocking(true)
        .context("Failed to start callback server")?;
    let listener =
        tokio::net::TcpListener::from_std(listener).context("Failed to start callback server")?;

    // Serve until the callback lands or the wait stops being plausible. The
    // hard timeout is what makes a walked-away-from login exit non-zero
    // instead of holding the port forever.
    tokio::select! {
        result = axum::serve(listener, app) => {
            result.context("The login callback server stopped unexpectedly")?;
        }
        _ = done.notified() => {
            // A beat for the browser to finish reading the success page
            // before the server goes away with the process.
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS)) => {
            return Err(anyhow!(
                "No sign-in completed within {} minutes. Run 'knaix login' to try again.",
                LOGIN_TIMEOUT_SECS / 60
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_api_keeps_the_hosted_dashboard() {
        assert_eq!(auth_base("https://api.kovalentai.com"), DEFAULT_AUTH_BASE);
        assert_eq!(
            auth_base("https://api.kovalentai.com/api"),
            DEFAULT_AUTH_BASE
        );
    }

    #[test]
    fn other_control_planes_serve_their_own_sign_in_page() {
        // The token a browser hands back is only valid against the control
        // plane that issued it, so login has to follow api_url.
        assert_eq!(
            auth_base("http://127.0.0.1:3002"),
            "http://127.0.0.1:3002/cli-auth"
        );
        assert_eq!(
            auth_base("https://api-sandbox.kovalentai.com/api"),
            "https://api-sandbox.kovalentai.com/cli-auth"
        );
    }

    #[test]
    fn a_query_string_on_the_api_url_is_not_carried_into_the_auth_url() {
        // It would collide with the callback parameter appended afterwards.
        assert_eq!(
            auth_base("http://localhost:3002/api?trace=1"),
            "http://localhost:3002/cli-auth"
        );
    }

    #[test]
    fn an_unparseable_api_url_falls_back_to_production() {
        assert_eq!(auth_base("not a url"), DEFAULT_AUTH_BASE);
        assert_eq!(auth_base(""), DEFAULT_AUTH_BASE);
    }

    #[test]
    fn state_token_is_256_bits_of_hex() {
        let token = new_state_token();
        assert_eq!(token.len(), 64, "expected 32 bytes as 64 hex chars");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn state_tokens_do_not_repeat() {
        let a = new_state_token();
        let b = new_state_token();
        assert_ne!(a, b, "each login attempt must get a fresh nonce");
    }

    #[test]
    fn constant_time_eq_matches_only_identical_strings() {
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        // Different lengths must not match and must not panic.
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abcd", "abc"));
    }

    fn state_with(expected: &str) -> AppState {
        AppState {
            completed: Arc::new(Mutex::new(false)),
            done: Arc::new(Notify::new()),
            expected_state: expected.to_string(),
        }
    }

    // The success branch of handle_callback writes to the real ~/.knaix
    // config, so these tests only exercise the rejection branch, which
    // returns before touching any state or the filesystem.

    #[tokio::test]
    async fn callback_with_missing_state_is_rejected() {
        let state = state_with("expected-nonce");
        let params = CallbackParams {
            token: "attacker-token".to_string(),
            username: "victim".to_string(),
            knaix_state: String::new(),
        };
        let resp = handle_callback(Query(params), Extension(state.clone())).await;
        assert_eq!(resp.0, REJECTED_HTML);
        assert!(
            !*state.completed.lock().unwrap(),
            "a callback without a state nonce must not complete login"
        );
    }

    #[tokio::test]
    async fn callback_with_mismatched_state_is_rejected() {
        let state = state_with("expected-nonce");
        let params = CallbackParams {
            token: "attacker-token".to_string(),
            username: "victim".to_string(),
            knaix_state: "wrong-nonce".to_string(),
        };
        let resp = handle_callback(Query(params), Extension(state.clone())).await;
        assert_eq!(resp.0, REJECTED_HTML);
        assert!(
            !*state.completed.lock().unwrap(),
            "a callback with the wrong state nonce must not complete login"
        );
    }
}
