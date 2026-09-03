use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

fn usage() {
    eprintln!(
        "Usage:\n  usbipd bind [--serial SERIAL]... [--vid HEX]... [--pid HEX]... [--listen ADDR] [--stop-adb]\n  usbipd --version\n  usbipd help"
    );
    eprintln!(
        "\nOptions are repeatable; a device matches when it passes every provided filter set."
    );
    eprintln!("At least one of --serial/--vid/--pid is required.");
    eprintln!("\nDefault listen address: 0.0.0.0:3240");
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
    if serials.is_empty() && vids.is_empty() && pids.is_empty() {
        eprintln!("at least one of --serial/--vid/--pid is required");
        usage();
        std::process::exit(2);
    }
    let listen = option_value(args, "--listen")
        .unwrap_or_else(|| "0.0.0.0:3240".to_string())
        .parse::<SocketAddr>()
        .unwrap_or_else(|err| {
            eprintln!("invalid --listen address: {err}");
            std::process::exit(2);
        });
    let stop_adb = args.iter().any(|arg| arg == "--stop-adb");

    if stop_adb {
        stop_adb_server();
    }

    let server = Arc::new(usbip::UsbIpServer::new_from_host_with_filter(
        move |device| {
            let Ok(descriptor) = device.device_descriptor() else {
                return false;
            };
            if !vids.is_empty() && !vids.contains(&descriptor.vendor_id()) {
                return false;
            }
            if !pids.is_empty() && !pids.contains(&descriptor.product_id()) {
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

    println!("USB/IP server listening on {listen}");
    println!("Press Ctrl-C to stop.");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
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

    if let Err(err) = runtime.block_on(usbip::try_server(listen, server)) {
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
