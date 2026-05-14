#![allow(unexpected_cfgs)]

use std::{
    env,
    ffi::{c_void, CString},
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicPtr, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
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
use display_info::DisplayInfo;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Vec2};
use futures_util::StreamExt;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};

#[cfg(target_os = "macos")]
use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
#[cfg(target_os = "macos")]
use core_graphics::access::ScreenCaptureAccess;
#[cfg(target_os = "macos")]
use core_graphics::geometry::{CGPoint, CGSize};
#[cfg(target_os = "macos")]
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowIsOnscreen,
    kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID, CGWindowID,
};
#[cfg(target_os = "macos")]
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl, Encode, Encoding,
};

static DNS_HINT_PRINTED: AtomicBool = AtomicBool::new(false);
static TUNNEL_CONNECTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static AX_PERMISSION_HINT_PRINTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static AX_PERMISSION_PROMPTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static SCREEN_CAPTURE_HINT_PRINTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static SCREEN_CAPTURE_PROMPTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static MACOS_NS_WINDOW_HACKS_HINT_PRINTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static MACOS_OVERLAY_WINDOW: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
#[cfg(target_os = "macos")]
static MACOS_NATIVE_HELPER_STATE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
const QUICK_TUNNEL_MAX_ATTEMPTS: usize = 3;
const QUICK_TUNNEL_RETRY_DELAYS_SECS: [u64; QUICK_TUNNEL_MAX_ATTEMPTS - 1] = [2, 5];

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
enum OverlayAttachMode {
    Monitor,
    FullscreenWindow,
    FrontmostWindow,
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
    overlay_attach_mode: OverlayAttachMode,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunnelLaunchErrorKind {
    CloudflareService,
    LocalProcess,
}

#[derive(Debug, Clone)]
struct TunnelLaunchError {
    kind: TunnelLaunchErrorKind,
    detail: String,
}

impl TunnelLaunchError {
    fn cloudflare_service(detail: impl Into<String>) -> Self {
        Self {
            kind: TunnelLaunchErrorKind::CloudflareService,
            detail: detail.into(),
        }
    }

    fn local(detail: impl Into<String>) -> Self {
        Self {
            kind: TunnelLaunchErrorKind::LocalProcess,
            detail: detail.into(),
        }
    }

    fn is_cloudflare_service(&self) -> bool {
        matches!(self.kind, TunnelLaunchErrorKind::CloudflareService)
    }
}

impl std::fmt::Display for TunnelLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Debug, Default, Clone)]
struct CloudflaredAttemptState {
    connected: bool,
    cloudflare_service_error: Option<String>,
    last_error: Option<String>,
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
            if selected_monitor_index.is_none() && should_prompt_monitor(config.overlay_attach_mode)
            {
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
                run_overlay_blocking(
                    config.port,
                    selected_monitor_index,
                    &monitors,
                    config.overlay_attach_mode,
                )
            } else {
                info!("overlay disabled by monitor index -1");
            }
        }
        RunMode::All => {
            let port = config.port;
            let monitor_index = selected_monitor_index;
            let monitors_for_overlay = monitors.clone();
            let overlay_attach_mode = config.overlay_attach_mode;
            let server_thread = thread::spawn(move || run_server_blocking(port));
            // Give the server a short head start before websocket connect attempts.
            thread::sleep(Duration::from_secs(2));
            if tunnel_enabled {
                let tunnel_port = port;
                let edge_pref = config.edge_ip_preference;
                thread::spawn(move || run_cloudflared_blocking(tunnel_port, edge_pref));
            }
            if overlay_enabled {
                run_overlay_blocking(
                    port,
                    monitor_index,
                    &monitors_for_overlay,
                    overlay_attach_mode,
                );
            } else {
                info!("overlay disabled by monitor index -1");
                let _ = server_thread.join();
                return;
            }
            let _ = server_thread.join();
        }
    }
}

fn default_overlay_attach_mode() -> OverlayAttachMode {
    if cfg!(target_os = "macos") {
        OverlayAttachMode::FullscreenWindow
    } else {
        OverlayAttachMode::Monitor
    }
}

fn should_prompt_monitor(attach_mode: OverlayAttachMode) -> bool {
    !(cfg!(target_os = "macos") && matches!(attach_mode, OverlayAttachMode::FrontmostWindow))
}

fn parse_args() -> AppConfig {
    let mut mode = RunMode::All;
    let mut monitor_index = None;
    let mut overlay_attach_mode = default_overlay_attach_mode();
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
            "--follow-fullscreen" => overlay_attach_mode = OverlayAttachMode::FullscreenWindow,
            "--follow-window" => overlay_attach_mode = OverlayAttachMode::FrontmostWindow,
            "--follow-monitor" => overlay_attach_mode = OverlayAttachMode::Monitor,
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
        overlay_attach_mode,
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

    info!("if created, open client at: https://<random>.trycloudflare.com/client");
    let origin_cert = find_existing_origin_cert();
    if let Some(path) = origin_cert.as_ref() {
        info!("using origin cert: {}", path.display());
    } else {
        info!("no origin cert found, continue with quick tunnel (no login)");
    }

    for attempt in 1..=QUICK_TUNNEL_MAX_ATTEMPTS {
        TUNNEL_CONNECTED.store(false, Ordering::Relaxed);
        info!(
            "starting cloudflared quick tunnel for http://127.0.0.1:{} (attempt {}/{})",
            port, attempt, QUICK_TUNNEL_MAX_ATTEMPTS
        );

        match run_cloudflared_attempt(port, edge_ip_version, origin_cert.clone()) {
            Ok(()) => return,
            Err(err) if err.is_cloudflare_service() && attempt < QUICK_TUNNEL_MAX_ATTEMPTS => {
                let delay = quick_tunnel_retry_delay_secs(attempt);
                warn!(
                    "cloudflared quick tunnel attempt {}/{} failed because TryCloudflare returned a service-side error: {}",
                    attempt, QUICK_TUNNEL_MAX_ATTEMPTS, err
                );
                warn!(
                    "这看起来是 Cloudflare TryCloudflare 服务端异常，不是 liver 代码问题；{} 秒后自动重试...",
                    delay
                );
                thread::sleep(Duration::from_secs(delay));
            }
            Err(err) if err.is_cloudflare_service() => {
                panic!("{}", format_cloudflare_quick_tunnel_panic(&err));
            }
            Err(err) => {
                error!("{}", err);
                return;
            }
        }
    }
}

fn run_cloudflared_attempt(
    port: u16,
    edge_ip_version: &str,
    origin_cert: Option<PathBuf>,
) -> Result<(), TunnelLaunchError> {
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
        cmd.arg("--origincert")
            .arg(path.to_string_lossy().to_string());
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return Err(TunnelLaunchError::local(format!(
                "failed to start cloudflared: {}",
                err
            )));
        }
    };

    let out = child.stdout.take();
    let err = child.stderr.take();
    let attempt_state = Arc::new(Mutex::new(CloudflaredAttemptState::default()));
    let out_state = Arc::clone(&attempt_state);
    let err_state = Arc::clone(&attempt_state);

    let out_handle = thread::spawn(move || {
        if let Some(stdout) = out {
            pump_cloudflared_output(BufReader::new(stdout), out_state);
        }
    });

    let err_handle = thread::spawn(move || {
        if let Some(stderr) = err {
            pump_cloudflared_output(BufReader::new(stderr), err_state);
        }
    });

    let status = child.wait().map_err(|err| {
        TunnelLaunchError::local(format!("failed while waiting for cloudflared: {}", err))
    })?;
    let _ = out_handle.join();
    let _ = err_handle.join();

    let state = lock_cloudflared_attempt_state(&attempt_state).clone();

    if state.connected {
        if !status.success() {
            warn!(
                target: "cloudflared",
                "cloudflared exited after tunnel had connected: {}",
                describe_exit_status(status)
            );
        }
        return Ok(());
    }

    if let Some(detail) = state.cloudflare_service_error {
        return Err(TunnelLaunchError::cloudflare_service(detail));
    }

    if let Some(detail) = state.last_error {
        return Err(TunnelLaunchError::local(format!(
            "cloudflared exited before tunnel connected: {}",
            detail
        )));
    }

    Err(TunnelLaunchError::local(format!(
        "cloudflared exited before tunnel connected with {}",
        describe_exit_status(status)
    )))
}

