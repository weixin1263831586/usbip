//! A library for running a USB/IP server

use log::*;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use nusb::MaybeFuture;
use rusb::*;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore, mpsc};
use usbip_protocol::UsbIpCommand;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

mod async_transfer;
pub mod cdc;
mod consts;
mod device;
mod endpoint;
pub mod hid;
mod host;
mod interface;
mod setup;
pub mod usbip_protocol;
mod util;
pub use consts::*;
pub use device::*;
pub use endpoint::*;
pub use host::*;
pub use interface::*;
pub use setup::*;
pub use util::*;

use crate::usbip_protocol::{USBIP_RET_SUBMIT, USBIP_RET_UNLINK, UsbIpResponse};

fn rusb_speed_to_usbip(speed: rusb::Speed) -> u32 {
    match speed {
        rusb::Speed::Unknown => UsbSpeed::Unknown as u32,
        rusb::Speed::Low => UsbSpeed::Low as u32,
        rusb::Speed::Full => UsbSpeed::Full as u32,
        rusb::Speed::High => UsbSpeed::High as u32,
        rusb::Speed::Super => UsbSpeed::Super as u32,
        rusb::Speed::SuperPlus => UsbSpeed::SuperPlus as u32,
        _ => UsbSpeed::Unknown as u32,
    }
}

/// Main struct of a USB/IP server
#[derive(Default, Debug)]
pub struct UsbIpServer {
    available_devices: RwLock<Vec<UsbDevice>>,
    used_devices: RwLock<HashMap<String, UsbDevice>>,
}

impl UsbIpServer {
    /// Create a [UsbIpServer] with simulated devices
    pub fn new_simulated(devices: Vec<UsbDevice>) -> Self {
        Self {
            available_devices: RwLock::new(devices),
            used_devices: RwLock::new(HashMap::new()),
        }
    }

    /// Create a [UsbIpServer] with Vec<[nusb::DeviceInfo]> for sharing host devices
    pub fn with_nusb_devices(nusb_device_infos: Vec<nusb::DeviceInfo>) -> Vec<UsbDevice> {
        let mut devices = vec![];
        for device_info in nusb_device_infos {
            let dev = match device_info.open().wait() {
                Ok(dev) => dev,
                Err(err) => {
                    warn!("Impossible to open device {device_info:?}: {err}, ignoring device",);
                    continue;
                }
            };
            let cfg = match dev.active_configuration() {
                Ok(cfg) => cfg,
                Err(err) => {
                    warn!(
                        "Impossible to get active configuration {device_info:?}: {err}, ignoring device",
                    );
                    continue;
                }
            };
            let mut interfaces = vec![];
            for intf in cfg.interfaces() {
                // ignore alternate settings
                let intf_num = intf.interface_number();
                let intf = dev.claim_interface(intf_num).wait().unwrap();
                let alt_setting = intf.descriptors().next().unwrap();
                let mut endpoints = vec![];

                for ep_desc in alt_setting.endpoints() {
                    endpoints.push(UsbEndpoint {
                        address: ep_desc.address(),
                        attributes: ep_desc.transfer_type() as u8,
                        max_packet_size: ep_desc.max_packet_size() as u16,
                        interval: ep_desc.interval(),
                    });
                }

                let handler = Arc::new(Mutex::new(Box::new(NusbUsbHostInterfaceHandler::new(
                    Arc::new(Mutex::new(intf.clone())),
                ))
                    as Box<dyn UsbInterfaceHandler + Send>));
                interfaces.push(UsbInterface {
                    interface_class: alt_setting.class(),
                    interface_subclass: alt_setting.subclass(),
                    interface_protocol: alt_setting.protocol(),
                    endpoints,
                    string_interface: alt_setting.string_index().map(|nz| nz.get()).unwrap_or(0),
                    class_specific_descriptor: Vec::new(),
                    handler,
                });
            }

            // Platform-specific bus number (Linux-only)
            let bus_num_val: u32;
            #[cfg(target_os = "linux")]
            {
                bus_num_val = device_info.busnum() as u32;
            }
            #[cfg(not(target_os = "linux"))]
            {
                bus_num_val = 0;
            }

            let device_address = device_info.device_address();

            let mut device = UsbDevice {
                path: format!("/sys/bus/{}/{}/{}", bus_num_val, device_address, 0),
                bus_id: format!("{}-{}-{}", bus_num_val, device_address, 0),
                bus_num: bus_num_val,
                dev_num: device_address as u32,
                speed: device_info.speed().unwrap() as u32,
                vendor_id: device_info.vendor_id(),
                product_id: device_info.product_id(),
                device_class: device_info.class(),
                device_subclass: device_info.subclass(),
                device_protocol: device_info.protocol(),
                device_bcd: device_info.device_version().into(),
                configuration_value: cfg.configuration_value(),
                num_configurations: dev.configurations().count() as u8,
                ep0_in: UsbEndpoint {
                    address: 0x80,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: 16,
                    interval: 0,
                },
                ep0_out: UsbEndpoint {
                    address: 0x00,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: 16,
                    interval: 0,
                },
                interfaces,
                device_handler: Some(Arc::new(Mutex::new(Box::new(
                    NusbUsbHostDeviceHandler::new(Arc::new(Mutex::new(dev))),
                )))),
                ..UsbDevice::default()
            };

            // set strings
            if let Some(s) = device_info.manufacturer_string() {
                device.string_manufacturer = device.new_string(s)
            }
            if let Some(s) = device_info.product_string() {
                device.string_product = device.new_string(s)
            }
            if let Some(s) = device_info.serial_number() {
                device.string_serial = device.new_string(s)
            }
            devices.push(device);
        }
        devices
    }

