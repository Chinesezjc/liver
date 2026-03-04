# Liver Danmaku (Rust)

支持两部分：
- `axum` 服务端：接收弹幕并通过 WebSocket 广播
- 桌面悬浮弹幕层（原生窗口）：置顶、透明、全屏，从右向左飘过，覆盖其他程序

## 启动方式

先确保当前终端可用 `cargo`。如果刚安装 Rust，请重开一个终端。

支持 `Windows / macOS / Linux`。

1. 默认同时启动服务端 + 悬浮层：

```bash
cargo run
```

2. 只启动服务端：

```bash
cargo run -- --server
```

3. 只启动悬浮层（要求服务端已运行在本机 `3000` 端口）：

```bash
cargo run -- --overlay
```

4. 启动包含悬浮层的模式时，会自动在命令行打印显示器列表并等待你输入编号。

例如：

```bash
[0] Color LCD (primary)
[1] EPSON Projector
请选择弹幕显示器编号（回车默认 0，输入 -1 为不显示悬浮层）:
```

如果只运行服务端（`--server`），不会进入显示器选择交互。

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