fn pump_cloudflared_output<R: BufRead>(
    reader: R,
    attempt_state: Arc<Mutex<CloudflaredAttemptState>>,
) {
    for line in reader.lines().map_while(Result::ok) {
        log_cloudflared_line(&line);
        if let Some(url) = extract_trycloudflare_url(&line) {
            info!(target: "cloudflared", "client URL: {}/client", url);
        }
        record_cloudflared_attempt_line(&attempt_state, &line);
    }
}

fn record_cloudflared_attempt_line(
    attempt_state: &Arc<Mutex<CloudflaredAttemptState>>,
    line: &str,
) {
    let (_, msg) = split_cloudflared_line(line);
    let mut state = lock_cloudflared_attempt_state(attempt_state);

    if msg.contains("Registered tunnel connection") {
        state.connected = true;
    }

    if let Some(detail) = classify_cloudflare_quick_tunnel_error(line, msg) {
        state.cloudflare_service_error.get_or_insert(detail);
    }

    if line.contains(" ERR ") || line.starts_with("ERR ") {
        state.last_error = Some(msg.to_string());
    }
}

fn classify_cloudflare_quick_tunnel_error(line: &str, msg: &str) -> Option<String> {
    if line.contains("Error unmarshaling QuickTunnel response")
        || line.contains("failed to unmarshal quick Tunnel")
    {
        return Some(format!(
            "TryCloudflare returned an invalid Quick Tunnel response: {}",
            msg
        ));
    }

    None
}

fn quick_tunnel_retry_delay_secs(attempt: usize) -> u64 {
    QUICK_TUNNEL_RETRY_DELAYS_SECS
        .get(attempt.saturating_sub(1))
        .copied()
        .unwrap_or(5)
}

fn format_cloudflare_quick_tunnel_panic(err: &TunnelLaunchError) -> String {
    format!(
        "Cloudflare Quick Tunnel 连续 {} 次启动失败：{}。这是 Cloudflare TryCloudflare 服务端问题，不是 liver 代码问题。请稍后重试，或改用登录后的 named tunnel。",
        QUICK_TUNNEL_MAX_ATTEMPTS, err
    )
}

fn describe_exit_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {}", code),
        None => "signal termination".to_string(),
    }
}