    /// Create a [UsbIpServer] with Vec<[rusb::DeviceHandle]> for sharing host devices
    pub fn with_rusb_device_handles(
        device_handles: Vec<DeviceHandle<GlobalContext>>,
    ) -> Vec<UsbDevice> {
        let mut devices = vec![];
        for open_device in device_handles {
            let dev = open_device.device();
            let desc = match dev.device_descriptor() {
                Ok(desc) => desc,
                Err(err) => {
                    warn!(
                        "Impossible to get device descriptor for {dev:?}: {err}, ignoring device",
                    );
                    continue;
                }
            };
            let cfg = match dev.active_config_descriptor() {
                Ok(desc) => desc,
                Err(err) => {
                    warn!(
                        "Impossible to get config descriptor for {dev:?}: {err}, ignoring device",
                    );
                    continue;
                }
            };

            let handle = Arc::new(open_device);
            let mut interfaces = vec![];
            let mut interface_numbers = vec![];
            handle.set_auto_detach_kernel_driver(true).ok();
            for intf in cfg.interfaces() {
                // ignore alternate settings
                let intf_desc = intf.descriptors().next().unwrap();
                if let Err(err) = handle.claim_interface(intf_desc.interface_number()) {
                    warn!(
                        "Impossible to claim interface {} for {dev:?}: {err}, ignoring device",
                        intf_desc.interface_number()
                    );
                    interfaces.clear();
                    break;
                }
                interface_numbers.push(intf_desc.interface_number());
                let mut endpoints = vec![];

                for ep_desc in intf_desc.endpoint_descriptors() {
                    endpoints.push(UsbEndpoint {
                        address: ep_desc.address(),
                        attributes: ep_desc.transfer_type() as u8,
                        max_packet_size: ep_desc.max_packet_size(),
                        interval: ep_desc.interval(),
                    });
                }

                let handler = Arc::new(Mutex::new(Box::new(RusbUsbHostInterfaceHandler::new(
                    handle.clone(),
                ))
                    as Box<dyn UsbInterfaceHandler + Send>));
                interfaces.push(UsbInterface {
                    interface_class: intf_desc.class_code(),
                    interface_subclass: intf_desc.sub_class_code(),
                    interface_protocol: intf_desc.protocol_code(),
                    endpoints,
                    string_interface: intf_desc.description_string_index().unwrap_or(0),
                    class_specific_descriptor: Vec::from(intf_desc.extra()),
                    handler,
                });
            }
            if interfaces.is_empty() {
                continue;
            }
            let physical_serial = desc
                .serial_number_string_index()
                .and_then(|index| handle.read_string_descriptor_ascii(index).ok())
                .unwrap_or_default();
            let host_runtime = Arc::new(async_transfer::HostDeviceRuntime::new(
                handle.clone(),
                desc.vendor_id(),
                desc.product_id(),
                physical_serial,
                interface_numbers,
            ));
            let endpoint_locks = interfaces
                .iter()
                .flat_map(|intf| intf.endpoints.iter().map(|ep| ep.address))
                .map(|address| (address, Arc::new(tokio::sync::Semaphore::new(1))))
                .collect();
            let mut device = UsbDevice {
                path: format!(
                    "/sys/bus/{}/{}/{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_id: format!(
                    "{}-{}-{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_num: dev.bus_number() as u32,
                // USB/IP's devnum is the USB device address, not the
                // physical hub port. The latter is already the third part
                // of bus_id (for example, bus-address-port: 1-11-13).
                dev_num: dev.address() as u32,
                speed: rusb_speed_to_usbip(dev.speed()),
                vendor_id: desc.vendor_id(),
                product_id: desc.product_id(),
                device_class: desc.class_code(),
                device_subclass: desc.sub_class_code(),
                device_protocol: desc.protocol_code(),
                device_bcd: desc.device_version().into(),
                configuration_value: cfg.number(),
                num_configurations: desc.num_configurations(),
                ep0_in: UsbEndpoint {
                    address: 0x80,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                ep0_out: UsbEndpoint {
                    address: 0x00,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                interfaces,
                device_handler: Some(Arc::new(Mutex::new(Box::new(
                    RusbUsbHostDeviceHandler::new(handle.clone()),
                )))),
                host_runtime: Some(host_runtime),
                host_endpoint_locks: Some(Arc::new(endpoint_locks)),
                usb_version: desc.usb_version().into(),
                ..UsbDevice::default()
            };

            // set strings
            //
            // Devices are not required to respond successfully to a string descriptor
            // read (some transiently NAK it, some don't implement the language ID we
            // ask for). A failure here used to be treated as fatal via .unwrap(), which
            // crashed enumeration - and with it every other device - instead of just
            // leaving that one string unset.
            if let Some(index) = desc.manufacturer_string_index() {
                match handle.read_string_descriptor_ascii(index) {
                    Ok(s) => device.string_manufacturer = device.new_string(&s),
                    Err(err) => warn!(
                        "[{:04x}:{:04x} {}] failed to read manufacturer string descriptor (index={index}): {err}",
                        device.vendor_id, device.product_id, device.bus_id
                    ),
                }
            }
            if let Some(index) = desc.product_string_index() {
                match handle.read_string_descriptor_ascii(index) {
                    Ok(s) => device.string_product = device.new_string(&s),
                    Err(err) => warn!(
                        "[{:04x}:{:04x} {}] failed to read product string descriptor (index={index}): {err}",
                        device.vendor_id, device.product_id, device.bus_id
                    ),
                }
            }
            if let Some(index) = desc.serial_number_string_index() {
                match handle.read_string_descriptor_ascii(index) {
                    Ok(s) => device.string_serial = device.new_string(&s),
                    Err(err) => warn!(
                        "[{:04x}:{:04x} {}] failed to read serial number string descriptor (index={index}): {err}",
                        device.vendor_id, device.product_id, device.bus_id
                    ),
                }
            }
            devices.push(device);
        }
        devices
    }

    fn with_rusb_devices(device_list: Vec<Device<GlobalContext>>) -> Vec<UsbDevice> {
        let mut device_handles = vec![];

        for dev in device_list {
            let open_device = match dev.open() {
                Ok(dev) => dev,
                Err(err) => {
                    warn!("Impossible to share {dev:?}: {err}, ignoring device");
                    continue;
                }
            };
            device_handles.push(open_device);
        }
        Self::with_rusb_device_handles(device_handles)
    }

    /// Create a [UsbIpServer] exposing devices in the host, and redirect all USB transfers to them using libusb
    pub fn new_from_host() -> Self {
        Self::new_from_host_with_filter(|_| true)
    }

    /// Create a [UsbIpServer] exposing filtered devices in the host, and redirect all USB transfers to them using libusb
    pub fn new_from_host_with_filter<F>(filter: F) -> Self
    where
        F: FnMut(&Device<GlobalContext>) -> bool,
    {
        match rusb::devices() {
            Ok(list) => {
                let mut devs = vec![];
                for d in list.iter().filter(filter) {
                    devs.push(d)
                }
                Self {
                    available_devices: RwLock::new(Self::with_rusb_devices(devs)),
                    ..Default::default()
                }
            }
            Err(_) => Default::default(),
        }
    }

    pub async fn add_device(&self, device: UsbDevice) {
        self.available_devices.write().await.push(device);
    }

    /// Return the number of devices currently available for import.
    pub async fn available_device_count(&self) -> usize {
        self.available_devices.read().await.len()
    }

    pub async fn remove_device(&self, bus_id: &str) -> Result<()> {
        let mut available_devices = self.available_devices.write().await;

        if let Some(device) = available_devices.iter().position(|d| d.bus_id == bus_id) {
            available_devices.remove(device);
            Ok(())
        } else if let Some(device) = self
            .used_devices
            .read()
            .await
            .values()
            .find(|d| d.bus_id == bus_id)
        {
            Err(std::io::Error::other(format!(
                "Device {} is in use",
                device.bus_id
            )))
        } else {
            Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("Device {bus_id} not found"),
            ))
        }
    }
}

struct CompletedSubmit {
    seqnum: u32,
    status: i32,
    response: UsbIpResponse,
}

impl Clone for CompletedSubmit {
    fn clone(&self) -> Self {
        Self {
            seqnum: self.seqnum,
            status: self.status,
            response: self.response.clone(),
        }
    }
}

struct PendingUrb {
    cancellation: Arc<async_transfer::TransferCancellation>,
    unlink_header: Option<usbip_protocol::UsbIpHeaderBasic>,
}

fn io_error_to_errno(error: &std::io::Error) -> i32 {
    match error.kind() {
        ErrorKind::NotFound => -2,
        ErrorKind::PermissionDenied => -13,
        ErrorKind::ConnectionRefused => -111,
        ErrorKind::ConnectionReset => -104,
        ErrorKind::ConnectionAborted => -103,
        ErrorKind::NotConnected => -107,
        ErrorKind::InvalidInput | ErrorKind::InvalidData => -22,
        ErrorKind::TimedOut => -110,
        ErrorKind::Interrupted => -4,
        ErrorKind::Unsupported => -95,
        ErrorKind::OutOfMemory => -12,
        _ => -5,
    }
}

async fn process_submit(
    device: UsbDevice,
    command: UsbIpCommand,
    cancellation: Arc<async_transfer::TransferCancellation>,
) -> CompletedSubmit {
    let UsbIpCommand::UsbIpCmdSubmit {
        mut header,
        transfer_flags,
        transfer_buffer_length,
        setup,
        data,
        ..
    } = command
    else {
        unreachable!("process_submit called with a non-submit command");
    };

    trace!("Got USBIP_CMD_SUBMIT");
    let out = header.direction == 0;
    let real_ep = if out { header.ep } else { header.ep | 0x80 };

    header.command = USBIP_RET_SUBMIT.into();

    let seqnum = header.seqnum;
    let (status, actual_length, response_data) = match device.find_ep(real_ep as u8) {
        None => {
            warn!("Endpoint {real_ep:02x?} not found");
            (-2, 0, Vec::new())
        }
        Some((ep, intf)) => {
            trace!("->Endpoint {ep:02x?}");
            trace!("->Setup {setup:02x?}");
            trace!("->Request {data:02x?}");
            if let Some(runtime) = &device.host_runtime
                && ep.attributes != EndpointAttributes::Control as u8
            {
                let _endpoint_permit = if let Some(lock) = device
                    .host_endpoint_locks
                    .as_ref()
                    .and_then(|locks| locks.get(&ep.address))
                {
                    Some(
                        lock.clone()
                            .acquire_owned()
                            .await
                            .expect("endpoint semaphore closed"),
                    )
                } else {
                    None
                };
                let transfer_data = data.clone();
                let mut result = if let Some(handle) = runtime.current() {
                    async_transfer::submit_transfer(
                        handle,
                        ep,
                        transfer_flags,
                        transfer_buffer_length,
                        SetupPacket::parse(&setup),
                        data,
                        cancellation.clone(),
                    )
                    .await
                } else {
                    Err(std::io::Error::new(
                        ErrorKind::NotConnected,
                        "physical USB device is disconnected",
                    ))
                };
                if matches!(result, Ok(ref completion) if completion.status == async_transfer::ENODEV_ERRNO)
                    && !cancellation.is_cancelled()
                {
                    let reconnect_runtime = runtime.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || reconnect_runtime.reconnect()).await;
                    if let Some(handle) = runtime.current() {
                        result = async_transfer::submit_transfer(
                            handle,
                            ep,
                            transfer_flags,
                            transfer_buffer_length,
                            SetupPacket::parse(&setup),
                            transfer_data,
                            cancellation.clone(),
                        )
                        .await;
                    }
                }
                match result {
                    Ok(completion) => {
                        (completion.status, completion.actual_length, completion.data)
                    }
                    Err(err) => {
                        warn!("Error handling asynchronous URB: {err}");
                        (io_error_to_errno(&err), 0, Vec::new())
                    }
                }
            } else {
                match device
                    .handle_urb(
                        ep,
                        intf,
                        transfer_buffer_length,
                        SetupPacket::parse(&setup),
                        &data,
                    )
                    .await
                {
                    Ok(resp) => {
                        let actual_length = if out {
                            debug_assert!(resp.is_empty());
                            data.len() as u32
                        } else {
                            resp.len() as u32
                        };
                        (0, actual_length, resp)
                    }
                    Err(err) => {
                        warn!("Error handling URB: {err}");
                        (io_error_to_errno(&err), 0, Vec::new())
                    }
                }
            }
        }
    };
    if out {
        trace!("<-Wrote {actual_length}");
    } else {
        trace!("<-Read {actual_length}");
    }

