//! Local-only browser terminal backed by the regular c-shell REPL in a PTY.

use crate::i18n::{self, Language};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{
        HeaderMap, HeaderValue, Response, StatusCode,
        header::{self, HeaderName},
    },
    response::IntoResponse,
    routing::get,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    ffi::OsString,
    io::{Read, Write},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, watch};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &[u8] = include_bytes!("../web/app.css");
const APP_JS: &[u8] = include_bytes!("../web/app.js");
const XTERM_CSS: &[u8] = include_bytes!("../web/vendor/xterm.css");
const XTERM_JS: &[u8] = include_bytes!("../web/vendor/xterm.js");
const FIT_ADDON_JS: &[u8] = include_bytes!("../web/vendor/addon-fit.js");

#[derive(Clone)]
pub struct Config {
    pub child_args: Vec<OsString>,
    pub language: Language,
    pub open_browser: bool,
    pub quiet: bool,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    expected_host: Arc<str>,
    expected_origin: Arc<str>,
    content_security_policy: Arc<str>,
    index_html: Arc<str>,
    shutdown: watch::Receiver<bool>,
}

struct PtySession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    input: mpsc::Sender<Vec<u8>>,
    output: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    fn spawn(config: &Config) -> Result<Self> {
        let executable =
            std::env::current_exe().context("cannot determine the current c-shell executable")?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("cannot create a pseudo-terminal")?;

        let mut command = CommandBuilder::new(executable);
        command.args(config.child_args.iter());
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("C_SHELL_WEB", "1");
        let child = pair
            .slave
            .spawn_command(command)
            .context("cannot start c-shell in the pseudo-terminal")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cannot open pseudo-terminal output")?;
        let mut writer = pair
            .master
            .take_writer()
            .context("cannot open pseudo-terminal input")?;
        let master = Arc::new(Mutex::new(pair.master));

        let (output_tx, output) = mpsc::channel::<Vec<u8>>(32);
        std::thread::Builder::new()
            .name("c-shell-web-pty-reader".into())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(length) => {
                            if output_tx.blocking_send(buffer[..length].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .context("cannot start pseudo-terminal reader")?;

        let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(32);
        std::thread::Builder::new()
            .name("c-shell-web-pty-writer".into())
            .spawn(move || {
                while let Some(bytes) = input_rx.blocking_recv() {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            })
            .context("cannot start pseudo-terminal writer")?;

        Ok(Self {
            master,
            input,
            output,
            child,
        })
    }

    fn resize(&self, cols: u16, rows: u16) {
        let Ok(master) = self.master.lock() else {
            return;
        };
        let _ = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn terminate(mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn serve(config: Config) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the browser terminal runtime")?
        .block_on(serve_async(config))
}

async fn serve_async(config: Config) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("cannot bind the local browser terminal")?;
    let address = listener.local_addr()?;
    let token = random_token()?;
    let root = format!("/{token}");
    let url = format!("http://{address}{root}/");
    let expected_host: Arc<str> = address.to_string().into();
    let expected_origin: Arc<str> = format!("http://{address}").into();
    let content_security_policy: Arc<str> = format!(
        // xterm.js sets element dimensions and glyph styles dynamically, so
        // inline CSS is required. Scripts remain restricted to bundled files.
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         connect-src 'self' ws://{address}; img-src 'self'; object-src 'none'; \
         base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
    )
    .into();
    let index_html: Arc<str> = render_index(config.language).into();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState {
        config: config.clone(),
        expected_host,
        expected_origin,
        content_security_policy,
        index_html,
        shutdown: shutdown_rx,
    };

    // Register the tokenized paths explicitly. Axum intentionally distinguishes
    // a nested router's empty remainder from `/`, which would otherwise make
    // the printed trailing-slash URL return 404.
    let app = Router::new()
        .route(&format!("{root}/"), get(index))
        .route(&format!("{root}/app.css"), get(app_css))
        .route(&format!("{root}/app.js"), get(app_js))
        .route(&format!("{root}/xterm.css"), get(xterm_css))
        .route(&format!("{root}/xterm.js"), get(xterm_js))
        .route(&format!("{root}/addon-fit.js"), get(fit_addon_js))
        .route(&format!("{root}/ws"), get(upgrade))
        .with_state(state);

    println!(
        "{}",
        i18n::text_with_for(config.language, "web-listening", &[("url", url.clone())])
    );
    if !config.quiet {
        println!("{}", i18n::text_for(config.language, "web-stop-hint"));
    }
    std::io::stdout().flush()?;

    if config.open_browser && webbrowser::open(&url).is_err() {
        eprintln!("{}", i18n::text_for(config.language, "web-open-failed"));
    }

    let shutdown = async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("browser terminal server failed")
}

async fn index(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/html; charset=utf-8",
        Body::from(state.index_html.to_string()),
        false,
    )
}

async fn app_css(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/css; charset=utf-8",
        Body::from(APP_CSS),
        false,
    )
}

async fn app_js(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/javascript; charset=utf-8",
        Body::from(APP_JS),
        false,
    )
}

