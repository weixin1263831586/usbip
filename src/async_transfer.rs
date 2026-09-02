//! Cancellable asynchronous transfers for physical devices backed by libusb.
//!
//! `rusb` intentionally exposes only the synchronous transfer helpers. USB/IP
//! cannot use those helpers because the client routinely keeps several URBs in
//! flight and may unlink any one of them. This module uses the libusb API
//! re-exported by `rusb` and owns every raw transfer until its completion
//! callback runs.

use crate::{EndpointAttributes, SetupPacket, UsbEndpoint};
use rusb::{DeviceHandle, GlobalContext, UsbContext, ffi};
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;

pub(crate) const ECONNRESET: i32 = 104;
pub(crate) const ENODEV_ERRNO: i32 = -19;
const EIO: i32 = 5;
const ENOENT: i32 = 2;
const EPIPE: i32 = 32;
const EOVERFLOW: i32 = 75;
const ETIMEDOUT: i32 = 110;
const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
const EBUSY: i32 = 16;

const USBIP_URB_SHORT_NOT_OK: u32 = 0x0001;
const USBIP_URB_ZERO_PACKET: u32 = 0x0040;

#[derive(Debug)]
pub(crate) struct TransferCompletion {
    pub status: i32,
    pub actual_length: u32,
    pub data: Vec<u8>,
}

/// Stable identity and replaceable libusb handle for a physical USB device.
/// Linux assigns a new device address after every re-enumeration; USB/IP's
/// exported identity must instead follow the serial number.
#[derive(Debug)]
pub(crate) struct HostDeviceRuntime {
    current: Mutex<Option<Arc<DeviceHandle<GlobalContext>>>>,
    reconnect_lock: Mutex<()>,
    vendor_id: u16,
    product_id: u16,
    serial: String,
    interfaces: Vec<u8>,
}

impl HostDeviceRuntime {
    pub(crate) fn new(
        handle: Arc<DeviceHandle<GlobalContext>>,
        vendor_id: u16,
        product_id: u16,
        serial: String,
        interfaces: Vec<u8>,
    ) -> Self {
        Self {
            current: Mutex::new(Some(handle)),
            reconnect_lock: Mutex::new(()),
            vendor_id,
            product_id,
            serial,
            interfaces,
        }
    }

    pub(crate) fn current(&self) -> Option<Arc<DeviceHandle<GlobalContext>>> {
        self.current.lock().unwrap().clone()
    }

    /// Find and claim the same physical gadget after it re-enumerates.
    pub(crate) fn reconnect(&self) -> Result<()> {
        let _reconnect_guard = self.reconnect_lock.lock().unwrap();
        let devices = rusb::devices().map_err(Error::other)?;
        for device in devices.iter() {
            let descriptor = device.device_descriptor().map_err(Error::other)?;
            if descriptor.vendor_id() != self.vendor_id {
                continue;
            }
            // Android gadgets may change PID when switching from ADB to
            // fastboot. A non-empty serial is the stable identity; devices
            // without a serial must retain the original VID/PID match.
            if self.serial.is_empty() && descriptor.product_id() != self.product_id {
                continue;
            }
            let handle = match device.open() {
                Ok(handle) => Arc::new(handle),
                Err(_) => continue,
            };
            let actual_serial = match handle.read_serial_number_string_ascii(&descriptor) {
                Ok(serial) => serial,
                Err(_) => continue,
            };
            if actual_serial != self.serial {
                continue;
            }
            handle.set_auto_detach_kernel_driver(true).ok();
            let mut claimed = true;
            for interface in &self.interfaces {
                if handle.claim_interface(*interface).is_err() {
                    claimed = false;
                    break;
                }
            }
            if claimed {
                *self.current.lock().unwrap() = Some(handle);
                log::info!(
                    "reconnected USB device {:04x}:{:04x} serial {}",
                    self.vendor_id,
                    self.product_id,
                    self.serial
                );
                return Ok(());
            }
        }
        Err(Error::new(
            ErrorKind::NotFound,
            "physical USB device not found",
        ))
    }
}