    CompletedSubmit {
        seqnum,
        status,
        response: UsbIpResponse::usbip_ret_submit_result(
            &header,
            status,
            0,
            0,
            actual_length,
            response_data,
            vec![],
        ),
    }
}

/// Maximum concurrently handled TCP connections (device import sessions).
pub const MAX_CONNECTIONS: usize = 32;

/// Maximum URBs in flight for a single connection.
pub const MAX_PENDING_URBS: usize = 256;

/// Idle timeout applied while waiting for the next USB/IP command.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard deadline for draining cancelled/in-flight URBs after the peer went
/// away. A wedged host-side cancellation must not pin the exported device
/// forever; late completions after the deadline are dropped.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Server-side accept policy and resource limits.
#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// When non-empty, only peers with these IP addresses may connect.
    pub allow_clients: Vec<IpAddr>,
    /// Explicitly accept clients from any address (fail-closed override,
    /// set by `--allow-any-client`). Loopback peers are always accepted.
    pub allow_any_client: bool,
}

impl ServerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow_client(mut self, ip: IpAddr) -> Self {
        self.allow_clients.push(ip);
        self
    }

    pub fn with_allow_any_client(mut self, allow: bool) -> Self {
        self.allow_any_client = allow;
        self
    }

    fn client_allowed(&self, peer: SocketAddr) -> bool {
        // Local tooling (usbip list via an SSH tunnel, diagnostics) talks
        // from loopback; it is trusted regardless of the allowlist.
        if peer.ip().is_loopback() {
            return true;
        }
        self.allow_any_client || self.allow_clients.contains(&peer.ip())
    }
}

