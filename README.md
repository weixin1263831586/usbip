# usbip

[![Coverage Status](https://coveralls.io/repos/github/jiegec/usbip/badge.svg?branch=master)](https://coveralls.io/github/jiegec/usbip?branch=master)
[![crates.io](https://img.shields.io/crates/v/usbip.svg)](https://crates.io/crates/usbip)
[![Documentation](https://docs.rs/usbip/badge.svg)](https://docs.rs/usbip)

一个用于运行 USB/IP 服务端的 Rust 库。它既可以导出模拟 USB 设备，也可以把服务端物理 USB 设备的传输请求转发到网络客户端。

## USB/IP 是什么

USB/IP 通过 TCP 传输 USB 设备操作。远程客户端可以发现本项目导出的设备，并把它挂载成本机 USB 设备来使用。

本项目支持：

- 模拟 HID 键盘和 CDC ACM 串口设备；
- 将控制传输、Bulk 传输和 Interrupt 传输转发到物理 USB 设备；
- 处理 USB/IP \`CMD_UNLINK\` 请求，并取消对应的物理 USB 传输；
- 物理设备重新枚举后，在序列号不变时自动重新连接；
- 通过 Tokio 异步 API 集成到其他 Rust 应用中。

本项目实现的是 USB/IP 服务端，不提供独立的 \`usbip\` 命令行客户端程序。Linux 客户端需要使用系统自带的 USB/IP 工具。

## 工作模式

USB/IP 使用时涉及两台机器：

| 角色 | 作用 |
| --- | --- |
| USB 主机 / 服务端 | 物理 USB 设备插在这台机器上，运行本项目的 \`host\` 程序。 |
| USB 客户端 | 运行 \`usbip attach\`，把远程设备挂载到本机。 |

对于 Android 设备，通常需要先停止占用 USB 设备的 ADB 服务，再启动 USB/IP 服务端。

## 环境要求

安装 Rust：[官方安装说明](https://www.rust-lang.org/tools/install)。本项目使用 Rust edition 2024。

转发物理 USB 设备时，服务端必须能通过 libusb 打开并 claim 设备接口。Linux 下可以使用 udev 规则授权，也可以暂时使用 root 权限运行。

客户端需要操作系统提供 USB/IP 支持。Linux 客户端需要安装 \`usbip\` 工具，并加载 \`vhci-hcd\` 内核模块。

本项目通过用户态 libusb 直接访问物理设备，因此服务端不需要把设备绑定到内核的 \`usbip-host\` 驱动。

## 快速开始：Android 或其他物理 USB 设备

以下命令都在“USB 设备实际插入的那台机器”上执行。

### 1. 编译服务端程序

在项目根目录执行：

\`\`\`bash
cargo build --release --example host
\`\`\`

### 2. 启动服务端

推荐使用项目自带的启动脚本。它会自动定位 release binary，并且不再需要手写很长的 \`sudo bash -lc\` 命令：

\`\`\`bash
./scripts/usbip-host.sh \
  --vid 2207 \
  --pid 0006 \
  --serial bb41e7b689aba45
\`\`\`

其中：

- \`--vid\`：USB Vendor ID，十六进制；
- \`--pid\`：USB Product ID，十六进制；
- \`--serial\`：USB 序列号，必须与设备上报的序列号完全一致；
- 默认监听地址为 \`0.0.0.0:3240\`。

如果 ADB 服务占用了设备，可以使用 \`--stop-adb\`：

\`\`\`bash
./scripts/usbip-host.sh --stop-adb \
  --vid 2207 \
  --pid 0006 \
  --serial bb41e7b689aba45
\`\`\`

这个选项执行的是 \`adb kill-server\`，不会永久关闭 ADB。USB/IP 设备卸载后，如需恢复本地 ADB，可以执行：

\`\`\`bash
adb start-server
\`\`\`

### 3. 使用环境变量保存设备信息

如果经常使用同一个设备，可以不用每次重复输入参数：

\`\`\`bash
export USBIP_VID=2207
export USBIP_PID=0006
export USBIP_SERIAL=bb41e7b689aba45

./scripts/usbip-host.sh --stop-adb
\`\`\`

也可以设置监听地址：

\`\`\`bash
export USBIP_LISTEN=0.0.0.0:3241
./scripts/usbip-host.sh
\`\`\`

如果项目不在当前目录，或者 release binary 在其他位置，可以设置：

\`\`\`bash
USBIP_HOST_BIN=/home/wlq/usbip/target/release/examples/host \
  ./scripts/usbip-host.sh --stop-adb \
  --vid 2207 --pid 0006 --serial bb41e7b689aba45
\`\`\`

### 4. 直接运行 binary

启动脚本只是为了方便，也可以直接运行：

\`\`\`bash
sudo /path/to/usbip/target/release/examples/host \
  --vid 2207 \
  --pid 0006 \
  --serial bb41e7b689aba45
\`\`\`

完整参数说明：

\`\`\`bash
./scripts/usbip-host.sh --help

# 或
cargo run --example host -- --help
\`\`\`

## 免 sudo 运行

\`sudo\` 不是 USB/IP 本身的要求，只是因为当前用户可能没有访问 USB 设备节点的权限。

Linux 下可以为指定设备添加 udev 规则：

\`\`\`bash
sudo tee /etc/udev/rules.d/70-usbip-android.rules >/dev/null <<'EOF'
SUBSYSTEM=="usb", ATTR{idVendor}=="2207", ATTR{idProduct}=="0006", ATTR{serial}=="bb41e7b689aba45", TAG+="uaccess"
EOF

sudo udevadm control --reload-rules
sudo udevadm trigger
\`\`\`

拔出并重新插入设备后，使用普通用户运行：

\`\`\`bash
./scripts/usbip-host.sh --no-sudo \
  --vid 2207 \
  --pid 0006 \
  --serial bb41e7b689aba45
\`\`\`

如果是 systemd 服务，建议使用专用用户组，并在 udev 规则中使用 \`GROUP="usbip"\` 和 \`MODE="0660"\`。\`TAG+="uaccess"\` 主要适用于当前有登录会话的普通用户。

## Linux 客户端连接

以下命令在需要使用远程 USB 设备的客户端机器上执行。

### 1. 加载客户端内核模块

\`\`\`bash
sudo modprobe vhci-hcd
\`\`\`

### 2. 查询服务端设备

假设服务端 IP 是 \`192.168.1.10\`：

\`\`\`bash
usbip list --remote 192.168.1.10
\`\`\`

输出中会包含类似 \`1-2-1\` 的 \`BUSID\`。

### 3. 挂载远程设备

\`\`\`bash
sudo usbip attach \
  --remote 192.168.1.10 \
  --busid 1-2-1
\`\`\`

如果服务端和客户端在同一台机器上：

\`\`\`bash
usbip list --remote 127.0.0.1
sudo usbip attach --remote 127.0.0.1 --busid 1-2-1
\`\`\`

挂载成功后，设备会出现在客户端系统中，可以像本地 USB 设备一样使用。

### 4. 查看和卸载设备

查看当前挂载端口：

\`\`\`bash
usbip port
\`\`\`

卸载设备：

\`\`\`bash
sudo usbip detach --port PORT
\`\`\`

服务端默认监听 TCP \`3240\` 端口。跨机器使用时，需要确保服务端防火墙允许该端口访问。

## 其他示例

项目提供三个可运行示例：

| 示例 | 说明 |
| --- | --- |
| \`hid_keyboard\` | 导出一个模拟 HID 键盘，每秒发送一次按键 \`1\`。 |
| \`cdc_acm_serial\` | 导出一个模拟 CDC ACM 串口设备，每秒发送一次字符 \`a\`。 |
| \`host\` | 按 VID、PID 和序列号导出一个物理 USB 设备。 |

所有示例默认监听 \`0.0.0.0:3240\`。

启动模拟 HID 键盘：

\`\`\`bash
cargo run --example hid_keyboard
\`\`\`

启动模拟 CDC ACM 串口：

\`\`\`bash
cargo run --example cdc_acm_serial
\`\`\`

直接启动物理设备示例：

\`\`\`bash
cargo run --example host -- \
  --vid 0x18d1 \
  --pid 0x4ee7 \
  --serial YOUR_SERIAL
\`\`\`

修改监听地址或端口：

\`\`\`bash
cargo run --example host -- \
  --vid 0x18d1 \
  --pid 0x4ee7 \
  --serial YOUR_SERIAL \
  --listen 0.0.0.0:3241
\`\`\`

使用 \`lsusb\` 可以查看 USB 设备信息：

\`\`\`bash
lsusb
\`\`\`

## 在其他 Rust 项目中使用

在 \`Cargo.toml\` 中添加依赖：

\`\`\`toml
[dependencies]
usbip = "0.9"
tokio = { version = "1", features = ["macros", "net", "rt-multi-thread", "time"] }
\`\`\`

\`serde\` 是可选 feature，可以为公开的 USB 描述符和协议类型启用序列化支持：

\`\`\`toml
usbip = { version = "0.9", features = ["serde"] }
\`\`\`

最小服务端示例：

\`\`\`rust,no_run
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let devices = vec![usbip::UsbDevice::new(0)];
    let server = Arc::new(usbip::UsbIpServer::new_simulated(devices));
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3240);

    usbip::server(address, server).await;
}
\`\`\`

自定义模拟设备时，可以使用 \`UsbDevice::with_interface\` 添加接口和端点，并实现 \`UsbInterfaceHandler\`。\`hid\` 和 \`cdc\` 模块中包含完整的处理器示例。

## 物理设备行为说明

服务端创建时会打开物理设备并 claim 其接口。USB 传输以异步方式转发，客户端发起 unlink 请求时会取消对应的 libusb 传输。

设备重新枚举后，Linux 可能会给它分配新的设备地址；某些设备在模式切换时还可能改变 PID。只要设备序列号保持不变，服务端就会通过 VID 和序列号查找并重新 claim 新设备。

没有序列号的设备会退回使用原始 VID/PID 进行匹配。

一个导出设备同时只能被一个客户端连接使用。客户端断开后，设备会回到服务端的可用设备列表中。

保持服务端进程持续运行，才能在物理设备重新枚举后自动重新连接。

## 使用 systemd 常驻运行

如果服务端需要开机自动启动，建议使用 systemd，而不是手工维护一条很长的 shell 命令。下面是示例配置，请根据实际路径和设备信息修改：

\`\`\`ini
# /etc/systemd/system/usbip-host.service
[Unit]
Description=USB/IP 物理 USB 设备服务端
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/usbip/target/release/examples/host --vid 2207 --pid 0006 --serial bb41e7b689aba45
Restart=on-failure
RestartSec=1

# 配置好 udev 规则后，可以改成有 USB 权限的普通用户。
User=root

[Install]
WantedBy=multi-user.target
\`\`\`

启用服务：

\`\`\`bash
sudo systemctl daemon-reload
sudo systemctl enable --now usbip-host.service
sudo systemctl status usbip-host.service
\`\`\`

查看实时日志：

\`\`\`bash
journalctl -u usbip-host.service -f
\`\`\`

## 常见问题

- **没有导出设备：** 先执行 \`lsusb\`，确认 VID、PID 和序列号。序列号比较区分大小写，并且必须完全匹配。
- **出现 \`Permission denied\` 或 claim interface 失败：** 添加 udev 规则，或者先使用启动脚本默认的 sudo 模式。
- **设备被占用：** 停止正在使用该设备的进程。Android 设备可以使用 \`adb kill-server\` 或启动脚本的 \`--stop-adb\`。
- **客户端查询不到设备：** 检查服务端是否监听 TCP \`3240\`、防火墙是否放行，以及客户端是否加载了 \`vhci-hcd\`。
- **提示地址已被占用：** 停止之前的 host 进程，或者传入不同的 \`--listen ADDR\`；同一个地址和端口只能被一个进程监听。
- **设备重新枚举后 BUSID 变化：** 不要重启服务端进程。只要序列号稳定，服务端会根据序列号重新寻找设备。
- **\`adb kill-server\` 后仍然占用设备：** 检查是否有 root 用户启动的 ADB 进程，必要时执行 \`sudo pkill -x adb\`，然后重新启动 USB/IP 服务端。

## 从源码构建

\`\`\`bash
git clone https://github.com/jiegec/usbip.git
cd usbip
cargo build --release
\`\`\`

运行测试：

\`\`\`bash
cargo test
\`\`\`

## 许可证

MIT License，详见 [LICENSE](LICENSE)。