fn lock_cloudflared_attempt_state(
    attempt_state: &Arc<Mutex<CloudflaredAttemptState>>,
) -> std::sync::MutexGuard<'_, CloudflaredAttemptState> {
    attempt_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
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
        TUNNEL_CONNECTED.store(true, Ordering::Relaxed);
    }
    if line.contains("Cannot determine default origin certificate path") {
        info!(target: "cloudflared", "quick tunnel without login (no cert.pem)");
    // Some networks intermittently fail this resolver init while tunnel can remain usable.
    } else
    // Some networks intermittently fail this resolver init while tunnel can remain usable.
    if line.contains("Failed to initialize DNS local resolver") {
        if TUNNEL_CONNECTED.load(Ordering::Relaxed) {
            info!(target: "cloudflared", "{} (ignored after tunnel connected)", msg);
        } else {
            warn!(target: "cloudflared", "{} (transient DNS issue; tunnel may still be connected)", msg);
            if !DNS_HINT_PRINTED.swap(true, Ordering::Relaxed) {
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

fn run_overlay_blocking(
    port: u16,
    monitor_index: Option<i32>,
    monitors: &[MonitorSpec],
    attach_mode: OverlayAttachMode,
) {
    let ws_url = format!("ws://127.0.0.1:{}/ws", port);
    info!(
        "starting overlay, ws={}, attach_mode={}",
        ws_url,
        overlay_attach_mode_label(attach_mode)
    );

    if !cfg!(target_os = "macos") && matches!(attach_mode, OverlayAttachMode::FrontmostWindow) {
        warn!("--follow-window is currently only implemented on macOS; fallback to monitor mode");
    }

    let selected_monitor = select_monitor(monitors, monitor_index);
    if let Some(m) = selected_monitor.as_ref() {
        info!(
            "overlay monitor={} name='{}' pos=({}, {}) size={}x{} scale={}",
            m.index, m.name, m.x, m.y, m.width, m.height, m.scale_factor
        );
    } else {
        info!("overlay monitor not resolved, fallback to maximized window");
    }

    #[cfg(target_os = "macos")]
    if overlay_attach_mode_uses_window_tracking(attach_mode)
        && macos_native_helper_overlay_enabled()
    {
        match run_macos_native_helper_overlay_blocking(
            port,
            monitors,
            selected_monitor.as_ref(),
            attach_mode,
        ) {
            Ok(()) => return,
            Err(err) => {
                warn!("native helper overlay failed, fallback to eframe: {}", err);
            }
        }
    }

    let (tx, rx) = mpsc::channel::<DanmakuMessage>();
    thread::spawn(move || websocket_consumer_loop(ws_url, tx));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Liver Danmaku Overlay")
        .with_decorations(false)
        .with_transparent(true)
        .with_fullscreen(false)
        .with_maximized(!cfg!(target_os = "macos"))
        .with_resizable(false)
        .with_mouse_passthrough(true)
        .with_window_level(egui::WindowLevel::AlwaysOnTop);

    if let Some(m) = selected_monitor.as_ref() {
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
            info!("macOS monitor bounds: x={}, y={}, w={}, h={}", x, y, w, h);
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
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                configure_overlay_fonts(&cc.egui_ctx);
            }))
            .is_err()
            {
                warn!("overlay font init panicked; fallback to default egui fonts");
            }
            #[cfg(target_os = "macos")]
            if macos_ns_window_hacks_enabled() {
                configure_macos_overlay_window(cc);
            } else {
                macos_log_ns_window_hacks_hint_once();
            }
            #[cfg(not(target_os = "macos"))]
            configure_macos_overlay_window(cc);
            Ok(Box::new(OverlayApp::new(
                rx,
                attach_mode,
                selected_monitor.clone(),
                monitors.to_vec(),
            )))
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

#[cfg(target_os = "macos")]
#[cfg(target_pointer_width = "64")]
type CGFloat = f64;
#[cfg(target_os = "macos")]
#[cfg(target_pointer_width = "32")]
type CGFloat = f32;

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: CGFloat,
    y: CGFloat,
}

#[cfg(target_os = "macos")]
unsafe impl Encode for NSPoint {
    fn encode() -> Encoding {
        #[cfg(target_pointer_width = "64")]
        unsafe {
            Encoding::from_str("{CGPoint=dd}")
        }
        #[cfg(target_pointer_width = "32")]
        unsafe {
            Encoding::from_str("{CGPoint=ff}")
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: CGFloat,
    height: CGFloat,
}

#[cfg(target_os = "macos")]
unsafe impl Encode for NSSize {
    fn encode() -> Encoding {
        #[cfg(target_pointer_width = "64")]
        unsafe {
            Encoding::from_str("{CGSize=dd}")
        }
        #[cfg(target_pointer_width = "32")]
        unsafe {
            Encoding::from_str("{CGSize=ff}")
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

#[cfg(target_os = "macos")]
unsafe impl Encode for NSRect {
    fn encode() -> Encoding {
        #[cfg(target_pointer_width = "64")]
        unsafe {
            Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}")
        }
        #[cfg(target_pointer_width = "32")]
        unsafe {
            Encoding::from_str("{CGRect={CGPoint=ff}{CGSize=ff}}")
        }
    }
}

#[cfg(target_os = "macos")]
struct MacNativeOverlayState {
    panel: *mut Object,
    attach_mode: OverlayAttachMode,
    window_kind: MacHelperWindowKind,
    preferred_monitor_index: Option<usize>,
    monitors: Vec<MonitorSpec>,
    tracker: MacWindowTracker,
    tracked_window: Option<MacTrackedWindow>,
    fallback_bounds: MacWindowBounds,
    desktop_bottom: f32,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacHelperWindowKind {
    Window,
    Panel,
}

#[cfg(target_os = "macos")]
fn macos_native_helper_overlay_enabled() -> bool {
    !matches!(
        std::env::var("DANMAKU_MACOS_OVERLAY_BACKEND")
            .ok()
            .as_deref(),
        Some("eframe" | "EFRAME")
    )
}

#[cfg(target_os = "macos")]
fn macos_helper_window_kind() -> MacHelperWindowKind {
    match std::env::var("DANMAKU_MACOS_HELPER_WINDOW_KIND")
        .ok()
        .as_deref()
    {
        Some("panel" | "PANEL") => MacHelperWindowKind::Panel,
        _ => MacHelperWindowKind::Window,
    }
}

#[cfg(target_os = "macos")]
fn macos_helper_window_kind_label(kind: MacHelperWindowKind) -> &'static str {
    match kind {
        MacHelperWindowKind::Window => "window",
        MacHelperWindowKind::Panel => "panel",
    }
}

#[cfg(target_os = "macos")]
fn macos_helper_collection_behavior(window_kind: MacHelperWindowKind) -> usize {
    const NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;
    const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE: usize = 1 << 6;
    const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_APPLICATIONS: usize = 1 << 18;

    match window_kind {
        MacHelperWindowKind::Window => {
            NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE
                | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE
                | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
                | NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_APPLICATIONS
        }
        MacHelperWindowKind::Panel => {
            NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
                | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE
                | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
                | NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_APPLICATIONS
        }
    }
}

#[cfg(target_os = "macos")]
fn run_macos_native_helper_overlay_blocking(
    port: u16,
    monitors: &[MonitorSpec],
    selected_monitor: Option<&MonitorSpec>,
    attach_mode: OverlayAttachMode,
) -> Result<(), String> {
    let app: *mut Object = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    if app.is_null() {
        return Err("failed to access NSApplication".to_string());
    }

    let _: () = unsafe { msg_send![app, setActivationPolicy: 1isize] };

    let mut tracker = MacWindowTracker::new();
    let window_kind = macos_helper_window_kind();
    let helper_level = macos_overlay_window_level();
    let helper_behavior = macos_helper_collection_behavior(window_kind);
    let preferred_monitor_index = selected_monitor.map(|monitor| monitor.index);
    let tracked_window =
        macos_poll_overlay_target(&mut tracker, attach_mode, monitors, preferred_monitor_index);
    let fallback_bounds = macos_native_helper_fallback_bounds(selected_monitor);
    let desktop_bottom = macos_desktop_bottom_edge(monitors);
    let initial_bounds = tracked_window
        .as_ref()
        .map(|window| window.bounds)
        .unwrap_or(fallback_bounds);
    let initial_appkit_bounds = macos_top_left_to_appkit_bounds(initial_bounds, desktop_bottom);

    info!(
        "starting macOS native helper overlay, kind={}, level={}, behavior=0x{:x}, url={}, initial_bounds=({}, {}) {}x{}",
        macos_helper_window_kind_label(window_kind),
        helper_level,
        helper_behavior,
        macos_native_helper_overlay_url(port),
        initial_bounds.x.round() as i32,
        initial_bounds.y.round() as i32,
        initial_bounds.width.round() as i32,
        initial_bounds.height.round() as i32
    );

    info!(
        "macOS helper initial appkit frame=({}, {}) {}x{}",
        initial_appkit_bounds.x.round() as i32,
        initial_appkit_bounds.y.round() as i32,
        initial_appkit_bounds.width.round() as i32,
        initial_appkit_bounds.height.round() as i32
    );

    let panel = macos_create_native_helper_panel(window_kind, initial_appkit_bounds)?;
    macos_attach_native_helper_webview(panel, &macos_native_helper_overlay_url(port))?;

    let mut state = Box::new(MacNativeOverlayState {
        panel,
        attach_mode,
        window_kind,
        preferred_monitor_index,
        monitors: monitors.to_vec(),
        tracker,
        tracked_window,
        fallback_bounds,
        desktop_bottom,
    });
    macos_sync_native_helper_state(&mut state);

    let timer_target: *mut Object =
        unsafe { msg_send![macos_native_helper_timer_target_class(), new] };
    if timer_target.is_null() {
        return Err("failed to create helper timer target".to_string());
    }

    let timer: *mut Object = unsafe {
        msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 0.15f64
            target: timer_target
            selector: sel!(tick:)
            userInfo: ptr::null_mut::<Object>()
            repeats: true
        ]
    };
    if timer.is_null() {
        return Err("failed to schedule helper overlay timer".to_string());
    }
    MACOS_NATIVE_HELPER_STATE.store(Box::into_raw(state).cast(), Ordering::Relaxed);
    let _: () = unsafe { msg_send![timer, fire] };
    let _: () = unsafe { msg_send![panel, orderFrontRegardless] };

    info!(
        "macOS helper overlay backend is active (set DANMAKU_MACOS_OVERLAY_BACKEND=eframe to revert)"
    );
    unsafe {
        let _: () = msg_send![app, run];
    }
    MACOS_NATIVE_HELPER_STATE.store(ptr::null_mut(), Ordering::Relaxed);
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_native_helper_overlay_url(port: u16) -> String {
    format!(
        "http://127.0.0.1:{}/screen?overlay=helper&transparent=1",
        port
    )
}

#[cfg(target_os = "macos")]
fn macos_native_helper_fallback_bounds(selected_monitor: Option<&MonitorSpec>) -> MacWindowBounds {
    if let Some(monitor) = selected_monitor {
        return MacWindowBounds {
            x: monitor.x as f32,
            y: monitor.y as f32,
            width: monitor.width as f32,
            height: monitor.height as f32,
        };
    }

    MacWindowBounds {
        x: 80.0,
        y: 80.0,
        width: 1280.0,
        height: 720.0,
    }
}

#[cfg(target_os = "macos")]
fn macos_desktop_bottom_edge(monitors: &[MonitorSpec]) -> f32 {
    monitors
        .iter()
        .map(|monitor| monitor.y as f32 + monitor.height as f32)
        .fold(0.0, f32::max)
}

#[cfg(target_os = "macos")]
fn macos_monitor_bounds(monitor: &MonitorSpec) -> MacWindowBounds {
    MacWindowBounds {
        x: monitor.x as f32,
        y: monitor.y as f32,
        width: monitor.width as f32,
        height: monitor.height as f32,
    }
}

#[cfg(target_os = "macos")]
fn macos_rect_intersection_area(a: MacWindowBounds, b: MacWindowBounds) -> f32 {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    let width = (right - left).max(0.0);
    let height = (bottom - top).max(0.0);
    width * height
}

#[cfg(target_os = "macos")]
fn macos_snap_fullscreen_target_to_monitor(
    mut target: MacTrackedWindow,
    monitors: &[MonitorSpec],
    preferred_monitor_index: Option<usize>,
) -> Option<MacTrackedWindow> {
    let window_area = (target.bounds.width * target.bounds.height).max(1.0);
    let mut best_monitor: Option<&MonitorSpec> = None;
    let mut best_overlap = 0.0f32;

    for monitor in monitors {
        let monitor_bounds = macos_monitor_bounds(monitor);
        let overlap = macos_rect_intersection_area(target.bounds, monitor_bounds);
        if overlap > best_overlap {
            best_overlap = overlap;
            best_monitor = Some(monitor);
        }
    }

    let monitor = best_monitor?;
    // In follow-fullscreen mode, a selected monitor acts as an affinity guard:
    // full-screen windows on other displays must not steal the overlay away.
    if preferred_monitor_index
        .map(|expected| expected != monitor.index)
        .unwrap_or(false)
    {
        return None;
    }
    let monitor_bounds = macos_monitor_bounds(monitor);
    let monitor_area = (monitor_bounds.width * monitor_bounds.height).max(1.0);
    let monitor_coverage = best_overlap / monitor_area;
    let window_coverage = best_overlap / window_area;
    let width_ratio = target.bounds.width / monitor_bounds.width.max(1.0);
    let height_ratio = target.bounds.height / monitor_bounds.height.max(1.0);
    let left_delta = (target.bounds.x - monitor_bounds.x).abs();
    let top_delta = (target.bounds.y - monitor_bounds.y).abs();

    if monitor_coverage < 0.88
        || window_coverage < 0.75
        || width_ratio < 0.88
        || height_ratio < 0.84
        || left_delta > 96.0
        || top_delta > 96.0
    {
        return None;
    }

    target.bounds = monitor_bounds;
    Some(target)
}

#[cfg(target_os = "macos")]
fn macos_poll_overlay_target(
    tracker: &mut MacWindowTracker,
    attach_mode: OverlayAttachMode,
    monitors: &[MonitorSpec],
    preferred_monitor_index: Option<usize>,
) -> Option<MacTrackedWindow> {
    match attach_mode {
        OverlayAttachMode::Monitor => None,
        OverlayAttachMode::FrontmostWindow => tracker.poll_target(),
        OverlayAttachMode::FullscreenWindow => {
            tracker.poll_frontmost_candidate().and_then(|target| {
                macos_snap_fullscreen_target_to_monitor(target, monitors, preferred_monitor_index)
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_create_native_helper_panel(
    window_kind: MacHelperWindowKind,
    initial_bounds: MacWindowBounds,
) -> Result<*mut Object, String> {
    const NS_WINDOW_STYLE_MASK_BORDERLESS: usize = 0;
    const NS_WINDOW_STYLE_MASK_NONACTIVATING_PANEL: usize = 1 << 7;
    const NS_BACKING_STORE_BUFFERED: usize = 2;

    let window_class = match window_kind {
        MacHelperWindowKind::Window => class!(NSWindow),
        MacHelperWindowKind::Panel => class!(NSPanel),
    };
    let style_mask = match window_kind {
        MacHelperWindowKind::Window => NS_WINDOW_STYLE_MASK_BORDERLESS,
        MacHelperWindowKind::Panel => {
            NS_WINDOW_STYLE_MASK_BORDERLESS | NS_WINDOW_STYLE_MASK_NONACTIVATING_PANEL
        }
    };

    let panel_alloc: *mut Object = unsafe { msg_send![window_class, alloc] };
    if panel_alloc.is_null() {
        return Err(format!(
            "failed to allocate macOS helper {}",
            macos_helper_window_kind_label(window_kind)
        ));
    }

    let panel: *mut Object = unsafe {
        msg_send![
            panel_alloc,
            initWithContentRect: ns_rect(0.0, 0.0, initial_bounds.width.max(160.0), initial_bounds.height.max(90.0))
            styleMask: style_mask
            backing: NS_BACKING_STORE_BUFFERED
            defer: false
        ]
    };
    if panel.is_null() {
        return Err(format!(
            "failed to initialize macOS helper {}",
            macos_helper_window_kind_label(window_kind)
        ));
    }

    macos_apply_native_helper_panel_style(panel, window_kind);
    macos_apply_native_helper_panel_bounds(panel, initial_bounds);
    Ok(panel)
}

#[cfg(target_os = "macos")]
fn macos_attach_native_helper_webview(panel: *mut Object, url: &str) -> Result<(), String> {
    let webview_class =
        Class::get("WKWebView").ok_or_else(|| "WKWebView class not available".to_string())?;
    let config_class = Class::get("WKWebViewConfiguration")
        .ok_or_else(|| "WKWebViewConfiguration class not available".to_string())?;

    let config_alloc: *mut Object = unsafe { msg_send![config_class, alloc] };
    if config_alloc.is_null() {
        return Err("failed to allocate WKWebViewConfiguration".to_string());
    }
    let config: *mut Object = unsafe { msg_send![config_alloc, init] };
    if config.is_null() {
        return Err("failed to initialize WKWebViewConfiguration".to_string());
    }

    let web_view_alloc: *mut Object = unsafe { msg_send![webview_class, alloc] };
    if web_view_alloc.is_null() {
        return Err("failed to allocate WKWebView".to_string());
    }
    let web_view: *mut Object = unsafe {
        msg_send![
            web_view_alloc,
            initWithFrame: ns_rect(0.0, 0.0, 1200.0, 700.0)
            configuration: config
        ]
    };
    if web_view.is_null() {
        return Err("failed to initialize WKWebView".to_string());
    }

    let clear_color: *mut Object = unsafe { msg_send![class!(NSColor), clearColor] };
    let false_number: *mut Object = unsafe { msg_send![class!(NSNumber), numberWithBool: false] };
    let draws_background_key = nsstring("drawsBackground");
    let supports_under_page_background: bool =
        unsafe { msg_send![web_view, respondsToSelector: sel!(setUnderPageBackgroundColor:)] };

    unsafe {
        let _: () = msg_send![web_view, setOpaque: false];
        let _: () = msg_send![web_view, setBackgroundColor: clear_color];
        let _: () = msg_send![web_view, setValue: false_number forKey: draws_background_key];
        if supports_under_page_background {
            let _: () = msg_send![web_view, setUnderPageBackgroundColor: clear_color];
        }
        let _: () = msg_send![panel, setContentView: web_view];
    }

    let ns_url_string = nsstring(url);
    let ns_url: *mut Object = unsafe { msg_send![class!(NSURL), URLWithString: ns_url_string] };
    if ns_url.is_null() {
        return Err(format!("failed to create NSURL from {}", url));
    }
    let request: *mut Object = unsafe { msg_send![class!(NSURLRequest), requestWithURL: ns_url] };
    if request.is_null() {
        return Err("failed to create NSURLRequest".to_string());
    }

    unsafe {
        let _: *mut Object = msg_send![web_view, loadRequest: request];
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_apply_native_helper_panel_style(panel: *mut Object, window_kind: MacHelperWindowKind) {
    if panel.is_null() {
        return;
    }

    let clear_color: *mut Object = unsafe { msg_send![class!(NSColor), clearColor] };
    let behavior = macos_helper_collection_behavior(window_kind);

    unsafe {
        let _: () = msg_send![panel, setCollectionBehavior: behavior];
        let _: () = msg_send![panel, setLevel: macos_overlay_window_level()];
        let _: () = msg_send![panel, setOpaque: false];
        let _: () = msg_send![panel, setHasShadow: false];
        let _: () = msg_send![panel, setHidesOnDeactivate: false];
        let _: () = msg_send![panel, setIgnoresMouseEvents: true];
        let _: () = msg_send![panel, setReleasedWhenClosed: false];
        let _: () = msg_send![panel, setBackgroundColor: clear_color];
        let _: () = msg_send![panel, setCanHide: false];
        let _: () = msg_send![panel, setMovable: false];
        let _: () = msg_send![panel, setMovableByWindowBackground: false];
        let _: () = msg_send![panel, setExcludedFromWindowsMenu: true];
        if matches!(window_kind, MacHelperWindowKind::Panel) {
            let _: () = msg_send![panel, setFloatingPanel: true];
            let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: true];
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_apply_native_helper_panel_bounds(panel: *mut Object, bounds: MacWindowBounds) {
    if panel.is_null() {
        return;
    }

    unsafe {
        let _: () = msg_send![panel, setFrame: ns_rect(bounds.x, bounds.y, bounds.width.max(160.0), bounds.height.max(90.0)) display: true];
        let _: () = msg_send![panel, orderFrontRegardless];
    }
}

#[cfg(target_os = "macos")]
fn macos_sync_native_helper_state(state: &mut MacNativeOverlayState) {
    macos_apply_native_helper_panel_style(state.panel, state.window_kind);

    let next_target = macos_poll_overlay_target(
        &mut state.tracker,
        state.attach_mode,
        &state.monitors,
        state.preferred_monitor_index,
    );
    if let Some(target) = next_target {
        let appkit_bounds = macos_top_left_to_appkit_bounds(target.bounds, state.desktop_bottom);
        let previous = state.tracked_window.clone();
        if previous
            .as_ref()
            .map(|prev| prev.window_id != target.window_id)
            .unwrap_or(true)
        {
            info!(
                "native helper tracking macOS window id={} owner='{}' title='{}' pos=({}, {}) size={}x{}",
                target.window_id,
                target.owner_name,
                target.window_name.as_deref().unwrap_or(""),
                target.bounds.x.round() as i32,
                target.bounds.y.round() as i32,
                target.bounds.width.round() as i32,
                target.bounds.height.round() as i32
            );
        }

        if previous
            .as_ref()
            .map(|prev| !same_mac_window_bounds(prev.bounds, target.bounds))
            .unwrap_or(true)
        {
            info!(
                "native helper appkit frame pos=({}, {}) size={}x{}",
                appkit_bounds.x.round() as i32,
                appkit_bounds.y.round() as i32,
                appkit_bounds.width.round() as i32,
                appkit_bounds.height.round() as i32
            );
            macos_apply_native_helper_panel_bounds(state.panel, appkit_bounds);
        }
        state.tracked_window = Some(target);
        return;
    }

    if matches!(state.attach_mode, OverlayAttachMode::FullscreenWindow) {
        if state.tracked_window.is_some() {
            info!(
                "native helper no fullscreen window detected, fallback to monitor pos=({}, {}) size={}x{}",
                state.fallback_bounds.x.round() as i32,
                state.fallback_bounds.y.round() as i32,
                state.fallback_bounds.width.round() as i32,
                state.fallback_bounds.height.round() as i32
            );
        }

        let fallback_bounds =
            macos_top_left_to_appkit_bounds(state.fallback_bounds, state.desktop_bottom);
        if state
            .tracked_window
            .as_ref()
            .map(|prev| !same_mac_window_bounds(prev.bounds, state.fallback_bounds))
            .unwrap_or(true)
        {
            macos_apply_native_helper_panel_bounds(state.panel, fallback_bounds);
        }
        state.tracked_window = None;
        return;
    }

    if state.tracked_window.is_none() {
        let fallback_bounds =
            macos_top_left_to_appkit_bounds(state.fallback_bounds, state.desktop_bottom);
        macos_apply_native_helper_panel_bounds(state.panel, fallback_bounds);
    }
}

#[cfg(target_os = "macos")]
fn macos_top_left_to_appkit_bounds(
    bounds: MacWindowBounds,
    desktop_bottom: f32,
) -> MacWindowBounds {
    MacWindowBounds {
        x: bounds.x,
        y: desktop_bottom - (bounds.y + bounds.height),
        width: bounds.width,
        height: bounds.height,
    }
}

#[cfg(target_os = "macos")]
fn macos_native_helper_timer_target_class() -> &'static Class {
    static TIMER_TARGET_CLASS: OnceLock<&'static Class> = OnceLock::new();
    TIMER_TARGET_CLASS.get_or_init(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("LiverMacNativeOverlayTimerTarget", superclass)
            .expect("timer target class");
        unsafe {
            decl.add_method(
                sel!(tick:),
                macos_native_helper_timer_tick as extern "C" fn(&Object, Sel, *mut Object),
            );
        }
        decl.register()
    })
}

#[cfg(target_os = "macos")]
extern "C" fn macos_native_helper_timer_tick(_this: &Object, _cmd: Sel, _timer: *mut Object) {
    let state_ptr = MACOS_NATIVE_HELPER_STATE.load(Ordering::Relaxed) as *mut MacNativeOverlayState;
    if state_ptr.is_null() {
        return;
    }

    let state = unsafe { &mut *state_ptr };
    macos_sync_native_helper_state(state);
}

#[cfg(target_os = "macos")]
fn ns_rect(x: f32, y: f32, width: f32, height: f32) -> NSRect {
    NSRect {
        origin: NSPoint {
            x: x as CGFloat,
            y: y as CGFloat,
        },
        size: NSSize {
            width: width as CGFloat,
            height: height as CGFloat,
        },
    }
}

#[cfg(target_os = "macos")]
fn nsstring(value: &str) -> *mut Object {
    let c_string = CString::new(value).expect("NSString input");
    unsafe { msg_send![class!(NSString), stringWithUTF8String: c_string.as_ptr()] }
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

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq)]
struct MacWindowBounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq)]
struct MacTrackedWindow {
    window_id: CGWindowID,
    owner_pid: i32,
    owner_name: String,
    window_name: Option<String>,
    bounds: MacWindowBounds,
}

#[cfg(target_os = "macos")]
type AXError = i32;
#[cfg(target_os = "macos")]
type AXUIElementRef = *const c_void;
#[cfg(target_os = "macos")]
type AXValueRef = *const c_void;
#[cfg(target_os = "macos")]
type CGWindowLevel = i32;
#[cfg(target_os = "macos")]
type CGWindowLevelKey = i32;

#[cfg(target_os = "macos")]
const AX_ERROR_SUCCESS: AXError = 0;
#[cfg(target_os = "macos")]
const AX_VALUE_TYPE_CGPOINT: u32 = 1;
#[cfg(target_os = "macos")]
const AX_VALUE_TYPE_CGSIZE: u32 = 2;
#[cfg(target_os = "macos")]
const K_CG_ASSISTIVE_TECH_HIGH_WINDOW_LEVEL_KEY: CGWindowLevelKey = 20;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> u8;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut core_foundation::base::CFTypeRef,
    ) -> AXError;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> AXError;
    fn AXValueGetValue(value: AXValueRef, the_type: u32, value_ptr: *mut c_void) -> u8;
    fn CGWindowLevelForKey(key: CGWindowLevelKey) -> CGWindowLevel;
}

#[cfg(target_os = "macos")]
#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

#[cfg(target_os = "macos")]
struct MacWindowTracker {
    own_pid: i32,
    last_window_id: Option<CGWindowID>,
}

#[cfg(target_os = "macos")]
impl MacWindowTracker {
    fn new() -> Self {
        macos_request_accessibility_prompt();
        macos_request_screen_capture_prompt();
        Self {
            own_pid: std::process::id() as i32,
            last_window_id: None,
        }
    }

    fn poll_frontmost_candidate(&mut self) -> Option<MacTrackedWindow> {
        let frontmost_pid = macos_frontmost_application_pid().filter(|pid| *pid != self.own_pid);
        let accessibility_target = macos_pick_accessibility_focused_window()
            .filter(|window| window.owner_pid != self.own_pid);

        let target =
            accessibility_target.or_else(|| frontmost_pid.and_then(macos_pick_best_window_for_pid));

        if let Some(target) = target.as_ref() {
            self.last_window_id = Some(target.window_id);
        }

        target
    }

    fn poll_target(&mut self) -> Option<MacTrackedWindow> {
        self.poll_frontmost_candidate()
            .or_else(|| self.last_window_id.and_then(macos_find_window_by_id))
            .or_else(|| macos_pick_first_external_window(self.own_pid))
    }
}

struct OverlayApp {
    rx: mpsc::Receiver<DanmakuMessage>,
    attach_mode: OverlayAttachMode,
    danmaku: Vec<ActiveDanmaku>,
    lane_busy_until: Vec<f64>,
    started_at: Instant,
    last_frame: Instant,
    top_padding: f32,
    lane_height: f32,
    #[cfg(target_os = "macos")]
    fallback_bounds: Option<MacWindowBounds>,
    #[cfg(target_os = "macos")]
    monitors: Vec<MonitorSpec>,
    #[cfg(target_os = "macos")]
    preferred_monitor_index: Option<usize>,
    #[cfg(target_os = "macos")]
    window_tracker: Option<MacWindowTracker>,
    #[cfg(target_os = "macos")]
    tracked_window: Option<MacTrackedWindow>,
    #[cfg(target_os = "macos")]
    last_window_sync: Instant,
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
        print!(
            "请选择弹幕显示器编号（回车默认 {}，输入 -1 为不显示悬浮层）: ",
            default_idx
        );
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            error!(
                "failed to read monitor input, fallback to default {}",
                default_idx
            );
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

fn overlay_attach_mode_label(mode: OverlayAttachMode) -> &'static str {
    match mode {
        OverlayAttachMode::Monitor => "monitor",
        OverlayAttachMode::FullscreenWindow => "fullscreen-window",
        OverlayAttachMode::FrontmostWindow => "frontmost-window",
    }
}

fn overlay_attach_mode_uses_window_tracking(mode: OverlayAttachMode) -> bool {
    cfg!(target_os = "macos")
        && matches!(
            mode,
            OverlayAttachMode::FullscreenWindow | OverlayAttachMode::FrontmostWindow
        )
}

fn overlay_top_padding(attach_mode: OverlayAttachMode) -> f32 {
    // macOS fullscreen presentations (especially Keynote) keep a top reserved area.
    let default = if cfg!(target_os = "macos")
        && matches!(
            attach_mode,
            OverlayAttachMode::Monitor | OverlayAttachMode::FullscreenWindow
        ) {
        56.0
    } else {
        20.0
    };
    std::env::var("DANMAKU_TOP_PADDING")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v >= 0.0 && *v <= 300.0)
        .unwrap_or(default)
}

#[cfg(target_os = "macos")]
fn macos_ns_window_hacks_enabled() -> bool {
    std::env::var("DANMAKU_MACOS_NS_WINDOW_HACKS")
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_log_ns_window_hacks_hint_once() {
    if MACOS_NS_WINDOW_HACKS_HINT_PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    info!("macOS raw NSWindow patch path is disabled by default for stability");
    info!("set DANMAKU_MACOS_NS_WINDOW_HACKS=1 to re-enable the experimental AppKit overlay patch");
}

impl OverlayApp {
    fn new(
        rx: mpsc::Receiver<DanmakuMessage>,
        attach_mode: OverlayAttachMode,
        selected_monitor: Option<MonitorSpec>,
        monitors: Vec<MonitorSpec>,
    ) -> Self {
        let top_padding = overlay_top_padding(attach_mode);
        info!(
            "overlay top padding={}, attach_mode={}",
            top_padding,
            overlay_attach_mode_label(attach_mode)
        );
        Self {
            rx,
            attach_mode,
            danmaku: Vec::new(),
            lane_busy_until: Vec::new(),
            started_at: Instant::now(),
            last_frame: Instant::now(),
            top_padding,
            lane_height: 50.0,
            #[cfg(target_os = "macos")]
            fallback_bounds: selected_monitor.as_ref().map(macos_monitor_bounds),
            #[cfg(target_os = "macos")]
            monitors,
            #[cfg(target_os = "macos")]
            preferred_monitor_index: selected_monitor.as_ref().map(|monitor| monitor.index),
            #[cfg(target_os = "macos")]
            window_tracker: overlay_attach_mode_uses_window_tracking(attach_mode)
                .then(MacWindowTracker::new),
            #[cfg(target_os = "macos")]
            tracked_window: None,
            #[cfg(target_os = "macos")]
            last_window_sync: Instant::now() - Duration::from_millis(500),
        }
    }

    fn rebuild_lanes(&mut self, height: f32) {
        let count = ((height - self.top_padding * 2.0) / self.lane_height).max(1.0) as usize;

        if self.lane_busy_until.len() != count {
            self.lane_busy_until = vec![0.0; count];
        }
    }

    fn spawn_danmaku(
        &mut self,
        ctx: &egui::Context,
        msg: DanmakuMessage,
        viewport: Vec2,
        now_s: f64,
    ) {
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

        let galley = ctx.fonts(|fonts| fonts.layout_no_wrap(msg.text.clone(), font.clone(), color));
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
        #[cfg(not(target_os = "macos"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
    }

    #[cfg(target_os = "macos")]
    fn sync_tracked_window(&mut self, ctx: &egui::Context) {
        if !overlay_attach_mode_uses_window_tracking(self.attach_mode) {
            return;
        }

        if self.last_window_sync.elapsed() < Duration::from_millis(150) {
            return;
        }
        self.last_window_sync = Instant::now();
        if macos_ns_window_hacks_enabled() {
            reassert_macos_overlay_window();
        }

        let Some(tracker) = self.window_tracker.as_mut() else {
            return;
        };

        let next_target = macos_poll_overlay_target(
            tracker,
            self.attach_mode,
            &self.monitors,
            self.preferred_monitor_index,
        );
        let Some(target) = next_target else {
            if matches!(self.attach_mode, OverlayAttachMode::FullscreenWindow) {
                if let Some(bounds) = self.fallback_bounds {
                    if self
                        .tracked_window
                        .as_ref()
                        .map(|prev| !same_mac_window_bounds(prev.bounds, bounds))
                        .unwrap_or(true)
                    {
                        info!(
                            "no fullscreen window detected, fallback to monitor pos=({}, {}) size={}x{}",
                            bounds.x.round() as i32,
                            bounds.y.round() as i32,
                            bounds.width.round() as i32,
                            bounds.height.round() as i32
                        );
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                            bounds.x, bounds.y,
                        )));
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                            bounds.width.max(160.0),
                            bounds.height.max(90.0),
                        )));
                    }
                }
                self.tracked_window = None;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            return;
        };

        let previous = self.tracked_window.clone();
        if previous
            .as_ref()
            .map(|prev| prev.window_id != target.window_id)
            .unwrap_or(true)
        {
            info!(
                "tracking macOS window id={} owner='{}' title='{}' pos=({}, {}) size={}x{}",
                target.window_id,
                target.owner_name,
                target.window_name.as_deref().unwrap_or(""),
                target.bounds.x.round() as i32,
                target.bounds.y.round() as i32,
                target.bounds.width.round() as i32,
                target.bounds.height.round() as i32
            );
        }

        if previous
            .as_ref()
            .map(|prev| !same_mac_window_bounds(prev.bounds, target.bounds))
            .unwrap_or(true)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                target.bounds.x,
                target.bounds.y,
            )));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                target.bounds.width.max(160.0),
                target.bounds.height.max(90.0),
            )));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));

        self.tracked_window = Some(target);
    }

    #[cfg(not(target_os = "macos"))]
    fn sync_tracked_window(&mut self, _ctx: &egui::Context) {}
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

        self.apply_overlay_window_flags(ctx);
        self.sync_tracked_window(ctx);

        let viewport = ctx.screen_rect().size();

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

                painter.text(
                    Pos2::new(14.0, 10.0),
                    Align2::LEFT_TOP,
                    "Liver Running",
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

#[cfg(target_os = "macos")]
fn same_mac_window_bounds(a: MacWindowBounds, b: MacWindowBounds) -> bool {
    (a.x - b.x).abs() < 1.0
        && (a.y - b.y).abs() < 1.0
        && (a.width - b.width).abs() < 1.0
        && (a.height - b.height).abs() < 1.0
}

#[cfg(target_os = "macos")]
fn mac_window_bounds_delta(a: MacWindowBounds, b: MacWindowBounds) -> f32 {
    (a.x - b.x).abs() + (a.y - b.y).abs() + (a.width - b.width).abs() + (a.height - b.height).abs()
}

#[cfg(target_os = "macos")]
fn macos_accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

#[cfg(target_os = "macos")]
fn macos_log_accessibility_hint_once() {
    if AX_PERMISSION_HINT_PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    warn!("macOS 全屏窗口跟随优先依赖辅助功能权限（Accessibility）");
    warn!("请到 系统设置 -> 隐私与安全性 -> 辅助功能 中允许当前终端/应用");
    warn!("未授权时会退回到普通窗口枚举，全屏 Space 场景可能失效");
}

#[cfg(target_os = "macos")]
fn macos_request_accessibility_prompt() {
    if macos_accessibility_trusted() || AX_PERMISSION_PROMPTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let options: CFDictionary<CFString, CFBoolean> = CFDictionary::from_CFType_pairs(&[(
        CFString::from_static_string("AXTrustedCheckOptionPrompt"),
        CFBoolean::true_value(),
    )]);
    let _ = unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
    macos_log_accessibility_hint_once();
}

#[cfg(target_os = "macos")]
fn macos_screen_capture_trusted() -> bool {
    let access = ScreenCaptureAccess;
    access.preflight()
}

#[cfg(target_os = "macos")]
fn macos_log_screen_capture_hint_once() {
    if SCREEN_CAPTURE_HINT_PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    warn!("macOS 窗口跟随同样建议授予屏幕录制（Screen Recording）权限");
    warn!("请到 系统设置 -> 隐私与安全性 -> 屏幕录制 中允许当前终端/应用");
    warn!("未授权时 Quartz / CGWindowListCopyWindowInfo 可能返回被过滤的窗口元数据");
    warn!("这会让原生全屏 Space 下的窗口匹配与 bounds 同步变得不稳定");
}

#[cfg(target_os = "macos")]
fn macos_request_screen_capture_prompt() {
    if macos_screen_capture_trusted() || SCREEN_CAPTURE_PROMPTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let access = ScreenCaptureAccess;
    if !access.request() {
        macos_log_screen_capture_hint_once();
    }
}

#[cfg(target_os = "macos")]
fn macos_copy_window_info() -> Option<core_graphics::display::CFArray> {
    if !macos_screen_capture_trusted() {
        macos_log_screen_capture_hint_once();
    }

    copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )
}