async fn xterm_css(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/css; charset=utf-8",
        Body::from(XTERM_CSS),
        true,
    )
}

async fn xterm_js(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/javascript; charset=utf-8",
        Body::from(XTERM_JS),
        true,
    )
}

async fn fit_addon_js(State(state): State<AppState>) -> Response<Body> {
    asset_response(
        &state,
        "text/javascript; charset=utf-8",
        Body::from(FIT_ADDON_JS),
        true,
    )
}

fn asset_response(
    state: &AppState,
    content_type: &'static str,
    body: Body,
    immutable: bool,
) -> Response<Body> {
    let cache_control = if immutable {
        "private, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache_control)
        .header(
            header::CONTENT_SECURITY_POLICY,
            &*state.content_security_policy,
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        )
        .body(body)
        .expect("valid static response")
}

async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !request_is_local(&headers, &state) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let socket_state = state.clone();
    ws.max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, socket_state))
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let config = state.config.clone();
    let session = match tokio::task::spawn_blocking(move || PtySession::spawn(&config)).await {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => {
            let message = i18n::text_with_for(
                state.config.language,
                "web-session-failed",
                &[("error", error.to_string())],
            );
            let _ = socket
                .send(Message::Binary(format!("\r\n{message}\r\n").into()))
                .await;
            return;
        }
        Err(error) => {
            let message = i18n::text_with_for(
                state.config.language,
                "web-session-failed",
                &[("error", error.to_string())],
            );
            let _ = socket
                .send(Message::Binary(format!("\r\n{message}\r\n").into()))
                .await;
            return;
        }
    };

    let mut shutdown = state.shutdown.clone();
    let mut session = session;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            output = session.output.recv() => {
                let Some(output) = output else {
                    break;
                };
                if socket.send(Message::Binary(output.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        if session.input.send(bytes.to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Some((cols, rows)) = parse_resize(&text) {
                            session.resize(cols, rows);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                }
            }
        }
    }

    let _ = tokio::task::spawn_blocking(move || session.terminate()).await;
}

fn request_is_local(headers: &HeaderMap, state: &AppState) -> bool {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
    };
    header(header::HOST) == Some(&state.expected_host)
        && header(header::ORIGIN) == Some(&state.expected_origin)
}

fn parse_resize(message: &str) -> Option<(u16, u16)> {
    let mut parts = message.split(':');
    if parts.next()? != "resize" {
        return None;
    }
    let cols = parts.next()?.parse::<u16>().ok()?;
    let rows = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() || !(2..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
        return None;
    }
    Some((cols, rows))
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("cannot create access token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn render_index(language: Language) -> String {
    INDEX_HTML
        .replace("{{lang}}", language.code())
        .replace(
            "{{connecting}}",
            &html_escape(&i18n::text_for(language, "web-connecting")),
        )
        .replace(
            "{{disconnected}}",
            &html_escape(&i18n::text_for(language, "web-disconnected")),
        )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_is_128_bit_lowercase_hex() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn accepts_only_bounded_resize_messages() {
        assert_eq!(parse_resize("resize:80:24"), Some((80, 24)));
        assert_eq!(parse_resize("resize:2:1"), Some((2, 1)));
        assert_eq!(parse_resize("resize:1:24"), None);
        assert_eq!(parse_resize("resize:80:0"), None);
        assert_eq!(parse_resize("resize:80:24:1"), None);
        assert_eq!(parse_resize("hello"), None);
    }
}
