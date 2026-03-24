# Liver Danmaku (Rust)

支持两部分：
- `axum` 服务端：接收弹幕并通过 WebSocket 广播
- 桌面悬浮弹幕层（原生窗口）：置顶、透明、点击穿透，支持按显示器覆盖，也支持自动追踪前台窗口（macOS）

## 启动方式

先确保当前终端可用 `cargo`。如果刚安装 Rust，请重开一个终端。

支持 `Windows / macOS / Linux`。

1. 默认同时启动服务端 + 悬浮层：

```bash
cargo run
```

- macOS 默认会自动追踪当前前台窗口，适合 Keynote / 会议窗口 / 演示窗口。
- Windows / Linux 默认按显示器显示弹幕。

2. 只启动服务端：

```bash
cargo run -- --server
```

3. 只启动悬浮层（要求服务端已运行在本机 `3000` 端口）：

```bash
cargo run -- --overlay
```

4. 如果你想强制切回“按显示器覆盖”的模式，可以使用：

```bash
cargo run -- --follow-monitor
```

5. 如果你想显式开启“自动追踪前台窗口”的模式，可以使用：

```bash
cargo run -- --follow-window
```

6. 启动包含悬浮层的模式时，在“按显示器覆盖”模式下会自动打印显示器列表并等待你输入编号。

例如：

```bash
[0] Color LCD (primary)
[1] EPSON Projector
请选择弹幕显示器编号（回车默认 0，输入 -1 为不显示悬浮层）:
```

如果只运行服务端（`--server`），不会进入显示器选择交互。

## macOS 说明

- macOS 默认启用“自动追踪前台窗口”，弹幕层会跟随当前最前面的应用窗口移动和缩放。
- macOS 的 `--follow-window` 现在默认优先走原生 helper panel 路线：独立 `NSPanel + WKWebView` 承载透明弹幕层，更接近腾讯会议这类会议软件的 overlay 形态。
- macOS 的窗口跟随优先基于 Quartz / Window Server 的具体窗口实体（`CGWindowID`）同步 bounds，而不是只按前台应用做模糊匹配。
- helper panel 会加入所有 Space，并作为全屏辅助窗口显示；这一步主要是为了尽量贴近会议软件在 macOS 上的 overlay 行为。
- macOS 悬浮层默认会尝试使用更高的 `screen saver` 级别窗口层级；如果想临时回退，可以用 `DANMAKU_MACOS_WINDOW_LEVEL=26 cargo run -- --follow-window` 做对比测试。
- 如果你想临时回退到旧的 `eframe` 路线，可以执行：`DANMAKU_MACOS_OVERLAY_BACKEND=eframe cargo run -- --follow-window`
- 首次使用 `--follow-window` 时，macOS 可能会弹出“辅助功能（Accessibility）”授权提示。请允许当前终端或该应用，否则系统不会把全屏窗口的焦点与位置提供给程序。
- 为了让 Quartz 的窗口列表在原生全屏场景下更稳定，macOS 也建议授予“屏幕录制（Screen Recording）”权限；未授权时，`CGWindowListCopyWindowInfo` 返回的窗口元数据可能被系统过滤。
- 已补充 Keynote 支持：Keynote / 腾讯会议 / 演示类全屏窗口会优先通过辅助功能接口跟踪；未授权时会退回普通窗口枚举，因此全屏 Space 场景可能不稳定。
- 如果当前前台窗口不可用，会暂时保留最近一次的窗口位置；也可以改用 `--follow-monitor` 回到传统的整屏覆盖模式。

## 内网穿透（跨网络访问）

推荐使用 Cloudflare Tunnel，把本机 `3000` 端口映射成公网 HTTPS 地址。

### 1) 安装 cloudflared

- Windows: `winget install --id Cloudflare.cloudflared -e`
- macOS: `brew install cloudflared`

### 2) 主程序内置 Tunnel 逻辑

默认启动时会询问是否开启 Tunnel。  
可通过参数控制：

- `--tunnel`：强制开启，不询问
- `--no-tunnel`：强制关闭，不询问
- `--edge-ip-version 4|6|auto`：指定 tunnel 使用 IPv4/IPv6（默认 `auto` 自动判定）

示例：

```bash
cargo run -- --server --tunnel
cargo run -- --all --no-tunnel
cargo run -- --server --tunnel --edge-ip-version 4
```

### 3) 打开 client

脚本启动后，终端会打印一个类似：

`https://xxxx.trycloudflare.com`

在任意设备浏览器打开：

`https://xxxx.trycloudflare.com/client`

即可发送弹幕到你本地服务端。

## 页面与接口

- 发送端网页：`http://127.0.0.1:3000/client`
- （可选）浏览器屏幕页：`http://127.0.0.1:3000/screen`
- 弹幕投递接口：`POST /api/danmaku`

示例请求体：

```json
{
  "text": "你好，世界",
  "color": "#ffffff",
  "speed": 90
}
```

字段说明：
- `text`：必填，最多 120 字符
- `color`：可选，`#RRGGBB`
- `speed`：可选，40-240（像素/秒）

## 致谢

- 感谢 **Qiuly** 进行 macOS 系统环境测试。
- 感谢 **Cloudflare** 提供 Tunnel 服务支持。
