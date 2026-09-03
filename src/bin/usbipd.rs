use std::net::{IpAddr, SocketAddr};
use std::process::Command;
use std::sync::Arc;

use usbip::ServerOptions;

fn usage() {
    eprintln!(
        "Usage:\n  usbipd bind [--serial SERIAL]... [--vid HEX]... [--pid HEX]... [--device VID:PID]... [--listen ADDR] [--allow-client IP]... [--stop-adb]\n  usbipd --version\n  usbipd help"
    );
    eprintln!(
        "\nOptions are repeatable; --serial/--vid/--pid are independent filter sets while\n--device VID:PID matches exact (vendor, product) pairs."
    );
    eprintln!("At least one of --serial/--vid/--pid/--device is required.");
    eprintln!(
        "\nDefault listen address: 127.0.0.1:3240 (loopback only; pass --listen to expose a LAN/Tailscale interface)."
    );
    eprintln!(
        "--allow-client restricts TCP peers to the given IPs (repeatable, empty = allow all)."
    );
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn option_values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .collect()
}

fn parse_hex(value: &str, name: &str) -> u16 {
    u16::from_str_radix(value.trim_start_matches("0x").trim_start_matches("0X"), 16).unwrap_or_else(
        |_| {
            eprintln!("invalid {name}: {value}");
            std::process::exit(2);
        },
    )
}

fn parse_vid_pid(value: &str) -> Option<(u16, u16)> {
    let value = value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let (vid, pid) = value.split_once(':')?;
    if vid.len() != 4 || pid.len() != 4 {
        return None;
    }
    Some((
        u16::from_str_radix(vid, 16).ok()?,
        u16::from_str_radix(pid, 16).ok()?,
    ))
}

fn stop_adb_server() {
    let sudo_user = std::env::var("SUDO_USER")
        .ok()
        .filter(|user| !user.is_empty() && user != "root");

    let status = if let Some(user) = sudo_user {
        Command::new("sudo")
            .args(["-u", &user, "-H", "adb", "kill-server"])
            .status()
    } else {
        Command::new("adb").arg("kill-server").status()
    };

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: adb kill-server exited with {status}"),
        Err(err) => eprintln!("warning: could not run adb kill-server: {err}"),
    }
}

fn run_bind(args: &[String]) {
    let serials = option_values(args, "--serial")
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>();
    let vids = option_values(args, "--vid")
        .into_iter()
        .map(|value| parse_hex(&value, "--vid"))
        .collect::<Vec<_>>();
    let pids = option_values(args, "--pid")
        .into_iter()
        .map(|value| parse_hex(&value, "--pid"))
        .collect::<Vec<_>>();
    let vid_pids = option_values(args, "--device")
        .into_iter()
        .map(|value| {
            parse_vid_pid(&value).unwrap_or_else(|| {
                eprintln!("invalid --device (expected VID:PID, e.g. 2207:0006): {value}");
                std::process::exit(2);
            })
        })
        .collect::<Vec<_>>();
    if serials.is_empty() && vids.is_empty() && pids.is_empty() && vid_pids.is_empty() {
        eprintln!("at least one of --serial/--vid/--pid/--device is required");
        usage();
        std::process::exit(2);
    }
    // Default to loopback so an unconfigured deployment never exposes USB
    // devices to the whole network (P0: bind to a trusted interface with
    // --listen, or allowlist peers with --allow-client).
    let listen = option_value(args, "--listen")
        .unwrap_or_else(|| "127.0.0.1:3240".to_string())
        .parse::<SocketAddr>()
        .unwrap_or_else(|err| {
            eprintln!("invalid --listen address: {err}");
            std::process::exit(2);
        });
    let mut options = ServerOptions::new();
    let allow_clients = option_values(args, "--allow-client");
    for value in &allow_clients {
        let ip: IpAddr = value.parse().unwrap_or_else(|err| {
            eprintln!("invalid --allow-client IP {value}: {err}");
            std::process::exit(2);
        });
        options = options.allow_client(ip);
    }
    let stop_adb = args.iter().any(|arg| arg == "--stop-adb");

    if stop_adb {
        stop_adb_server();
    }

    let server = Arc::new(usbip::UsbIpServer::new_from_host_with_filter(
        move |device| {
            let Ok(descriptor) = device.device_descriptor() else {
                return false;
            };
            let vendor_id = descriptor.vendor_id();
            let product_id = descriptor.product_id();
            // --vid/--pid are independent sets (legacy behaviour); --device
            // pairs must match exactly as (VID, PID) tuples, avoiding the
            // cartesian VID×PID over-matching of independent --vid/--pid.
            let paired_match = vid_pids.is_empty()
                || vid_pids
                    .iter()
                    .any(|(vid, pid)| *vid == vendor_id && *pid == product_id);
            if !paired_match {
                return false;
            }
            if !vids.is_empty() && !vids.contains(&vendor_id) {
                return false;
            }
            if !pids.is_empty() && !pids.contains(&product_id) {
                return false;
            }
            if serials.is_empty() {
                return true;
            }
            device
                .open()
                .and_then(|handle| handle.read_serial_number_string_ascii(&descriptor))
                .is_ok_and(|device_serial| serials.iter().any(|s| s == &device_serial))
        },
    ));

    match std::net::TcpListener::bind(listen) {
        Ok(listener) => drop(listener),
        Err(err) => {
            eprintln!("无法监听 {listen}: {err}");
            eprintln!("请停止占用该端口的旧 usbipd/host 进程，或使用 --listen 指定其他端口。",);
            std::process::exit(1);
        }
    }

    if listen.ip().is_unspecified() && allow_clients.is_empty() {
        eprintln!(
            "warning: 监听 {listen} 且未配置 --allow-client，任何可达该地址的主机都可访问被导出的 USB 设备。"
        );
        eprintln!("建议使用 --listen 指定可信接口，或追加 --allow-client 限制客户端 IP。");
    }

    println!("USB/IP server listening on {listen}");
    if !allow_clients.is_empty() {
        println!("Allowed clients: {}", allow_clients.join(", "));
    }
    println!("Press Ctrl-C to stop.");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("create Tokio runtime");

    let available_devices = runtime.block_on(server.available_device_count());
    if available_devices == 0 {
        eprintln!("未找到匹配的 USB 设备，服务端不会导出任何设备。请检查：");
        eprintln!("1. 序列号、VID/PID 是否与当前 lsusb 结果一致；");
        eprintln!("2. ADB 或其他程序是否占用了设备；Android 设备可先使用 --stop-adb；");
        eprintln!("3. 普通用户是否有 /dev/bus/usb/* 的访问权限。可以使用 sudo 重试。",);
        std::process::exit(1);
    }

    if let Err(err) = runtime.block_on(usbip::try_server_with_options(listen, server, options)) {
        eprintln!("无法启动 USB/IP 服务端 {listen}: {err}");
        std::process::exit(1);
    }
}

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args[0] == "help" || args[0] == "--help" || args[0] == "-h" {
        usage();
        return;
    }

    if args[0] == "--version" || args[0] == "-V" || args[0] == "version" {
        println!("usbipd {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let command = args.remove(0);
    match command.as_str() {
        "bind" => run_bind(&args),
        _ => {
            eprintln!("unknown command: {command}");
            usage();
            std::process::exit(2);
        }
    }
}