pub async fn handler<T: AsyncReadExt + AsyncWriteExt + Unpin>(
    socket: &mut T,
    server: Arc<UsbIpServer>,
) -> Result<()> {
    handler_with_options(socket, server, &ServerOptions::default()).await
}

pub async fn handler_with_options<T: AsyncReadExt + AsyncWriteExt + Unpin>(
    socket: &mut T,
    server: Arc<UsbIpServer>,
    _options: &ServerOptions,
) -> Result<()> {
    let (completed_tx, mut completed_rx) = mpsc::channel::<CompletedSubmit>(MAX_PENDING_URBS);
    let (mut reader, mut writer) = tokio::io::split(socket);
    let mut imported_device_id: Option<String> = None;
    let result = handler_loop(
        &mut reader,
        &mut writer,
        server.clone(),
        completed_tx,
        &mut completed_rx,
        &mut imported_device_id,
    )
    .await;
    // Unified device release (P0): the imported device is returned to the
    // available pool on EVERY exit path — clean EOF, protocol violation,
    // write failure, or task teardown — so a broken or malicious client can
    // never soft-lock it as "in use" until daemon restart.
    if let Some(dev_id) = imported_device_id.take() {
        let mut used_devices = server.used_devices.write().await;
        let mut available_devices = server.available_devices.write().await;
        if let Some(dev) = used_devices.remove(&dev_id) {
            info!("Released imported device {dev_id} back to available pool");
            available_devices.push(dev);
        }
    }
    result
}

