use std::{
    env,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, mpsc, Arc},
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
use display_info::DisplayInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};

static DNS_HINT_PRINTED: AtomicBool = AtomicBool::new(false);
static TUNNEL_CONNECTED: AtomicBool = AtomicBool::new(false);

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum TunnelPreference {
    Ask,
    Always,
    Never,
}

#[derive(Clone, Copy)]
enum EdgeIpPreference {
    Auto,
    V4,
    V6,
}

#[derive(Clone, Copy)]
struct AppConfig {
    mode: RunMode,
    port: u16,
    monitor_index: Option<i32>,
    list_monitors: bool,
    tunnel_preference: TunnelPreference,
    edge_ip_preference: EdgeIpPreference,
}

#[derive(Clone, Debug)]
struct MonitorSpec {
    index: usize,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale_factor: f32,
    is_primary: bool,
    name: String,
}

fn main() {
    enable_utf8_console();

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "liver=info,cloudflared=info,tower_http=info".to_string()),
        )
        .init();

    let mut config = parse_args();

    let env_port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok());
    if let Some(p) = env_port {
        config.port = p;
    }

    let monitors = get_monitors();
    let mut selected_monitor_index = config.monitor_index;
    if matches!(config.mode, RunMode::Overlay | RunMode::All) {
        if monitors.is_empty() {
            error!("no monitor found; overlay may fail to start");
        } else {
            log_monitors(&monitors);
            if selected_monitor_index.is_none() {
                selected_monitor_index = prompt_monitor_index(&monitors);
            }
        }
    } else if config.list_monitors {
        log_monitors(&monitors);
    }

    let overlay_enabled = selected_monitor_index != Some(-1);
    let tunnel_enabled = resolve_tunnel_enabled(config.mode, config.tunnel_preference);

    match config.mode {
        RunMode::Server => {
            if tunnel_enabled {
                let port = config.port;
                let edge_pref = config.edge_ip_preference;
                thread::spawn(move || run_cloudflared_blocking(port, edge_pref));
                thread::sleep(Duration::from_secs(1));
            }
            run_server_blocking(config.port)
        }
        RunMode::Overlay => {
            if tunnel_enabled {
                warn!("tunnel is ignored in --overlay mode because server is not started");
            }
            if overlay_enabled {
                run_overlay_blocking(config.port, selected_monitor_index, &monitors)
            } else {
                info!("overlay disabled by monitor index -1");
            }
        }
        RunMode::All => {
            let port = config.port;
            let monitor_index = selected_monitor_index;
            let monitors_for_overlay = monitors.clone();
            let server_thread = thread::spawn(move || run_server_blocking(port));
            // Give the server a short head start before websocket connect attempts.
            thread::sleep(Duration::from_secs(2));
            if tunnel_enabled {
                let tunnel_port = port;
                let edge_pref = config.edge_ip_preference;
                thread::spawn(move || run_cloudflared_blocking(tunnel_port, edge_pref));
            }
            if overlay_enabled {
                run_overlay_blocking(port, monitor_index, &monitors_for_overlay);
            } else {
                info!("overlay disabled by monitor index -1");
                let _ = server_thread.join();
                return;
            }
            let _ = server_thread.join();
        }
    }
}