/// A cancellation handle shared between the USB/IP connection and libusb.
///
/// The pointer is protected by a mutex so the completion callback cannot free
/// the transfer while another thread is inside `libusb_cancel_transfer`.
#[derive(Debug, Default)]
pub(crate) struct TransferCancellation {
    inner: Mutex<CancellationInner>,
}

#[derive(Debug, Default)]
struct CancellationInner {
    transfer: Option<RawTransfer>,
    submitted: bool,
    cancel_requested: bool,
}

#[derive(Clone, Copy, Debug)]
struct RawTransfer(*mut ffi::libusb_transfer);

// libusb transfers may be submitted and cancelled from different threads. The
// mutex in TransferCancellation serializes pointer access with the callback.
unsafe impl Send for RawTransfer {}
unsafe impl Sync for RawTransfer {}

impl TransferCancellation {
    pub(crate) fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.cancel_requested = true;
        if inner.submitted
            && let Some(transfer) = inner.transfer
        {
            // SAFETY: `transfer` remains allocated while the mutex is held; the
            // callback takes the same mutex before clearing and freeing it.
            let result = unsafe { ffi::libusb_cancel_transfer(transfer.0) };
            if result != 0 && result != ffi::constants::LIBUSB_ERROR_NOT_FOUND {
                log::warn!("libusb_cancel_transfer failed: {result}");
            }
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.lock().unwrap().cancel_requested
    }

    fn attach(&self, transfer: *mut ffi::libusb_transfer) {
        let mut inner = self.inner.lock().unwrap();
        inner.transfer = Some(RawTransfer(transfer));
        inner.submitted = false;
    }

    fn mark_submitted(&self, transfer: *mut ffi::libusb_transfer) {
        let mut inner = self.inner.lock().unwrap();
        if inner.transfer != Some(RawTransfer(transfer)) {
            return;
        }
        inner.submitted = true;
        if inner.cancel_requested {
            // SAFETY: the transfer was allocated and has not completed yet.
            unsafe { ffi::libusb_cancel_transfer(transfer) };
        }
    }

    fn detach(&self, transfer: *mut ffi::libusb_transfer) {
        let mut inner = self.inner.lock().unwrap();
        if inner.transfer == Some(RawTransfer(transfer)) {
            inner.transfer = None;
            inner.submitted = false;
        }
    }
}

