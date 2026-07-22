use crate::config::{load_config, load_stored_config, save_config};
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
    expected_state: String,
}

/// Page shown when a callback arrives with a missing or mismatched state
/// nonce. The token is never stored in that case.
const REJECTED_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"UTF-8\"><title>Knaix</title></head><body style=\"font-family:sans-serif;text-align:center;padding:64px;color:#0f172a\"><h1>Authentication rejected</h1><p>This login callback did not match the request started by your terminal. You can close this window and run <code>knaix login</code> again.</p></body></html>";

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

    // --- Mesh Synchronization ---
    println!("\n{} Synchronizing with private mesh...", "Info:".blue());

    let client = reqwest::Client::new();
    // The request itself honours KNAIX_API_URL, even though the saved file does not.
    let api_url = load_config().api_url;
    let token = params.token.clone();

    // Trigger mesh join request in the background/async
    tokio::spawn(async move {
        match client
            .post(format!("{}/mesh/join", api_url))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
        {
            Ok(res) => {
                if let Ok(mesh) = res.json::<serde_json::Value>().await {
                    if let (Some(key), Some(host)) =
                        (mesh["authKey"].as_str(), mesh["hostname"].as_str())
                    {
                        println!("\n{}", "=== Mesh Credentials ===".bold());
                        println!("{} {}", "Join Key:".black(), key.yellow());
                        println!("{} {}", "Hostname:".black(), host.cyan());
                        println!("\nTo finalize your connection, run:");
                        println!(
                            "{}",
                            format!("  sudo tailscale up --authkey={} --hostname={}", key, host)
                                .white()
                                .on_blue()
                        );
                        println!("{} Tip: Knaix will now securely route all chat requests through this encrypted mesh.\n", "Info:".blue());
                    }
                }
            }
            Err(e) => {
                eprintln!("{} Failed to sync mesh: {}", "Error:".red(), e);
            }
        }
    });

    *state.completed.lock().unwrap() = true;

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

                <div class="brand">Kovalent · Private AI Stack · Sovereign Intelligence</div>
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

pub async fn login() {
    // Bind the callback server to an OS-assigned loopback port. A random port
    // (rather than a fixed 4242) means other local processes cannot predict the
    // callback URL to race or forge it.
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "{} Could not start the local callback server: {}",
                "Error:".red(),
                e
            );
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!(
                "{} Could not resolve the callback port: {}",
                "Error:".red(),
                e
            );
            return;
        }
    };

    // Single-use nonce the browser redirect must echo back (CSRF protection).
    let expected_state = new_state_token();

    let state = AppState {
        completed: Arc::new(Mutex::new(false)),
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
    let auth_url = match url::Url::parse("https://app.kovalentai.com/cli-auth") {
        Ok(mut u) => {
            u.query_pairs_mut().append_pair("callback", &callback_url);
            u.to_string()
        }
        Err(e) => {
            eprintln!("{} Invalid authentication URL: {}", "Error:".red(), e);
            return;
        }
    };

    println!("{} Starting Knaix SSO Login...", "Info:".blue());
    println!("  Opening browser: {}\n", auth_url.dimmed());

    if let Err(e) = open::that(&auth_url) {
        eprintln!("{} {}", "Failed to open browser:".red(), e);
    }

    // Spawn polling task
    let state_poll = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let completed = *state_poll.completed.lock().unwrap();
            if completed {
                std::process::exit(0);
            }
        }
    });

    let server = match axum::Server::from_tcp(listener) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{} Failed to start callback server: {}", "Error:".red(), e);
            return;
        }
    };
    let _ = server.serve(app.into_make_service()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