fn parse_args() -> AppConfig {
    let mut mode = RunMode::All;
    let mut monitor_index = None;
    let mut list_monitors = false;
    let mut port = 3000u16;
    let mut tunnel_preference = TunnelPreference::Ask;
    let mut edge_ip_preference = EdgeIpPreference::Auto;
    let mut args = std::env::args().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => mode = RunMode::Server,
            "--overlay" => mode = RunMode::Overlay,
            "--all" => mode = RunMode::All,
            "--tunnel" => tunnel_preference = TunnelPreference::Always,
            "--no-tunnel" => tunnel_preference = TunnelPreference::Never,
            "--edge-ip-version" => {
                if let Some(v) = args.next() {
                    match v.as_str() {
                        "4" => edge_ip_preference = EdgeIpPreference::V4,
                        "6" => edge_ip_preference = EdgeIpPreference::V6,
                        "auto" => edge_ip_preference = EdgeIpPreference::Auto,
                        _ => error!("invalid --edge-ip-version value: {} (use 4/6/auto)", v),
                    }
                } else {
                    error!("missing value for --edge-ip-version");
                }
            }
            "--monitor" => {
                if let Some(v) = args.next() {
                    match v.parse::<i32>() {
                        Ok(idx) => monitor_index = Some(idx),
                        Err(_) => error!("invalid --monitor value: {}", v),
                    }
                } else {
                    error!("missing value for --monitor");
                }
            }
            "--list-monitors" => list_monitors = true,
            "--port" => {
                if let Some(v) = args.next() {
                    match v.parse::<u16>() {
                        Ok(p) => port = p,
                        Err(_) => error!("invalid --port value: {}", v),
                    }
                } else {
                    error!("missing value for --port");
                }
            }
            _ => {}
        }
    }

    AppConfig {
        mode,
        port,
        monitor_index,
        list_monitors,
        tunnel_preference,
        edge_ip_preference,
    }
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

fn run_cloudflared_blocking(port: u16, edge_pref: EdgeIpPreference) {
    if !command_exists("cloudflared") {
        error!("cloudflared not found. Install it first:");
        error!("  Windows: winget install --id Cloudflare.cloudflared -e");
        error!("  macOS:   brew install cloudflared");
        return;
    }
    let edge_ip_version = match choose_edge_ip_version(edge_pref) {
        Ok(v) => v,
        Err(msg) => {
            warn!("tunnel precheck failed: {}", msg);
            warn!("建议操作:");
            warn!("1) 先强制 IPv4: 加参数 --edge-ip-version 4");
            warn!("2) 验证 DNS: Resolve-DnsName region1.v2.argotunnel.com");
            warn!("3) 验证端口: Test-NetConnection region1.v2.argotunnel.com -Port 7844");
            warn!("4) 若 DNS 失败，改为可达 DNS（如 8.8.8.8）并执行 ipconfig /flushdns");
            warn!("将继续尝试启动 cloudflared，默认使用 IPv4...");
            "4"
        }
    };
    info!("cloudflared edge-ip-version={}", edge_ip_version);

    info!("starting cloudflared quick tunnel for http://127.0.0.1:{}", port);
    info!("if created, open client at: https://<random>.trycloudflare.com/client");
    let origin_cert = find_existing_origin_cert();
    if let Some(path) = origin_cert.as_ref() {
        info!("using origin cert: {}", path.display());
    } else {
        info!("no origin cert found, continue with quick tunnel (no login)");
    }

    let mut cmd = Command::new("cloudflared");
    cmd.arg("tunnel")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{}", port))
        .arg("--protocol")
        .arg("http2")
        .arg("--edge-ip-version")
        .arg(edge_ip_version)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(path) = origin_cert.as_ref() {
        cmd.arg("--origincert").arg(path.to_string_lossy().to_string());
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            error!("failed to start cloudflared: {}", err);
            return;
        }
    };

    let out = child.stdout.take();
    let err = child.stderr.take();

    let out_handle = thread::spawn(move || {
        if let Some(stdout) = out {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                log_cloudflared_line(&line);
                if let Some(url) = extract_trycloudflare_url(&line) {
                    info!(target: "cloudflared", "client URL: {}/client", url);
                }
            }
        }
    });

    let err_handle = thread::spawn(move || {
        if let Some(stderr) = err {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                log_cloudflared_line(&line);
                if let Some(url) = extract_trycloudflare_url(&line) {
                    info!(target: "cloudflared", "client URL: {}/client", url);
                }
            }
        }
    });

    let _ = child.wait();
    let _ = out_handle.join();
    let _ = err_handle.join();
}