async fn handler_loop<R, W>(
    reader: &mut R,
    writer: &mut W,
    server: Arc<UsbIpServer>,
    completed_tx: mpsc::Sender<CompletedSubmit>,
    completed_rx: &mut mpsc::Receiver<CompletedSubmit>,
    imported_device_id: &mut Option<String>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut current_import_device: Option<UsbDevice> = None;
    let mut pending_urbs: HashMap<u32, PendingUrb> = HashMap::new();
    // bounded channel: a flooding completion stream cannot grow without
    // backpressure; senders retry after a short yield instead of unbounded
    // memory growth.

    loop {
        tokio::select! {
            completed = completed_rx.recv(), if !pending_urbs.is_empty() => {
                let Some(completed): Option<CompletedSubmit> = completed else {
                    return Err(std::io::Error::new(ErrorKind::BrokenPipe, "URB worker channel closed"));
                };
                let Some(pending) = pending_urbs.remove(&completed.seqnum) else {
                    continue;
                };
                if let Some(unlink_header) = pending.unlink_header {
                    UsbIpResponse::usbip_ret_unlink_result(&unlink_header, completed.status)
                        .write_to_socket(writer)
                        .await?;
                    trace!("Sent USBIP_RET_UNLINK for {:x}", completed.seqnum);
                } else {
                    completed.response.write_to_socket(writer).await?;
                    trace!("Sent USBIP_RET_SUBMIT for {:x}", completed.seqnum);
                }
                continue;
            }
                    command = UsbIpCommand::read_from_socket_timeout(reader, Some(COMMAND_TIMEOUT)) => {
                let command = match command {
                    Ok(command) => command,
                    Err(err) => {
                        for pending in pending_urbs.values() {
                            pending.cancellation.cancel();
                        }
                        // A TCP half-close means no more commands are coming,
                        // but the peer may still be waiting for replies. Drain
                        // cancelled/completed URBs before releasing the device —
                        // bounded by a hard teardown deadline so a wedged
                        // host-side cancellation cannot pin the device (or this
                        // handler) forever; late completions are dropped.
                        // Device release itself happens in the unified
                        // handler_with_options cleanup on every exit path.
                        let drain = async {
                            while !pending_urbs.is_empty() {
                                let Some(completed): Option<CompletedSubmit> = completed_rx.recv().await else {
                                    break;
                                };
                                let Some(pending) = pending_urbs.remove(&completed.seqnum) else {
                                    continue;
                                };
                                let response = if let Some(unlink_header) = pending.unlink_header {
                                    UsbIpResponse::usbip_ret_unlink_result(
                                        &unlink_header,
                                        completed.status,
                                    )
                                } else {
                                    completed.response
                                };
                                if response.write_to_socket(writer).await.is_err() {
                                    break;
                                }
                            }
                        };
                        if tokio::time::timeout(TEARDOWN_TIMEOUT, drain).await.is_err() {
                            warn!(
                                "URB teardown exceeded {TEARDOWN_TIMEOUT:?}; forcing release ({} URBs dropped)",
                                pending_urbs.len()
                            );
                        }
                        if err.kind() == ErrorKind::UnexpectedEof {
                            info!("Remote closed the connection");
                            return Ok(());
                        }
                        return Err(err);
                    }
                };

                match command {
                    UsbIpCommand::OpReqDevlist { .. } => {
                        trace!("Got OP_REQ_DEVLIST");
                        let devices = server.available_devices.read().await;
                        UsbIpResponse::op_rep_devlist(&devices)
                            .write_to_socket(writer)
                            .await?;
                        trace!("Sent OP_REP_DEVLIST");
                    }
                    UsbIpCommand::OpReqImport { busid, .. } => {
                        trace!("Got OP_REQ_IMPORT");
                        current_import_device = None;

                        let mut used_devices = server.used_devices.write().await;
                        let mut available_devices = server.available_devices.write().await;
                        // Release a device previously imported by THIS
                        // connection before switching: a second OP_REQ_IMPORT
                        // must not leak the first device into used_devices
                        // forever (unified cleanup only tracks the latest).
                        if let Some(prev_id) = imported_device_id.take()
                            && let Some(dev) = used_devices.remove(&prev_id)
                        {
                            available_devices.push(dev);
                        }
                        let busid_compare =
                            &busid[..busid.iter().position(|&x| x == 0).unwrap_or(busid.len())];
                        if let Some(index) = available_devices
                            .iter()
                            .position(|device| busid_compare == device.bus_id.as_bytes())
                        {
                            let device = available_devices.remove(index);
                            let dev_id = device.bus_id.clone();
                            current_import_device = Some(device.clone());
                            *imported_device_id = Some(dev_id.clone());
                            used_devices.insert(dev_id, device);
                        }

                        let response = current_import_device
                            .as_ref()
                            .map(UsbIpResponse::op_rep_import_success)
                            .unwrap_or_else(UsbIpResponse::op_rep_import_fail);
                        response.write_to_socket(writer).await?;
                        trace!("Sent OP_REP_IMPORT");
                    }
                    command @ UsbIpCommand::UsbIpCmdSubmit { .. } => {
                        let Some(device) = current_import_device.clone() else {
                            return Err(std::io::Error::new(ErrorKind::NotConnected, "no imported device"));
                        };
                        let seqnum = match &command {
                            UsbIpCommand::UsbIpCmdSubmit { header, .. } => header.seqnum,
                            _ => unreachable!(),
                        };
                        if pending_urbs.contains_key(&seqnum) {
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                format!("duplicate USB/IP sequence number {seqnum}"),
                            ));
                        }
                        // Protocol invariant: devid must reference the
                        // currently imported device ((bus << 16) | devnum).
                        // A mismatched devid means a hostile/broken client.
                        let expected_devid =
                            (device.bus_num << 16) | device.dev_num;
                        let header_devid = match &command {
                            UsbIpCommand::UsbIpCmdSubmit { header, .. } => header.devid,
                            _ => unreachable!(),
                        };
                        if header_devid != expected_devid {
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                format!(
                                    "devid {header_devid:#x} does not match imported device {expected_devid:#x}"
                                ),
                            ));
                        }
                        if pending_urbs.len() >= MAX_PENDING_URBS {
                            return Err(std::io::Error::new(
                                ErrorKind::OutOfMemory,
                                format!("too many pending URBs (limit {MAX_PENDING_URBS})"),
                            ));
                        }

                        let cancellation = Arc::new(async_transfer::TransferCancellation::default());
                        pending_urbs.insert(
                            seqnum,
                            PendingUrb {
                                cancellation: cancellation.clone(),
                                unlink_header: None,
                            },
                        );
                        let sender = completed_tx.clone();
                        tokio::spawn(async move {
                            let completed = process_submit(device, command, cancellation).await;
                            // bounded channel: retry on full instead of
                            // panicking or growing memory without bound.
                            let mut backoff = 1;
                            loop {
                                match sender.try_send(completed.clone()) {
                                    Ok(()) => break,
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                                        backoff = (backoff * 2).min(50);
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                                }
                            }
                        });
                    }
                    UsbIpCommand::UsbIpCmdUnlink {
                        mut header,
                        unlink_seqnum,
                    } => {
                        trace!("Got USBIP_CMD_UNLINK for {unlink_seqnum:10x?}");
                        header.command = USBIP_RET_UNLINK.into();
                        if let Some(pending) = pending_urbs.get_mut(&unlink_seqnum) {
                            if pending.unlink_header.is_none() {
                                pending.unlink_header = Some(header);
                                pending.cancellation.cancel();
                            } else {
                                UsbIpResponse::usbip_ret_unlink_result(&header, 0)
                                    .write_to_socket(writer)
                                    .await?;
                            }
                        } else {
                            UsbIpResponse::usbip_ret_unlink_result(&header, 0)
                                .write_to_socket(writer)
                                .await?;
                        }
                    }
                }
            }
        }
    }
}

