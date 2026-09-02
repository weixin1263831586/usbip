# usbip

一个使用 Rust 编写的用户态 USB/IP 服务端，支持将物理 USB 设备通过网络转发给另一台 Linux 主机。

## 工作原理

本项目负责服务端，客户端仍使用 Ubuntu 系统自带的 USB/IP 工具：

```text
服务端物理 USB
      ↓
本项目通过 libusb 打开并 claim USB 接口
      ↓
usbipd 通过 TCP 3240 转发 USB 请求
      ↓
客户端 vhci-hcd 创建虚拟 USB
      ↓
客户端系统、ADB 或其他程序使用该 USB 设备
```

Ubuntu 原生的 `usbip-host.ko` 并不是完全不能导出 USB，键盘、鼠标等简单设备通常可以正常工作。但 Android 手机、开发板等复合 Gadget 设备包含 ADB、MTP、PTP 等多个接口，被内核 `usbip-host` 接管时可能断开并重新枚举，导致设备从 exportable 列表消失。

本项目使用用户态 libusb 转发，不让 `usbip-host.ko` 接管整个物理设备，因此更适合 Android 设备。客户端仍然需要 `vhci-hcd`，这两者不是同一个实现。

## 两台主机的角色

| 主机 | 操作 |
| --- | --- |
| USB 服务端 | 设备实际插在这台机器上，运行 `usbipd bind` |
| USB 客户端 | 运行 `usbip list`、`usbip attach`，使用远程设备 |

以下示例中：

- 服务端：`172.16.14.246`
- 客户端：`172.16.14.233`
- 设备：VID `2207`、PID `0006`

## 服务端使用

### 1. 安装 usbipd

#### 一键安装

在 Linux x86_64 主机上，可以直接下载 GitHub Releases 中的预编译文件：

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/weixin1263831586/usbip/master/install.sh | sh
```

脚本只下载 GitHub Releases 中由 `target/release/usbipd` 生成的预编译文件，校验通过后安装到 `/usr/local/bin/usbipd`，不会拉取源码或现场编译。

#### 手动安装编译结果

如果已经在项目目录编译出 `target/release/usbipd`，直接安装：

```bash
cargo build --release --bin usbipd
sudo install -m 0755 target/release/usbipd /usr/local/bin/usbipd
usbipd --version
```

以后直接使用 `usbipd`，不需要再输入构建目录中的路径。

项目维护者发布新版本时，创建并推送版本 tag：

```bash
git tag v0.9.1
git push origin v0.9.1
```

GitHub Actions 会自动编译 `target/release/usbipd`，并将 `usbipd-linux-x86_64` 和校验文件上传到对应 Release。

### 2. 查询设备序列号

```bash
lsusb
lsusb -v -d 2207:0006 2>/dev/null | grep -E 'iManufacturer|iProduct|iSerial'
```

序列号必须填写当前 USB 描述符实际返回的值。设备重新枚举后，序列号或 BUSID 可能变化。

### 3. 启动服务

Android 设备推荐使用：

```bash
usbipd bind \
  --stop-adb \
  --vid 2207 \
  --pid 0006 \
  --serial YOUR_USB_SERIAL
```

参数说明：

- `--serial`：必填，选择要导出的设备；
- `--vid`、`--pid`：可选，用于进一步限制设备；
- `--stop-adb`：先停止当前用户的 ADB；
- 默认监听 `0.0.0.0:3240`；
- 按 `Ctrl-C` 停止服务。

如果 CTS、GMS Worker 或其他后台任务会自动启动 ADB，单次 `--stop-adb` 可能不够，需要先暂停会自动拉起 ADB 的任务。

如果看到：

```text
cannot connect to daemon at tcp:5037: Connection refused
```

表示 ADB 本来就没有运行，可以忽略，USB/IP 服务仍会继续启动。

## Linux 客户端使用

### 1. 安装并加载客户端组件

```bash
sudo apt update
sudo apt install usbip linux-tools-generic linux-cloud-tools-generic -y
sudo modprobe vhci_hcd
```

### 2. 查询服务端设备

```bash
usbip list -r 172.16.14.246
```

输出示例：

```text
1-17-13: Fuzhou Rockchip Electronics Company : unknown product (2207:0006)
```

这里的 `1-17-13` 是当前 BUSID。设备重新枚举后可能变成 `1-18-13`，每次挂载前都要重新查询。

### 3. 挂载设备

```bash
sudo usbip attach -r 172.16.14.246 -b 1-17-13
```

`attach` 成功时通常没有输出。它只提交挂载请求，客户端内核还需要异步完成 USB 枚举，ADB 也需要再次轮询设备。因此第一次执行 `adb devices` 看不到设备是正常的，等待几秒后再检查：

```bash
sudo usbip port
lsusb
adb devices
```

确认设备状态为 `device` 后即可使用：

```bash
adb shell
```

客户端 ADB 显示的设备 ID 不一定等于服务端 `--serial`，两者不需要相同。

### 4. 卸载设备

先查看端口：

```bash
sudo usbip port
```

再卸载对应端口：

```bash
sudo usbip detach -p 1
```

## 权限说明

服务端不一定需要 `sudo`。只要当前用户有权限访问 `/dev/bus/usb/*`，即可直接运行 `usbipd bind`。没有权限时，最简单的方式是：

```bash
sudo usbipd bind --vid 2207 --pid 0006 --serial YOUR_SERIAL
```

客户端的 `modprobe`、`attach` 和 `detach` 通常需要 `sudo`，因为它们需要操作内核的 `vhci-hcd`。

## 常见问题

### `no exportable devices found`

服务端没有找到可导出的匹配设备，依次检查：

1. `--serial`、VID、PID 是否正确；
2. ADB 或其他程序是否占用了设备；
3. 服务端用户是否有 USB 设备节点权限；
4. 是否启动了旧的 `usbipd`，占用了 3240 端口。

### `attach` 没有输出，ADB 暂时看不到

这是 USB 异步枚举延时。先执行 `sudo usbip port` 和 `lsusb`，等待几秒后再执行 `adb devices`。如果 BUSID 已变化，重新执行 `usbip list -r SERVER_IP`。

### 设备连接后又掉线

优先检查服务端是否有 ADB、CTS、GMS Worker 或其他程序重新抢占设备。Android 设备发生模式切换时，也可能改变 PID、序列号或 BUSID，需要重新查询后启动服务。

### `Address already in use`

说明 3240 已经被其他 `usbipd` 或旧的 `host` 进程占用。停止旧进程，或者指定其他监听地址：

```bash
usbipd bind --listen 0.0.0.0:3241 --serial YOUR_SERIAL
```

客户端使用对应端口：

```bash
sudo usbip attach -r 172.16.14.246 -b BUSID
```

## 开发

```bash
cargo test
cargo fmt --check
```

项目也保留模拟 HID 和 CDC ACM 设备示例，可用于测试 USB/IP 协议本身。

## 许可证

MIT License，详见 [LICENSE](LICENSE)。