fn find_existing_origin_cert() -> Option<PathBuf> {
    candidate_origin_cert_paths()
        .into_iter()
        .find(|p| p.is_file())
}

fn candidate_origin_cert_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let base = PathBuf::from(home);
        out.push(base.join(".cloudflared").join("cert.pem"));
        out.push(base.join(".cloudflare-warp").join("cert.pem"));
        out.push(base.join("cloudflare-warp").join("cert.pem"));
    }
    out
}

fn resolve_tunnel_enabled(mode: RunMode, preference: TunnelPreference) -> bool {
    if matches!(mode, RunMode::Overlay) {
        return false;
    }
    match preference {
        TunnelPreference::Always => true,
        TunnelPreference::Never => false,
        TunnelPreference::Ask => prompt_enable_tunnel(),
    }
}

fn prompt_enable_tunnel() -> bool {
    loop {
        print!("是否启动内网穿透 Tunnel? [Y/n]: ");
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return true;
        }
        let s = input.trim().to_ascii_lowercase();
        if s.is_empty() || s == "y" || s == "yes" {
            return true;
        }
        if s == "n" || s == "no" {
            return false;
        }
        println!("请输入 y 或 n。");
    }
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn extract_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let tail = &line[start..];
    let end = tail
        .find(char::is_whitespace)
        .unwrap_or(tail.len());
    let url = &tail[..end];
    if url.contains("trycloudflare.com") {
        Some(url.trim_end_matches('/').to_string())
    } else {
        None
    }
}

fn choose_edge_ip_version(pref: EdgeIpPreference) -> Result<&'static str, String> {
    match pref {
        EdgeIpPreference::V4 => return Ok("4"),
        EdgeIpPreference::V6 => return Ok("6"),
        EdgeIpPreference::Auto => {}
    }

    let hosts = ["region1.v2.argotunnel.com", "region2.v2.argotunnel.com"];
    let mut resolved_any = false;
    let mut reachable_v4 = false;
    let mut reachable_v6 = false;

    for host in hosts {
        let addrs = resolve_host_port(host, 7844)?;
        if addrs.is_empty() {
            continue;
        }
        resolved_any = true;

        for addr in addrs {
            if TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
                info!("tunnel precheck ok: {}", addr);
                if addr.is_ipv4() {
                    reachable_v4 = true;
                } else if addr.is_ipv6() {
                    reachable_v6 = true;
                }
            }
        }
    }

    if !resolved_any {
        return Err("cannot resolve argotunnel DNS records".to_string());
    }
    if reachable_v4 {
        return Ok("4");
    }
    if reachable_v6 {
        return Ok("6");
    }
    if !reachable_v4 && !reachable_v6 {
        return Err("cannot connect to argotunnel on TCP/7844".to_string());
    }
    Ok("4")
}

fn resolve_host_port(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let target = format!("{}:{}", host, port);
    target
        .to_socket_addrs()
        .map(|iter| iter.collect())
        .map_err(|e| format!("DNS resolve failed for {}: {}", host, e))
}

fn log_cloudflared_line(line: &str) {
    let (kind, msg) = split_cloudflared_line(line);
    if msg.contains("Registered tunnel connection") {
        TUNNEL_CONNECTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if line.contains("Cannot determine default origin certificate path") {
        info!(target: "cloudflared", "quick tunnel without login (no cert.pem)");
    // Some networks intermittently fail this resolver init while tunnel can remain usable.
    } else
    // Some networks intermittently fail this resolver init while tunnel can remain usable.
    if line.contains("Failed to initialize DNS local resolver") {
        if TUNNEL_CONNECTED.load(std::sync::atomic::Ordering::Relaxed) {
            info!(target: "cloudflared", "{} (ignored after tunnel connected)", msg);
        } else {
            warn!(target: "cloudflared", "{} (transient DNS issue; tunnel may still be connected)", msg);
            if !DNS_HINT_PRINTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!("DNS 修复建议:");
                warn!("1) 优先使用 --edge-ip-version 4");
                warn!("2) 验证 DNS: Resolve-DnsName region1.v2.argotunnel.com");
                warn!("3) 验证 7844: Test-NetConnection region1.v2.argotunnel.com -Port 7844");
                warn!("4) 若 DNS 不可达，改为可达 DNS（如 8.8.8.8）并 ipconfig /flushdns");
            }
        }
    } else if kind == "ERR" {
        error!(target: "cloudflared", "{}", msg);
    } else if kind == "WRN" {
        warn!(target: "cloudflared", "{}", msg);
    } else {
        info!(target: "cloudflared", "{}", msg);
    }
}

