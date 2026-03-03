use std::{
    net::SocketAddr,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Vec2};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<DanmakuMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DanmakuMessage {
    text: String,
    color: String,
    speed: f32,
}

#[derive(Debug, Deserialize)]
struct DanmakuInput {
    text: String,
    color: Option<String>,
    speed: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ApiResponse {
    ok: bool,
    message: String,
}

#[derive(Clone, Copy)]
enum RunMode {
    Server,
    Overlay,
    All,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "liver=info,tower_http=info".to_string()),
        )
        .init();

    let mode = parse_mode();
    let port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(3000);

    match mode {
        RunMode::Server => run_server_blocking(port),
        RunMode::Overlay => run_overlay_blocking(port),
        RunMode::All => {
            let server_thread = thread::spawn(move || run_server_blocking(port));
            // Give the server a short head start before websocket connect attempts.
            thread::sleep(Duration::from_millis(500));
            run_overlay_blocking(port);
            let _ = server_thread.join();
        }
    }
}

fn parse_mode() -> RunMode {
    let mut mode = RunMode::All;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--server" => mode = RunMode::Server,
            "--overlay" => mode = RunMode::Overlay,
            "--all" => mode = RunMode::All,
            _ => {}
        }
    }
    mode
}

fn run_server_blocking(port: u16) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    runtime
        .block_on(run_server(port))
        .expect("server runtime failed");
}

async fn run_server(port: u16) -> Result<(), String> {
    let (tx, _rx) = broadcast::channel(1024);
    let state = Arc::new(AppState { tx });

    let app = Router::new()
        .route("/", get(index))
        .route("/client", get(client_page))
        .route("/screen", get(screen_page))
        .route("/api/danmaku", post(post_danmaku))
        .route("/ws", get(ws_handler))
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("server listening on http://{}", addr);
    info!("client page: http://127.0.0.1:{}/client", port);
    info!("overlay mode: cargo run -- --overlay");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("failed to bind address: {}", err))?;

    axum::serve(listener, app)
        .await
        .map_err(|err| format!("failed to serve app: {}", err))
}

fn run_overlay_blocking(port: u16) {
    let ws_url = format!("ws://127.0.0.1:{}/ws", port);
    info!("starting overlay, ws={}", ws_url);

    let (tx, rx) = mpsc::channel::<DanmakuMessage>();
    thread::spawn(move || websocket_consumer_loop(ws_url, tx));

    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Liver Danmaku Overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_fullscreen(false)
            .with_maximized(true)
            .with_resizable(false)
            .with_mouse_passthrough(false),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Liver Danmaku Overlay",
        native_options,
        Box::new(move |_cc| Ok(Box::new(OverlayApp::new(rx)))),
    );

    if let Err(err) = result {
        error!("overlay exited with error: {}", err);
    }
}