#[cfg(target_os = "macos")]
fn macos_ax_copy_attribute(element: AXUIElementRef, attribute: &'static str) -> Option<CFType> {
    let attr = CFString::from_static_string(attribute);
    let mut value = ptr::null();
    let error =
        unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
    if error == AX_ERROR_SUCCESS && !value.is_null() {
        Some(unsafe { CFType::wrap_under_create_rule(value) })
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_ax_value_point(value: &CFType) -> Option<CGPoint> {
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef() as AXValueRef,
            AX_VALUE_TYPE_CGPOINT,
            &mut point as *mut _ as *mut c_void,
        )
    };
    (ok != 0).then_some(point)
}

#[cfg(target_os = "macos")]
fn macos_ax_value_size(value: &CFType) -> Option<CGSize> {
    let mut size = CGSize {
        width: 0.0,
        height: 0.0,
    };
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef() as AXValueRef,
            AX_VALUE_TYPE_CGSIZE,
            &mut size as *mut _ as *mut c_void,
        )
    };
    (ok != 0).then_some(size)
}

#[cfg(target_os = "macos")]
fn macos_ax_pid(element: AXUIElementRef) -> Option<i32> {
    let mut pid = 0i32;
    let error = unsafe { AXUIElementGetPid(element, &mut pid) };
    (error == AX_ERROR_SUCCESS).then_some(pid)
}