fn split_cloudflared_line(line: &str) -> (&'static str, &str) {
    if let Some(i) = line.find(" INF ") {
        return ("INF", line[i + 5..].trim());
    }
    if let Some(i) = line.find(" ERR ") {
        return ("ERR", line[i + 5..].trim());
    }
    if let Some(i) = line.find(" WRN ") {
        return ("WRN", line[i + 5..].trim());
    }
    if let Some(rest) = line.strip_prefix("INF ") {
        return ("INF", rest.trim());
    }
    if let Some(rest) = line.strip_prefix("ERR ") {
        return ("ERR", rest.trim());
    }
    if let Some(rest) = line.strip_prefix("WRN ") {
        return ("WRN", rest.trim());
    }
    ("INF", line.trim())
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

fn run_overlay_blocking(port: u16, monitor_index: Option<i32>, monitors: &[MonitorSpec]) {
    let ws_url = format!("ws://127.0.0.1:{}/ws", port);
    info!("starting overlay, ws={}", ws_url);

    let (tx, rx) = mpsc::channel::<DanmakuMessage>();
    thread::spawn(move || websocket_consumer_loop(ws_url, tx));

    let selected_monitor = select_monitor(monitors, monitor_index);
    if let Some(m) = selected_monitor.as_ref() {
        info!(
            "overlay monitor={} name='{}' pos=({}, {}) size={}x{} scale={}",
            m.index, m.name, m.x, m.y, m.width, m.height, m.scale_factor
        );
    } else {
        info!("overlay monitor not resolved, fallback to maximized window");
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Liver Danmaku Overlay")
        .with_decorations(false)
        .with_transparent(true)
        .with_fullscreen(false)
        .with_maximized(!cfg!(target_os = "macos"))
        .with_resizable(false)
        .with_mouse_passthrough(false);
    if !cfg!(target_os = "macos") {
        viewport = viewport.with_always_on_top();
    }

    if let Some(m) = selected_monitor {
        if cfg!(target_os = "macos") {
            // On macOS, maximized windows often stick to primary display.
            // Use monitor bounds directly to honor user selection.
            // display-info values here are already suitable for window positioning.
            let x = m.x as f32;
            let y = m.y as f32;
            let w = (m.width as f32).max(200.0);
            let h = (m.height as f32).max(120.0);
            viewport = viewport
                .with_maximized(false)
                .with_position(Pos2::new(x, y))
                .with_inner_size(Vec2::new(w, h));
            info!(
                "macOS monitor bounds: x={}, y={}, w={}, h={}",
                x, y, w, h
            );
        } else {
            // Keep the previously stable behavior for non-macOS.
            viewport = viewport
                .with_position(Pos2::new(m.x as f32, m.y as f32))
                .with_inner_size(Vec2::new(320.0, 200.0));
        }
    }

    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Liver Danmaku Overlay",
        native_options,
        Box::new(move |cc| {
            configure_overlay_fonts(&cc.egui_ctx);
            configure_macos_overlay_window(cc);
            Ok(Box::new(OverlayApp::new(rx)))
        }),
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
    top_padding: f32,
    lane_height: f32,
}

fn get_monitors() -> Vec<MonitorSpec> {
    let all = match DisplayInfo::all() {
        Ok(v) => v,
        Err(err) => {
            error!("failed to query monitors: {}", err);
            return Vec::new();
        }
    };

    all.into_iter()
        .enumerate()
        .map(|(idx, m)| MonitorSpec {
            index: idx,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            scale_factor: m.scale_factor,
            is_primary: m.is_primary,
            name: m.name,
        })
        .collect()
}

fn log_monitors(monitors: &[MonitorSpec]) {
    if monitors.is_empty() {
        info!("no monitors found");
        return;
    }
    info!("detected {} monitor(s):", monitors.len());
    for m in monitors {
        info!(
            "  [{}] {}{} pos=({}, {}) size={}x{} scale={}",
            m.index,
            m.name,
            if m.is_primary { " (primary)" } else { "" },
            m.x,
            m.y,
            m.width,
            m.height,
            m.scale_factor
        );
    }
}

fn prompt_monitor_index(monitors: &[MonitorSpec]) -> Option<i32> {
    let default_idx = monitors
        .iter()
        .find(|m| m.is_primary)
        .map(|m| m.index)
        .unwrap_or(0) as i32;

    loop {
        print!("请选择弹幕显示器编号（回车默认 {}，输入 -1 为不显示悬浮层）: ", default_idx);
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            error!("failed to read monitor input, fallback to default {}", default_idx);
            return Some(default_idx);
        }

        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Some(default_idx);
        }

        match trimmed.parse::<i32>() {
            Ok(-1) => return Some(-1),
            Ok(idx) if monitors.iter().any(|m| m.index == idx as usize) => return Some(idx),
            _ => println!("无效编号：{}，请重新输入。", trimmed),
        }
    }
}