fn websocket_consumer_loop(ws_url: String, tx: mpsc::Sender<DanmakuMessage>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create websocket runtime");

    runtime.block_on(async move {
        loop {
            match connect_async(&ws_url).await {
                Ok((stream, _)) => {
                    info!("overlay websocket connected");
                    let (_, mut reader) = stream.split();

                    while let Some(next) = reader.next().await {
                        match next {
                            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                                match serde_json::from_str::<DanmakuMessage>(&text) {
                                    Ok(msg) => {
                                        if tx.send(msg).is_err() {
                                            return;
                                        }
                                    }
                                    Err(err) => error!("failed to parse danmaku: {}", err),
                                }
                            }
                            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(err) => {
                                error!("websocket read error: {}", err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    error!("websocket connect error: {}", err);
                }
            }

            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });
}

struct ActiveDanmaku {
    text: String,
    color: Color32,
    x: f32,
    y: f32,
    speed: f32,
    width: f32,
    font_size: f32,
}

struct OverlayApp {
    rx: mpsc::Receiver<DanmakuMessage>,
    danmaku: Vec<ActiveDanmaku>,
    lane_busy_until: Vec<f64>,
    started_at: Instant,
    last_frame: Instant,
}

impl OverlayApp {
    fn new(rx: mpsc::Receiver<DanmakuMessage>) -> Self {
        Self {
            rx,
            danmaku: Vec::new(),
            lane_busy_until: Vec::new(),
            started_at: Instant::now(),
            last_frame: Instant::now(),
        }
    }

    fn rebuild_lanes(&mut self, height: f32) {
        let top_padding = 20.0;
        let lane_height = 50.0;
        let count = ((height - top_padding * 2.0) / lane_height).max(1.0) as usize;

        if self.lane_busy_until.len() != count {
            self.lane_busy_until = vec![0.0; count];
        }
    }

    fn spawn_danmaku(&mut self, ctx: &egui::Context, msg: DanmakuMessage, viewport: Vec2, now_s: f64) {
        if msg.text.trim().is_empty() {
            return;
        }

        self.rebuild_lanes(viewport.y);
        if self.lane_busy_until.is_empty() {
            return;
        }

        let font_size = 26.0 + (msg.text.len() % 14) as f32;
        let font = FontId::proportional(font_size);
        let color = parse_color_or_white(&msg.color);
        let speed = msg.speed.clamp(40.0, 240.0);

        let galley = ctx.fonts(|fonts| {
            fonts.layout_no_wrap(msg.text.clone(), font.clone(), color)
        });
        let width = galley.size().x.max(30.0);

        let lane_index = choose_lane(&self.lane_busy_until, now_s);
        let lane_height = 50.0;
        let top_padding = 20.0;
        let y = top_padding + lane_index as f32 * lane_height;

        let gap_distance = width + 80.0;
        self.lane_busy_until[lane_index] = now_s + (gap_distance / speed) as f64;

        self.danmaku.push(ActiveDanmaku {
            text: msg.text,
            color,
            x: viewport.x + 24.0,
            y,
            speed,
            width,
            font_size,
        });
    }

    fn apply_click_through(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
    }
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.last_frame = now;
        let now_s = (now - self.started_at).as_secs_f64();

        let viewport = ctx.screen_rect().size();

        self.apply_click_through(ctx);

        while let Ok(msg) = self.rx.try_recv() {
            self.spawn_danmaku(ctx, msg, viewport, now_s);
        }

        for item in &mut self.danmaku {
            item.x -= item.speed * dt;
        }

        self.danmaku.retain(|item| item.x + item.width > -20.0);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let painter = ui.painter();
                for item in &self.danmaku {
                    let shadow_pos = Pos2::new(item.x + 2.0, item.y + 2.0);
                    painter.text(
                        shadow_pos,
                        Align2::LEFT_TOP,
                        &item.text,
                        FontId::proportional(item.font_size),
                        Color32::from_black_alpha(160),
                    );
                    painter.text(
                        Pos2::new(item.x, item.y),
                        Align2::LEFT_TOP,
                        &item.text,
                        FontId::proportional(item.font_size),
                        item.color,
                    );
                }

                // Keep a tiny always-on label so we can verify the overlay is visible.
                painter.text(
                    Pos2::new(14.0, 10.0),
                    Align2::LEFT_TOP,
                    "Liver Overlay Running  |  鼠标穿透: 开",
                    FontId::proportional(16.0),
                    Color32::from_rgba_unmultiplied(200, 255, 200, 220),
                );
            });

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn choose_lane(lanes: &[f64], now_s: f64) -> usize {
    let mut best_idx = 0;
    let mut best_busy = f64::MAX;
    for (idx, &busy_until) in lanes.iter().enumerate() {
        if busy_until <= now_s {
            return idx;
        }
        if busy_until < best_busy {
            best_busy = busy_until;
            best_idx = idx;
        }
    }
    best_idx
}

fn parse_color_or_white(input: &str) -> Color32 {
    if input.len() == 7
        && input.starts_with('#')
        && input.chars().skip(1).all(|c| c.is_ascii_hexdigit())
    {
        let r = u8::from_str_radix(&input[1..3], 16).unwrap_or(255);
        let g = u8::from_str_radix(&input[3..5], 16).unwrap_or(255);
        let b = u8::from_str_radix(&input[5..7], 16).unwrap_or(255);
        Color32::from_rgb(r, g, b)
    } else {
        Color32::WHITE
    }
}

async fn index() -> Html<&'static str> {
    Html(
        r#"
<!doctype html>
<html lang="zh-CN">
  <head><meta charset="utf-8" /><title>Liver Danmaku</title></head>
  <body style="font-family: sans-serif; padding: 20px;">
    <h2>Liver Danmaku Server</h2>
    <ul>
      <li><a href="/client">/client</a> 提交弹幕</li>
      <li><a href="/screen">/screen</a> 浏览器版弹幕屏幕（可选）</li>
    </ul>
    <p>桌面悬浮弹幕: 运行 <code>cargo run</code>（默认 server + overlay）</p>
    <p>只运行服务端: <code>cargo run -- --server</code></p>
    <p>只运行悬浮层: <code>cargo run -- --overlay</code></p>
  </body>
</html>
"#,
    )
}

async fn client_page() -> Html<String> {
    match tokio::fs::read_to_string("static/client.html").await {
        Ok(content) => Html(content),
        Err(err) => Html(format!("failed to load client page: {}", err)),
    }
}

async fn screen_page() -> Html<String> {
    match tokio::fs::read_to_string("static/screen.html").await {
        Ok(content) => Html(content),
        Err(err) => Html(format!("failed to load screen page: {}", err)),
    }
}

async fn post_danmaku(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<DanmakuInput>,
) -> Result<Json<ApiResponse>, (StatusCode, Json<ApiResponse>)> {
    let text = payload.text.trim();

    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                ok: false,
                message: "text cannot be empty".to_string(),
            }),
        ));
    }

    if text.chars().count() > 120 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                ok: false,
                message: "text too long (max 120 chars)".to_string(),
            }),
        ));
    }

    let color = normalize_color(payload.color.unwrap_or_else(|| "#ffffff".to_string()));
    let speed = payload.speed.unwrap_or(90.0).clamp(40.0, 240.0);

    let message = DanmakuMessage {
        text: text.to_string(),
        color,
        speed,
    };

    if state.tx.send(message).is_err() {
        error!("no websocket clients connected");
    }

    Ok(Json(ApiResponse {
        ok: true,
        message: "sent".to_string(),
    }))
}

fn normalize_color(input: String) -> String {
    let s = input.trim();
    if s.len() == 7
        && s.starts_with('#')
        && s.chars().skip(1).all(|c| c.is_ascii_hexdigit())
    {
        s.to_string()
    } else {
        "#ffffff".to_string()
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    info!("websocket connected");

    loop {
        tokio::select! {
            recv_result = rx.recv() => {
                match recv_result {
                    Ok(message) => {
                        match serde_json::to_string(&message) {
                            Ok(serialized) => {
                                if socket.send(Message::Text(serialized.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                error!("failed to serialize message: {}", err);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        error!("websocket lagged, skipped {} messages", skipped);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    info!("websocket disconnected");
}