#[cfg(target_os = "macos")]
fn macos_pick_accessibility_focused_window() -> Option<MacTrackedWindow> {
    if !macos_accessibility_trusted() {
        macos_log_accessibility_hint_once();
        return None;
    }

    let system_ref = unsafe { AXUIElementCreateSystemWide() };
    if system_ref.is_null() {
        return None;
    }
    let system =
        unsafe { CFType::wrap_under_create_rule(system_ref as core_foundation::base::CFTypeRef) };

    let focused_app = macos_ax_copy_attribute(
        system.as_CFTypeRef() as AXUIElementRef,
        "AXFocusedApplication",
    )?;
    let focused_app_ref = focused_app.as_CFTypeRef() as AXUIElementRef;
    let owner_pid = macos_ax_pid(focused_app_ref)?;

    let focused_window = macos_ax_copy_attribute(focused_app_ref, "AXFocusedWindow")
        .or_else(|| macos_ax_copy_attribute(focused_app_ref, "AXMainWindow"))?;
    let focused_window_ref = focused_window.as_CFTypeRef() as AXUIElementRef;

    let position = macos_ax_copy_attribute(focused_window_ref, "AXPosition")
        .and_then(|value| macos_ax_value_point(&value))?;
    let size = macos_ax_copy_attribute(focused_window_ref, "AXSize")
        .and_then(|value| macos_ax_value_size(&value))?;

    if size.width < 160.0 || size.height < 90.0 {
        return None;
    }

    let window_name =
        cf_type_to_string(&focused_window, "AXTitle").filter(|title| !title.trim().is_empty());
    let bounds = MacWindowBounds {
        x: position.x as f32,
        y: position.y as f32,
        width: size.width as f32,
        height: size.height as f32,
    };

    macos_pick_window_matching_target(owner_pid, bounds, window_name.as_deref()).or_else(|| {
        macos_pick_best_window_for_pid(owner_pid).map(|mut tracked| {
            tracked.window_name = window_name;
            tracked
        })
    })
}