fn select_monitor(monitors: &[MonitorSpec], monitor_index: Option<i32>) -> Option<MonitorSpec> {
    if monitors.is_empty() {
        return None;
    }

    if let Some(idx) = monitor_index {
        if idx < 0 {
            return None;
        }
        if let Some(found) = monitors.iter().find(|m| m.index == idx as usize) {
            return Some(found.clone());
        }
        error!("monitor index {} not found, fallback to primary", idx);
    }

    if let Some(primary) = monitors.iter().find(|m| m.is_primary) {
        return Some(primary.clone());
    }

    Some(monitors[0].clone())
}

fn overlay_top_padding() -> f32 {
    // macOS fullscreen presentations (especially Keynote) keep a top reserved area.
    let default = if cfg!(target_os = "macos") { 56.0 } else { 20.0 };
    std::env::var("DANMAKU_TOP_PADDING")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v >= 0.0 && *v <= 300.0)
        .unwrap_or(default)
}

impl OverlayApp {
    fn new(rx: mpsc::Receiver<DanmakuMessage>) -> Self {
        let top_padding = overlay_top_padding();
        info!("overlay top padding={}", top_padding);
        Self {
            rx,
            danmaku: Vec::new(),
            lane_busy_until: Vec::new(),
            started_at: Instant::now(),
            last_frame: Instant::now(),
            top_padding,
            lane_height: 50.0,
        }
    }

    fn rebuild_lanes(&mut self, height: f32) {
        let count = ((height - self.top_padding * 2.0) / self.lane_height).max(1.0) as usize;

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
        let y = self.top_padding + lane_index as f32 * self.lane_height;

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

    fn apply_overlay_window_flags(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Transparent(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
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

        self.apply_overlay_window_flags(ctx);

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
                    "Liver Overlay Running",
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


fn configure_overlay_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = Vec::new();

    for (idx, path) in candidate_cjk_font_paths().iter().enumerate() {
        if let Ok(bytes) = fs::read(path) {
            let key = format!("cjk_{}", idx);
            fonts
                .font_data
                .insert(key.clone(), egui::FontData::from_owned(bytes).into());
            loaded.push((key, *path));
        }
    }

    if loaded.is_empty() {
        error!("no CJK font found for overlay; Chinese may render as squares");
        return;
    }

    for (key, _) in loaded.iter().rev() {
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, key.clone());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, key.clone());
    }

    ctx.set_fonts(fonts);
    let names: Vec<&str> = loaded.iter().map(|(_, path)| *path).collect();
    info!("overlay loaded CJK font(s): {}", names.join(", "));
}

#[cfg(target_os = "macos")]
fn configure_macos_overlay_window(cc: &eframe::CreationContext<'_>) {
    use objc::{msg_send, sel, sel_impl};
    use objc::runtime::{Object, NO, YES};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGShieldingWindowLevel() -> i32;
        fn CGWindowLevelForKey(key: i32) -> i32;
    }