impl PartialEq for RawTransfer {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

struct TransferState {
    transfer: *mut ffi::libusb_transfer,
    buffer: Vec<u8>,
    data_offset: usize,
    direction_in: bool,
    cancellation: Arc<TransferCancellation>,
    sender: Option<oneshot::Sender<TransferCompletion>>,
    // Keep the device handle alive until libusb invokes the callback.
    _device_handle: Arc<DeviceHandle<GlobalContext>>,
}

unsafe impl Send for TransferState {}

extern "system" fn transfer_callback(transfer: *mut ffi::libusb_transfer) {
    // SAFETY: submit_transfer stores exactly one Box<TransferState> in
    // user_data and libusb invokes this callback at most once per submission.
    let mut state = unsafe { Box::from_raw((*transfer).user_data.cast::<TransferState>()) };
    state.cancellation.detach(transfer);

    // SAFETY: libusb owns `transfer` until this callback and all fields are
    // valid for the duration of the callback.
    let (transfer_status, actual_length) = unsafe {
        (
            (*transfer).status,
            (*transfer).actual_length.max(0) as usize,
        )
    };
    let status = transfer_status_to_errno(transfer_status);
    let actual_length = actual_length.min(state.buffer.len().saturating_sub(state.data_offset));
    let data = if state.direction_in {
        state.buffer[state.data_offset..state.data_offset + actual_length].to_vec()
    } else {
        Vec::new()
    };

    if let Some(sender) = state.sender.take() {
        let _ = sender.send(TransferCompletion {
            status,
            actual_length: actual_length as u32,
            data,
        });
    }

    // No libusb-owned buffer flag is used, so dropping `state` frees the Vec.
    // SAFETY: the transfer is no longer pending once its callback is running.
    unsafe { ffi::libusb_free_transfer(state.transfer) };
}

fn ensure_event_thread() {
    static EVENT_THREAD: OnceLock<()> = OnceLock::new();
    EVENT_THREAD.get_or_init(|| {
        std::thread::Builder::new()
            .name("usbip-libusb-events".to_string())
            .spawn(|| {
                loop {
                    if let Err(err) =
                        GlobalContext::default().handle_events(Some(Duration::from_millis(250)))
                    {
                        log::warn!("libusb event handling failed: {err}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn libusb event thread");
    });
}

pub(crate) async fn submit_transfer(
    handle: Arc<DeviceHandle<GlobalContext>>,
    endpoint: UsbEndpoint,
    transfer_flags: u32,
    transfer_buffer_length: u32,
    setup: SetupPacket,
    data: Vec<u8>,
    cancellation: Arc<TransferCancellation>,
) -> Result<TransferCompletion> {
    ensure_event_thread();

    let direction_in = endpoint.address & 0x80 != 0;
    let is_control = endpoint.attributes == EndpointAttributes::Control as u8;
    let data_length = if direction_in {
        transfer_buffer_length as usize
    } else {
        data.len()
    };
    let data_offset = if is_control {
        ffi::constants::LIBUSB_CONTROL_SETUP_SIZE
    } else {
        0
    };
    let mut buffer = vec![0u8; data_offset + data_length];
    if is_control {
        // SAFETY: the buffer has room for the eight-byte libusb setup header.
        unsafe {
            ffi::libusb_fill_control_setup(
                buffer.as_mut_ptr(),
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                data_length as u16,
            )
        };
    }
    if !direction_in && !data.is_empty() {
        buffer[data_offset..].copy_from_slice(&data);
    }

    // SAFETY: zero ISO packets are requested because this function currently
    // accepts control, bulk and interrupt endpoints only.
    let transfer = unsafe { ffi::libusb_alloc_transfer(0) };
    if transfer.is_null() {
        return Err(Error::new(ErrorKind::OutOfMemory, "libusb_alloc_transfer"));
    }

    let (sender, receiver) = oneshot::channel();
    let state = Box::new(TransferState {
        transfer,
        buffer,
        data_offset,
        direction_in,
        cancellation: cancellation.clone(),
        sender: Some(sender),
        _device_handle: handle.clone(),
    });
    let state_ptr = Box::into_raw(state);

    // SAFETY: state_ptr remains owned by the callback, and its buffer cannot
    // move before completion. A timeout of zero means no libusb-side timeout;
    // USB/IP CMD_UNLINK supplies cancellation.
    unsafe {
        let state = &mut *state_ptr;
        let callback_data = state_ptr.cast::<c_void>();
        match endpoint.attributes {
            value if value == EndpointAttributes::Control as u8 => {
                ffi::libusb_fill_control_transfer(
                    transfer,
                    handle.as_raw(),
                    state.buffer.as_mut_ptr(),
                    transfer_callback,
                    callback_data,
                    0,
                );
            }
            value if value == EndpointAttributes::Bulk as u8 => {
                ffi::libusb_fill_bulk_transfer(
                    transfer,
                    handle.as_raw(),
                    endpoint.address,
                    state.buffer.as_mut_ptr(),
                    state.buffer.len() as i32,
                    transfer_callback,
                    callback_data,
                    0,
                );
            }
            value if value == EndpointAttributes::Interrupt as u8 => {
                ffi::libusb_fill_interrupt_transfer(
                    transfer,
                    handle.as_raw(),
                    endpoint.address,
                    state.buffer.as_mut_ptr(),
                    state.buffer.len() as i32,
                    transfer_callback,
                    callback_data,
                    0,
                );
            }
            _ => {
                drop(Box::from_raw(state_ptr));
                ffi::libusb_free_transfer(transfer);
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "isochronous transfers are not implemented",
                ));
            }
        }

        if transfer_flags & USBIP_URB_SHORT_NOT_OK != 0 {
            (*transfer).flags |= ffi::constants::LIBUSB_TRANSFER_SHORT_NOT_OK;
        }
        if transfer_flags & USBIP_URB_ZERO_PACKET != 0 && !direction_in {
            (*transfer).flags |= ffi::constants::LIBUSB_TRANSFER_ADD_ZERO_PACKET;
        }

        cancellation.attach(transfer);
        let result = ffi::libusb_submit_transfer(transfer);
        if result != 0 {
            cancellation.detach(transfer);
            drop(Box::from_raw(state_ptr));
            ffi::libusb_free_transfer(transfer);
            return Ok(TransferCompletion {
                status: libusb_error_to_errno(result),
                actual_length: 0,
                data: Vec::new(),
            });
        }
        cancellation.mark_submitted(transfer);
    }

    receiver
        .await
        .map_err(|_| Error::new(ErrorKind::BrokenPipe, "libusb callback dropped"))
}

fn transfer_status_to_errno(status: i32) -> i32 {
    match status {
        ffi::constants::LIBUSB_TRANSFER_COMPLETED => 0,
        ffi::constants::LIBUSB_TRANSFER_TIMED_OUT => -ETIMEDOUT,
        ffi::constants::LIBUSB_TRANSFER_CANCELLED => -ECONNRESET,
        ffi::constants::LIBUSB_TRANSFER_STALL => -EPIPE,
        ffi::constants::LIBUSB_TRANSFER_NO_DEVICE => ENODEV_ERRNO,
        ffi::constants::LIBUSB_TRANSFER_OVERFLOW => -EOVERFLOW,
        _ => -EIO,
    }
}

fn libusb_error_to_errno(status: i32) -> i32 {
    match status {
        ffi::constants::LIBUSB_ERROR_IO => -EIO,
        ffi::constants::LIBUSB_ERROR_INVALID_PARAM => -EINVAL,
        ffi::constants::LIBUSB_ERROR_ACCESS => -EACCES,
        ffi::constants::LIBUSB_ERROR_NO_DEVICE => ENODEV_ERRNO,
        ffi::constants::LIBUSB_ERROR_NOT_FOUND => -ENOENT,
        ffi::constants::LIBUSB_ERROR_BUSY => -EBUSY,
        ffi::constants::LIBUSB_ERROR_TIMEOUT => -ETIMEDOUT,
        ffi::constants::LIBUSB_ERROR_OVERFLOW => -EOVERFLOW,
        ffi::constants::LIBUSB_ERROR_PIPE => -EPIPE,
        ffi::constants::LIBUSB_ERROR_INTERRUPTED => -EINTR,
        ffi::constants::LIBUSB_ERROR_NO_MEM => -ENOMEM,
        _ => -EIO,
    }
}

const EACCES: i32 = 13;
const EINTR: i32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_libusb_transfer_statuses_to_linux_errno() {
        assert_eq!(
            transfer_status_to_errno(ffi::constants::LIBUSB_TRANSFER_COMPLETED),
            0
        );
        assert_eq!(
            transfer_status_to_errno(ffi::constants::LIBUSB_TRANSFER_CANCELLED),
            -ECONNRESET
        );
        assert_eq!(
            transfer_status_to_errno(ffi::constants::LIBUSB_TRANSFER_STALL),
            -EPIPE
        );
        assert_eq!(
            transfer_status_to_errno(ffi::constants::LIBUSB_TRANSFER_NO_DEVICE),
            ENODEV_ERRNO
        );
    }
}
