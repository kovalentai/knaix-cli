use crate::config::{load_config, save_config};
use axum::{
    extract::{Extension, Query},
    response::Html,
    routing::get,
    Router,
};
use colored::*;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
pub struct CallbackParams {
    token: String,
    username: String,
}

#[derive(Clone)]
struct AppState {
    completed: Arc<Mutex<bool>>,
}

async fn handle_callback(
    Query(params): Query<CallbackParams>,
    Extension(state): Extension<AppState>,
) -> Html<&'static str> {
    let mut config = load_config();
    config.token = Some(params.token.clone());
    config.username = Some(params.username.clone());
    save_config(&config).ok();

    println!("\n{} Successfully logged in!", "✓".green());
    println!("  Welcome, {}", params.username.cyan());

    // --- Mesh Synchronization ---
    println!("\n{} Synchronizing with private mesh...", "Info:".blue());

    let client = reqwest::Client::new();
    let api_url = config.api_url.clone();
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
    let state = AppState {
        completed: Arc::new(Mutex::new(false)),
    };

    let app = Router::new()
        .route("/callback", get(handle_callback))
        .layer(Extension(state.clone()));

    let addr = SocketAddr::from(([127, 0, 0, 1], 4242));
    let auth_url = "https://app.kovalentai.com/cli-auth?callback=http://localhost:4242/callback";

    println!("{} Starting Knaix SSO Login...", "Info:".blue());
    println!("  Opening browser: {}\n", auth_url.dimmed());

    if let Err(e) = open::that(auth_url) {
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

    let _server = axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await;
}