#[cfg(target_os = "macos")]
fn cf_type_to_string(element: &CFType, attribute: &'static str) -> Option<String> {
    let value = macos_ax_copy_attribute(element.as_CFTypeRef() as AXUIElementRef, attribute)?;
    Some(value.downcast::<CFString>()?.to_string())
}

#[cfg(target_os = "macos")]
fn macos_frontmost_application_pid() -> Option<i32> {
    unsafe {
        let workspace: *mut objc::runtime::Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut objc::runtime::Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        Some(pid)
    }
}

#[cfg(target_os = "macos")]
fn macos_pick_best_window_for_pid(pid: i32) -> Option<MacTrackedWindow> {
    let windows = macos_copy_window_info()?;

    let mut best: Option<MacTrackedWindow> = None;
    let mut best_area = 0.0f32;

    for entry in &windows {
        let Some(window) = macos_window_from_array_entry(*entry) else {
            continue;
        };
        if window.owner_pid != pid {
            continue;
        }

        let area = window.bounds.width * window.bounds.height;
        if area > best_area {
            best_area = area;
            best = Some(window);
        }
    }

    best
}

#[cfg(target_os = "macos")]
fn macos_find_window_by_id(window_id: CGWindowID) -> Option<MacTrackedWindow> {
    let windows = macos_copy_window_info()?;

    for entry in &windows {
        let Some(window) = macos_window_from_array_entry(*entry) else {
            continue;
        };
        if window.window_id == window_id {
            return Some(window);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn macos_pick_window_matching_target(
    owner_pid: i32,
    target_bounds: MacWindowBounds,
    target_title: Option<&str>,
) -> Option<MacTrackedWindow> {
    let windows = macos_copy_window_info()?;

    let mut best: Option<MacTrackedWindow> = None;
    let mut best_title_mismatch = i32::MAX;
    let mut best_bounds_delta = f32::MAX;

    for entry in &windows {
        let Some(window) = macos_window_from_array_entry(*entry) else {
            continue;
        };
        if window.owner_pid != owner_pid {
            continue;
        }

        let title_mismatch = match (target_title, window.window_name.as_deref()) {
            (Some(expected), Some(actual)) if expected == actual => 0,
            (Some(_), _) => 1,
            (None, _) => 0,
        };
        let bounds_delta = mac_window_bounds_delta(window.bounds, target_bounds);

        if title_mismatch < best_title_mismatch
            || (title_mismatch == best_title_mismatch && bounds_delta < best_bounds_delta)
        {
            best_title_mismatch = title_mismatch;
            best_bounds_delta = bounds_delta;
            best = Some(window);
        }
    }

    best
}

#[cfg(target_os = "macos")]
fn macos_pick_first_external_window(exclude_pid: i32) -> Option<MacTrackedWindow> {
    let windows = macos_copy_window_info()?;

    for entry in &windows {
        let Some(window) = macos_window_from_array_entry(*entry) else {
            continue;
        };
        if window.owner_pid == exclude_pid {
            continue;
        }
        return Some(window);
    }

    None
}

#[cfg(target_os = "macos")]
fn macos_window_from_array_entry(entry: *const c_void) -> Option<MacTrackedWindow> {
    if entry.is_null() {
        return None;
    }

    let dict_ref = entry as core_foundation::dictionary::CFDictionaryRef;
    let info: CFDictionary<CFString, CFType> = unsafe { TCFType::wrap_under_get_rule(dict_ref) };
    macos_window_from_info(&info)
}

#[cfg(target_os = "macos")]
fn macos_window_from_info(info: &CFDictionary<CFString, CFType>) -> Option<MacTrackedWindow> {
    let window_id = cf_dict_u32(info, unsafe { kCGWindowNumber })?;
    let owner_pid = cf_dict_i32(info, unsafe { kCGWindowOwnerPID })?;
    let layer = cf_dict_i32(info, unsafe { kCGWindowLayer })?;
    let alpha = cf_dict_f64(info, unsafe { kCGWindowAlpha }).unwrap_or(1.0) as f32;
    let onscreen = cf_dict_bool(info, unsafe { kCGWindowIsOnscreen }).unwrap_or(true);
    let bounds = cf_dict_rect(info, unsafe { kCGWindowBounds })?;

    if !onscreen || layer < 0 || alpha <= 0.01 {
        return None;
    }
    if bounds.width < 160.0 || bounds.height < 90.0 {
        return None;
    }

    Some(MacTrackedWindow {
        window_id,
        owner_pid,
        owner_name: cf_dict_string(info, unsafe { kCGWindowOwnerName })
            .unwrap_or_else(|| format!("pid-{}", owner_pid)),
        window_name: cf_dict_string(info, unsafe { kCGWindowName })
            .filter(|title| !title.trim().is_empty()),
        bounds,
    })
}

#[cfg(target_os = "macos")]
fn cf_dict_u32(info: &CFDictionary<CFString, CFType>, key_ref: CFStringRef) -> Option<u32> {
    let key = cf_string_from_ref(key_ref);
    info.find(&key)?
        .downcast::<CFNumber>()?
        .to_i32()
        .map(|value| value as u32)
}

#[cfg(target_os = "macos")]
fn cf_dict_i32(info: &CFDictionary<CFString, CFType>, key_ref: CFStringRef) -> Option<i32> {
    let key = cf_string_from_ref(key_ref);
    info.find(&key)?.downcast::<CFNumber>()?.to_i32()
}

#[cfg(target_os = "macos")]
fn cf_dict_f64(info: &CFDictionary<CFString, CFType>, key_ref: CFStringRef) -> Option<f64> {
    let key = cf_string_from_ref(key_ref);
    info.find(&key)?.downcast::<CFNumber>()?.to_f64()
}

#[cfg(target_os = "macos")]
fn cf_dict_bool(info: &CFDictionary<CFString, CFType>, key_ref: CFStringRef) -> Option<bool> {
    let key = cf_string_from_ref(key_ref);
    Some(bool::from(info.find(&key)?.downcast::<CFBoolean>()?))
}

#[cfg(target_os = "macos")]
fn cf_dict_string(info: &CFDictionary<CFString, CFType>, key_ref: CFStringRef) -> Option<String> {
    let key = cf_string_from_ref(key_ref);
    Some(info.find(&key)?.downcast::<CFString>()?.to_string())
}

#[cfg(target_os = "macos")]
fn cf_dict_rect(
    info: &CFDictionary<CFString, CFType>,
    key_ref: CFStringRef,
) -> Option<MacWindowBounds> {
    let key = cf_string_from_ref(key_ref);
    let bounds = info.find(&key)?.downcast::<CFDictionary>()?;

    Some(MacWindowBounds {
        x: cf_untyped_dict_f64(&bounds, "X")? as f32,
        y: cf_untyped_dict_f64(&bounds, "Y")? as f32,
        width: cf_untyped_dict_f64(&bounds, "Width")? as f32,
        height: cf_untyped_dict_f64(&bounds, "Height")? as f32,
    })
}

#[cfg(target_os = "macos")]
fn cf_untyped_dict_f64(info: &CFDictionary, key: &'static str) -> Option<f64> {
    let key = CFString::from_static_string(key);
    let value_ptr = *info.find(key.as_CFTypeRef() as *const c_void)?;
    let value_ref = value_ptr as core_foundation::base::CFTypeRef;
    let value = unsafe { CFType::wrap_under_get_rule(value_ref) };
    value.downcast::<CFNumber>()?.to_f64()
}

#[cfg(target_os = "macos")]
fn cf_string_from_ref(key_ref: CFStringRef) -> CFString {
    unsafe { TCFType::wrap_under_get_rule(key_ref) }
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

    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.set_fonts(fonts);
    }))
    .is_err()
    {
        warn!("failed to apply custom CJK fonts; fallback to default egui fonts");
        return;
    }
    let names: Vec<&str> = loaded.iter().map(|(_, path)| *path).collect();
    info!("overlay loaded CJK font(s): {}", names.join(", "));
}