/// Spawn a USB/IP server at `addr` using [TcpListener]
pub async fn server(addr: SocketAddr, server: Arc<UsbIpServer>) {
    try_server(addr, server).await.expect("bind to addr");
}

/// Try to spawn a USB/IP server at `addr` using [TcpListener].
///
/// This variant returns bind errors to the caller so command-line frontends
/// can report an occupied port without panicking.
pub async fn try_server(addr: SocketAddr, server: Arc<UsbIpServer>) -> Result<()> {
    try_server_with_options(addr, server, ServerOptions::default()).await
}

/// Try to spawn a USB/IP server with an accept policy (client allowlist) and
/// resource limits (connection cap, per-connection idle timeout).
pub async fn try_server_with_options(
    addr: SocketAddr,
    server: Arc<UsbIpServer>,
    options: ServerOptions,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    // Cap concurrent connections so idle peers cannot exhaust tasks/memory.
    let connection_semaphores = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let options = Arc::new(options);

    let accept_loop = async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, peer_addr)) => {
                    if !options.client_allowed(peer_addr) {
                        warn!("Rejected connection from non-allowlisted peer {peer_addr}");
                        continue;
                    }
                    let permit = match connection_semaphores.clone().acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    info!("Got connection from {:?}", peer_addr);
                    let new_server = server.clone();
                    let new_options = options.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let res = handler_with_options(&mut socket, new_server, &new_options).await;
                        info!("Handler ended with {res:?}");
                    });
                }
                Err(err) => {
                    warn!("Got error {err:?}");
                }
            }
        }
    };

    accept_loop.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::{net::TcpStream, task::JoinSet};

    use super::*;
    use crate::{
        usbip_protocol::{USBIP_CMD_SUBMIT, UsbIpHeaderBasic},
        util::tests::*,
    };

    const SINGLE_DEVICE_BUSID: &str = "0-0-0";

    fn new_server_with_single_device() -> UsbIpServer {
        UsbIpServer::new_simulated(vec![UsbDevice::new(0).with_interface(
            ClassCode::CDC as u8,
            cdc::CDC_ACM_SUBCLASS,
            0x00,
            Some("Test CDC ACM"),
            cdc::UsbCdcAcmHandler::endpoints(),
            Arc::new(Mutex::new(
                Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
            )),
        )])
    }

    fn op_req_import(busid: &str) -> Vec<u8> {
        let mut busid = busid.to_string().as_bytes().to_vec();
        busid.resize(32, 0);
        UsbIpCommand::OpReqImport {
            status: 0,
            busid: busid.try_into().unwrap(),
        }
        .to_bytes()
    }

    async fn attach_device(connection: &mut TcpStream, busid: &str) -> u32 {
        let req = op_req_import(busid);
        connection.write_all(req.as_slice()).await.unwrap();
        connection.read_u32().await.unwrap();
        let result = connection.read_u32().await.unwrap();
        if result == 0 {
            connection.read_exact(&mut vec![0; 0x138]).await.unwrap();
        }
        result
    }

    #[tokio::test]
    async fn req_empty_devlist() {
        setup_test_logger();
        let server = UsbIpServer::new_simulated(vec![]);
        let req = UsbIpCommand::OpReqDevlist { status: 0 };

        let mut mock_socket = MockSocket::new(req.to_bytes());
        handler(&mut mock_socket, Arc::new(server)).await.ok();

        assert_eq!(
            mock_socket.output,
            UsbIpResponse::op_rep_devlist(&[]).to_bytes(),
        );
    }

    #[tokio::test]
    async fn req_sample_devlist() {
        setup_test_logger();
        let server = new_server_with_single_device();
        let req = UsbIpCommand::OpReqDevlist { status: 0 };

        let mut mock_socket = MockSocket::new(req.to_bytes());
        handler(&mut mock_socket, Arc::new(server)).await.ok();

        // OP_REP_DEVLIST
        // header: 0xC
        // device: 0x138
        // interface: 4 * 0x1
        assert_eq!(mock_socket.output.len(), 0xC + 0x138 + 4);
    }

    #[tokio::test]
    async fn req_import() {
        setup_test_logger();
        let server = new_server_with_single_device();

        // OP_REQ_IMPORT
        let req = op_req_import(SINGLE_DEVICE_BUSID);
        let mut mock_socket = MockSocket::new(req);
        handler(&mut mock_socket, Arc::new(server)).await.ok();
        // OP_REQ_IMPORT
        assert_eq!(mock_socket.output.len(), 0x140);
    }

    #[tokio::test]
    async fn add_and_remove_10_devices() {
        setup_test_logger();
        let server_ = Arc::new(UsbIpServer::new_simulated(vec![]));
        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let mut join_set = JoinSet::new();
        let devices = (0..10).map(UsbDevice::new).collect::<Vec<_>>();

        for device in devices.iter() {
            let new_server = server_.clone();
            let new_device = device.clone();
            join_set.spawn(async move {
                new_server.add_device(new_device).await;
            });
        }

        for device in devices.iter() {
            let new_server = server_.clone();
            let new_device = device.clone();
            join_set.spawn(async move {
                new_server.remove_device(&new_device.bus_id).await.unwrap();
            });
        }

        while join_set.join_next().await.is_some() {}

        let device_len = server_.clone().available_devices.read().await.len();

        assert_eq!(device_len, 0);
    }

    #[tokio::test]
    async fn send_usb_traffic_while_adding_and_removing_devices() {
        setup_test_logger();
        let server_ = Arc::new(new_server_with_single_device());

        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let cmd_loop_handle = tokio::spawn(async move {
            let mut connection = poll_connect(addr).await;
            let result = attach_device(&mut connection, SINGLE_DEVICE_BUSID).await;
            assert_eq!(result, 0);

            let cdc_loopback_bulk_cmd = UsbIpCommand::UsbIpCmdSubmit {
                header: usbip_protocol::UsbIpHeaderBasic {
                    command: USBIP_CMD_SUBMIT.into(),
                    seqnum: 1,
                    devid: 0,
                    direction: 0, // OUT
                    ep: 2,
                },
                transfer_flags: 0,
                transfer_buffer_length: 8,
                start_frame: 0,
                number_of_packets: 0,
                interval: 0,
                setup: [0; 8],
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
                iso_packet_descriptor: vec![],
            };

            loop {
                connection
                    .write_all(cdc_loopback_bulk_cmd.to_bytes().as_slice())
                    .await
                    .unwrap();
                let mut result = vec![0; 4 * 12];
                connection.read_exact(&mut result).await.unwrap();
            }
        });

        let add_and_remove_device_handle = tokio::spawn(async move {
            let mut join_set = JoinSet::new();
            let devices = (1..4).map(UsbDevice::new).collect::<Vec<_>>();

            loop {
                for device in devices.iter() {
                    let new_server = server_.clone();
                    let new_device = device.clone();
                    join_set.spawn(async move {
                        new_server.add_device(new_device).await;
                    });
                }

                for device in devices.iter() {
                    let new_server = server_.clone();
                    let new_device = device.clone();
                    join_set.spawn(async move {
                        new_server.remove_device(&new_device.bus_id).await.unwrap();
                    });
                }
                while join_set.join_next().await.is_some() {}
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            }
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        cmd_loop_handle.abort();
        add_and_remove_device_handle.abort();
    }

    #[tokio::test]
    async fn only_single_connection_allowed_to_device() {
        setup_test_logger();
        let server_ = Arc::new(new_server_with_single_device());

        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let mut first_connection = poll_connect(addr).await;
        let mut second_connection = TcpStream::connect(addr).await.unwrap();

        let result = attach_device(&mut first_connection, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 0);

        let result = attach_device(&mut second_connection, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 1);
    }

    #[tokio::test]
    async fn device_gets_released_on_closed_socket() {
        setup_test_logger();
        let server_ = Arc::new(new_server_with_single_device());

        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let mut connection = poll_connect(addr).await;
        let result = attach_device(&mut connection, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 0);

        std::mem::drop(connection);

        let mut connection = TcpStream::connect(addr).await.unwrap();
        let result = attach_device(&mut connection, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn req_import_get_device_desc() {
        setup_test_logger();
        let server = new_server_with_single_device();

        let mut req = op_req_import(SINGLE_DEVICE_BUSID);
        req.extend(
            UsbIpCommand::UsbIpCmdSubmit {
                header: UsbIpHeaderBasic {
                    command: USBIP_CMD_SUBMIT.into(),
                    seqnum: 1,
                    devid: 0,
                    direction: 1, // IN
                    ep: 0,
                },
                transfer_flags: 0,
                transfer_buffer_length: 0,
                start_frame: 0,
                number_of_packets: 0,
                interval: 0,
                // GetDescriptor to Device
                setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00],
                data: vec![],
                iso_packet_descriptor: vec![],
            }
            .to_bytes(),
        );

        let mut mock_socket = MockSocket::new(req);
        handler(&mut mock_socket, Arc::new(server)).await.ok();
        // OP_REQ_IMPORT + USBIP_CMD_SUBMIT + Device Descriptor
        assert_eq!(mock_socket.output.len(), 0x140 + 0x30 + 0x12);
    }

    #[tokio::test]
    async fn device_gets_released_after_protocol_error() {
        setup_test_logger();
        let server_ = Arc::new(new_server_with_single_device());

        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let mut connection = poll_connect(addr).await;
        let result = attach_device(&mut connection, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 0);

        // Submit with a mismatched devid: handler must return an error, and
        // the unified cleanup must still release the imported device.
        let bad_submit = UsbIpCommand::UsbIpCmdSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_CMD_SUBMIT.into(),
                seqnum: 1,
                devid: 0xffff,
                direction: 1, // IN
                ep: 0,
            },
            transfer_flags: 0,
            transfer_buffer_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
            data: vec![],
            iso_packet_descriptor: vec![],
        };
        connection
            .write_all(bad_submit.to_bytes().as_slice())
            .await
            .unwrap();
        let _ = connection.read_u32().await;

        // The device must be back in the available pool despite the error.
        let mut next = TcpStream::connect(addr).await.unwrap();
        let result = attach_device(&mut next, SINGLE_DEVICE_BUSID).await;
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn second_import_releases_previous_device() {
        setup_test_logger();
        let first_device = UsbDevice::new(0).with_interface(
            ClassCode::CDC as u8,
            cdc::CDC_ACM_SUBCLASS,
            0x00,
            Some("Test CDC ACM"),
            cdc::UsbCdcAcmHandler::endpoints(),
            Arc::new(Mutex::new(
                Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
            )),
        );
        let mut second_device = UsbDevice::new(1).with_interface(
            ClassCode::CDC as u8,
            cdc::CDC_ACM_SUBCLASS,
            0x00,
            Some("Test CDC ACM 2"),
            cdc::UsbCdcAcmHandler::endpoints(),
            Arc::new(Mutex::new(
                Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
            )),
        );
        second_device.bus_id = "0-0-1".to_string();
        let server_ = Arc::new(UsbIpServer::new_simulated(vec![
            first_device,
            second_device,
        ]));

        let addr = get_free_address().await;
        tokio::spawn(server(addr, server_.clone()));

        let mut connection = poll_connect(addr).await;
        assert_eq!(attach_device(&mut connection, "0-0-0").await, 0);
        // Importing a second device on the SAME connection must not leak the
        // first one into used_devices.
        assert_eq!(attach_device(&mut connection, "0-0-1").await, 0);
        std::mem::drop(connection);

        // Both devices must be importable again by a fresh connection.
        let mut next = TcpStream::connect(addr).await.unwrap();
        assert_eq!(attach_device(&mut next, "0-0-0").await, 0);
        std::mem::drop(next);
        let mut next = TcpStream::connect(addr).await.unwrap();
        assert_eq!(attach_device(&mut next, "0-0-1").await, 0);
    }

    #[test]
    fn client_allowlist_is_fail_closed() {
        use std::net::Ipv4Addr;

        let peer = |ip: [u8; 4], port: u16| SocketAddr::from((Ipv4Addr::from(ip), port));

        // Empty allowlist without the explicit override rejects non-loopback.
        let options = ServerOptions::default();
        assert!(!options.client_allowed(peer([192, 168, 1, 50], 3240)));
        // Loopback is always trusted.
        assert!(options.client_allowed(peer([127, 0, 0, 1], 3240)));
        // Allowlisted peer passes, others are rejected.
        let options = ServerOptions::new().allow_client(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        assert!(options.client_allowed(peer([192, 168, 1, 50], 9999)));
        assert!(!options.client_allowed(peer([192, 168, 1, 51], 3240)));
        // Explicit override accepts any non-loopback address too.
        let options = ServerOptions::new().with_allow_any_client(true);
        assert!(options.client_allowed(peer([10, 0, 0, 7], 3240)));
    }
}