    let Ok(handle) = cc.window_handle() else {
        warn!("failed to fetch macOS window handle");
        return;
    };

    let ns_window = match handle.as_raw() {
        RawWindowHandle::AppKit(appkit) => {
            // raw-window-handle 0.6 exposes `ns_view`; fetch NSWindow from it.
            let ns_view = appkit.ns_view.as_ptr() as *mut Object;
            let window: *mut Object = unsafe { msg_send![ns_view, window] };
            window
        }
        _ => {
            warn!("unexpected raw window handle on macOS");
            return;
        }
    };
    if ns_window.is_null() {
        warn!("failed to resolve NSWindow from NSView");
        return;
    }

    // NSWindowCollectionBehavior flags:
    // 1 << 0  -> CanJoinAllSpaces
    // 1 << 1  -> MoveToActiveSpace
    // 1 << 3  -> Transient
    // 1 << 4  -> Stationary
    // 1 << 8  -> FullScreenAuxiliary
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;
    const TRANSIENT: usize = 1 << 3;
    const STATIONARY: usize = 1 << 4;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;

    // CoreGraphics CGWindowLevelKey values.
    const K_CG_FLOATING_WINDOW_LEVEL_KEY: i32 = 5;
    const K_CG_OVERLAY_WINDOW_LEVEL_KEY: i32 = 15;

    unsafe {
        let current: usize = msg_send![ns_window, collectionBehavior];
        let updated = current
            | CAN_JOIN_ALL_SPACES
            | MOVE_TO_ACTIVE_SPACE
            | TRANSIENT
            | STATIONARY
            | FULL_SCREEN_AUXILIARY;
        let _: () = msg_send![ns_window, setCollectionBehavior: updated];

        // Tencent-style strategy: floating/overlay level + all-spaces/fullscreen auxiliary.
        let floating = CGWindowLevelForKey(K_CG_FLOATING_WINDOW_LEVEL_KEY);
        let overlay = CGWindowLevelForKey(K_CG_OVERLAY_WINDOW_LEVEL_KEY);
        let shielding = CGShieldingWindowLevel() + 1;
        let level = floating.max(overlay).max(shielding) as isize;
        let _: () = msg_send![ns_window, setLevel: level];
        let _: () = msg_send![ns_window, setIgnoresMouseEvents: YES];
        let _: () = msg_send![ns_window, setHidesOnDeactivate: NO];
        let _: () = msg_send![ns_window, orderFrontRegardless];
    }

    info!("configured macOS overlay window with floating/overlay all-spaces strategy");
}

#[cfg(not(target_os = "macos"))]
fn configure_macos_overlay_window(_cc: &eframe::CreationContext<'_>) {}

#[cfg(target_os = "windows")]
fn candidate_cjk_font_paths() -> &'static [&'static str] {
    &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyhbd.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/simkai.ttf",
    ]
}

#[cfg(target_os = "macos")]
fn candidate_cjk_font_paths() -> &'static [&'static str] {
    &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
    ]
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn candidate_cjk_font_paths() -> &'static [&'static str] {
    &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ]
}

#[cfg(target_os = "windows")]
fn enable_utf8_console() {
    use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(target_os = "windows"))]
fn enable_utf8_console() {}

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