#[cfg(target_os = "macos")]
fn macos_overlay_window_level() -> isize {
    if let Some(level) = std::env::var("DANMAKU_MACOS_WINDOW_LEVEL")
        .ok()
        .and_then(|value| value.parse::<isize>().ok())
        .filter(|value| (0..=5000).contains(value))
    {
        return level;
    }

    // Ask CoreGraphics for the current Assistive Tech level instead of baking
    // in a literal, so we stay aligned with the system on future macOS builds.
    let system_level =
        unsafe { CGWindowLevelForKey(K_CG_ASSISTIVE_TECH_HIGH_WINDOW_LEVEL_KEY) as isize };
    if system_level > 0 {
        system_level
    } else {
        1500
    }
}

#[cfg(target_os = "macos")]
fn apply_macos_overlay_window_style(ns_window: *mut objc::runtime::Object) {
    const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE: usize = 1 << 6;
    const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: usize = 1 << 8;

    if ns_window.is_null() {
        return;
    }

    unsafe {
        // Default to the screen saver level so the overlay has a better chance
        // of staying above native full-screen spaces on macOS.
        let overlay_level = macos_overlay_window_level();
        let behavior: usize = msg_send![ns_window, collectionBehavior];
        let overlay_behavior = behavior
            | NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY;

        let _: () = msg_send![ns_window, setCollectionBehavior: overlay_behavior];
        let _: () = msg_send![ns_window, setLevel: overlay_level];
        let _: () = msg_send![ns_window, setOpaque: false];
        let _: () = msg_send![ns_window, setHasShadow: false];
        let _: () = msg_send![ns_window, setHidesOnDeactivate: false];
        let _: () = msg_send![ns_window, setIgnoresMouseEvents: true];

        let clear_color: *mut objc::runtime::Object = msg_send![class!(NSColor), clearColor];
        if !clear_color.is_null() {
            let _: () = msg_send![ns_window, setBackgroundColor: clear_color];
        }

        let _: () = msg_send![ns_window, orderFrontRegardless];
    }
}

