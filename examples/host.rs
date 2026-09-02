use std::net::*;
use std::sync::Arc;
use std::time::Duration;

fn required_arg(args: &[String], name: &str) -> String {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| {
            eprintln!("usage: host --vid HEX --pid HEX --serial SERIAL");
            std::process::exit(2);
        })
}

fn parse_hex_u16(value: &str, name: &str) -> u16 {
    u16::from_str_radix(value.trim_start_matches("0x"), 16).unwrap_or_else(|_| {
        eprintln!("invalid {name}: {value}");
        std::process::exit(2);
    })
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vid = parse_hex_u16(&required_arg(&args, "--vid"), "--vid");
    let pid = parse_hex_u16(&required_arg(&args, "--pid"), "--pid");
    let serial = required_arg(&args, "--serial");

    let server = Arc::new(usbip::UsbIpServer::new_from_host_with_filter(
        move |device| {
            let Ok(descriptor) = device.device_descriptor() else {
                return false;
            };
            if descriptor.vendor_id() != vid || descriptor.product_id() != pid {
                return false;
            }

            device
                .open()
                .and_then(|handle| handle.read_serial_number_string_ascii(&descriptor))
                .is_ok_and(|device_serial| device_serial == serial)
        },
    ));
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 3240);
    tokio::spawn(usbip::server(addr, server));

    loop {
        // sleep 1s
        tokio::time::sleep(Duration::new(1, 0)).await;
    }
}