#[cfg(target_os = "macos")]
fn reassert_macos_overlay_window() {
    let ns_window = MACOS_OVERLAY_WINDOW.load(Ordering::Relaxed) as *mut objc::runtime::Object;
    if ns_window.is_null() {
        return;
    }
    apply_macos_overlay_window_style(ns_window);
}

#[cfg(target_os = "macos")]
fn configure_macos_overlay_window(cc: &eframe::CreationContext<'_>) {
    let Ok(window_handle) = cc.window_handle() else {
        warn!("failed to access macOS window handle");
        return;
    };

    let RawWindowHandle::AppKit(handle) = window_handle.as_raw() else {
        warn!("unexpected raw window handle on macOS");
        return;
    };

    unsafe {
        let ns_view = handle.ns_view.as_ptr() as *mut objc::runtime::Object;
        let ns_window: *mut objc::runtime::Object = msg_send![ns_view, window];
        if ns_window.is_null() {
            warn!("failed to access NSWindow from NSView");
            return;
        }
        MACOS_OVERLAY_WINDOW.store(ns_window.cast(), Ordering::Relaxed);
        apply_macos_overlay_window_style(ns_window);
        let overlay_level = macos_overlay_window_level();
        info!("configured macOS overlay window level={}", overlay_level);
    }
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
    if s.len() == 7 && s.starts_with('#') && s.chars().skip(1).all(|c| c.is_ascii_hexdigit()) {
        s.to_string()
    } else {
        "#ffffff".to_string()
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
