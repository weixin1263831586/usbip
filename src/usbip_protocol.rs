//! USB/IP protocol structs
//!
//! This module contains declarations of all structs used in the USB/IP protocol,
//! as well as functions to serialize and deserialize them to/from byte arrays,
//! and functions to send and receive them over a socket.
//!
//! They are based on the [Linux kernel documentation](https://docs.kernel.org/usb/usbip_protocol.html).

use log::trace;
use std::io::{ErrorKind, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum accepted ``transfer_buffer_length`` for a single URB.
///
/// The value comes straight off the network, so it must be bounded before any
/// allocation; otherwise a malicious client can request multi-gigabyte
/// ``Vec`` allocations and OOM the server.
pub const MAX_TRANSFER_BUFFER_LENGTH: u32 = 16 * 1024 * 1024;

/// Maximum accepted ``number_of_packets`` for ISO transfers.
pub const MAX_NUMBER_OF_PACKETS: u32 = 4096;

/// Size in bytes of each ISO packet descriptor on the wire.
const ISO_PACKET_DESCRIPTOR_SIZE: usize = 16;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::UsbDevice;

/// USB/IP protocol version
///
/// This is currently the only supported version of USB/IP
/// for this library.
pub const USBIP_VERSION: u16 = 0x0111;

/// Command code: Retrieve the list of exported USB devices
pub const OP_REQ_DEVLIST: u16 = 0x8005;
/// Command code: import a remote USB device
pub const OP_REQ_IMPORT: u16 = 0x8003;
/// Reply code: The list of exported USB devices
pub const OP_REP_DEVLIST: u16 = 0x0005;
/// Reply code: Reply to import
pub const OP_REP_IMPORT: u16 = 0x0003;

/// Command code: Submit an URB
pub const USBIP_CMD_SUBMIT: u16 = 0x0001;
/// Command code: Unlink an URB
pub const USBIP_CMD_UNLINK: u16 = 0x0002;
/// Reply code: Reply for submitting an URB
pub const USBIP_RET_SUBMIT: u16 = 0x0003;
/// Reply code: Reply for URB unlink
pub const USBIP_RET_UNLINK: u16 = 0x0004;

/// USB/IP direction
///
/// NOTE: Must not be confused with rusb::Direction,
/// which has the opposite enum values. This is only for
/// internal use in the USB/IP protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Out = 0,
    In = 1,
}

/// Common header for all context sensitive packets
///
/// All commands/responses which rely on a device being attached
/// to a client use this header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UsbIpHeaderBasic {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl UsbIpHeaderBasic {
    /// Converts a byte array into a [UsbIpHeaderBasic].
    pub fn from_bytes(bytes: &[u8; 20]) -> Self {
        let result = UsbIpHeaderBasic {
            command: u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
            seqnum: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            devid: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            direction: u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
            ep: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        };
        // The direction should be 0 or 1
        debug_assert!(result.direction & 1 == result.direction);
        result
    }

    /// Returns the validated direction flag (0=OUT, 1=IN) or an error.
    ///
    /// Struct literal construction cannot fail, so call sites that receive
    /// network-controlled headers must call this instead of relying on
    /// ``debug_assert!`` only.
    pub fn validated_direction(&self) -> Result<u32> {
        if self.direction & 1 != self.direction {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid direction: {}", self.direction),
            ));
        }
        Ok(self.direction)
    }

    /// Validate protocol-invariant fields that come off the network.
    ///
    /// Release builds must reject malformed input too, not only debug builds
    /// (previously these were only ``debug_assert!``s).
    fn validate(&self) -> Result<()> {
        if self.direction & 1 != self.direction {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid direction: {}", self.direction),
            ));
        }
        // Endpoint number is a 4-bit field in USB; the network value is
        // truncated to u8 further down the stack, so reject anything wider.
        if self.ep > 15 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid endpoint number: {}", self.ep),
            ));
        }
        Ok(())
    }

    /// Converts the [UsbIpHeaderBasic] into a byte array.
    pub fn to_bytes(&self) -> [u8; 20] {
        let mut result = [0u8; 20];
        result[0..4].copy_from_slice(&self.command.to_be_bytes());
        result[4..8].copy_from_slice(&self.seqnum.to_be_bytes());
        result[8..12].copy_from_slice(&self.devid.to_be_bytes());
        result[12..16].copy_from_slice(&self.direction.to_be_bytes());
        result[16..20].copy_from_slice(&self.ep.to_be_bytes());
        result
    }

    pub(crate) async fn read_from_socket_with_command<T: AsyncReadExt + Unpin>(
        socket: &mut T,
        command: u16,
    ) -> Result<Self> {
        let seqnum = socket.read_u32().await?;
        let devid = socket.read_u32().await?;
        let direction = socket.read_u32().await?;
        // The direction should be 0 or 1; enforced below via validate() so
        // release builds reject malformed input too (was debug_assert only).
        let ep = socket.read_u32().await?;

        let header = UsbIpHeaderBasic {
            command: command.into(),
            seqnum,
            devid,
            direction,
            ep,
        };
        header.validate()?;
        Ok(header)
    }
}

/// Client side commands from the Virtual Host Controller
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum UsbIpCommand {
    OpReqDevlist {
        status: u32,
    },
    OpReqImport {
        status: u32,
        busid: [u8; 32],
    },
    UsbIpCmdSubmit {
        header: UsbIpHeaderBasic,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup: [u8; 8],
        data: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpCmdUnlink {
        header: UsbIpHeaderBasic,
        unlink_seqnum: u32,
    },
}

impl UsbIpCommand {
    /// Constructs a [UsbIpCommand] from a socket
    ///
    /// This will consume a variable amount of bytes from the socket.
    /// It might fail if the bytes does not follow the USB/IP protocol properly.
    pub async fn read_from_socket<T: AsyncReadExt + Unpin>(socket: &mut T) -> Result<UsbIpCommand> {
        Self::read_from_socket_timeout(socket, None).await
    }

    /// Same as [UsbIpCommand::read_from_socket] with an overall deadline:
    /// the first byte of the command must arrive within `timeout`, and the
    /// remaining fields must keep streaming. Prevents idle peers from
    /// holding connections/devices forever.
    pub async fn read_from_socket_timeout<T: AsyncReadExt + Unpin>(
        socket: &mut T,
        timeout: Option<std::time::Duration>,
    ) -> Result<UsbIpCommand> {
        match timeout {
            Some(deadline) => tokio::time::timeout(deadline, Self::read_from_socket_inner(socket))
                .await
                .unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for USB/IP command",
                    ))
                }),
            None => Self::read_from_socket_inner(socket).await,
        }
    }

    async fn read_from_socket_inner<T: AsyncReadExt + Unpin>(
        socket: &mut T,
    ) -> Result<UsbIpCommand> {
        let version: u16 = socket.read_u16().await?;

        if version != 0 && version != USBIP_VERSION {
            return Err(std::io::Error::other(format!(
                "Unknown version: {version:#04X}"
            )));
        }

        let command: u16 = socket.read_u16().await?;

        trace!(
            "Received command: {:#04X} ({}), parsing...",
            command,
            match command {
                OP_REQ_DEVLIST => "OP_REQ_DEVLIST",
                OP_REQ_IMPORT => "OP_REQ_IMPORT",
                USBIP_CMD_SUBMIT => "USBIP_CMD_SUBMIT",
                USBIP_CMD_UNLINK => "USBIP_CMD_UNLINK",
                _ => "Unknown",
            }
        );

        match command {
            OP_REQ_DEVLIST => {
                let status = socket.read_u32().await?;
                if status != 0 {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!("OP_REQ_DEVLIST status must be 0, got {status}"),
                    ));
                }

                Ok(UsbIpCommand::OpReqDevlist { status })
            }
            OP_REQ_IMPORT => {
                let status = socket.read_u32().await?;
                if status != 0 {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!("OP_REQ_IMPORT status must be 0, got {status}"),
                    ));
                }
                let mut busid = [0; 32];
                socket.read_exact(&mut busid).await?;

                Ok(UsbIpCommand::OpReqImport { status, busid })
            }
            USBIP_CMD_SUBMIT => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_SUBMIT)
                        .await?;
                let transfer_flags = socket.read_u32().await?;
                let transfer_buffer_length = socket.read_u32().await?;
                let start_frame = socket.read_u32().await?;
                let number_of_packets = socket.read_u32().await?;
                let interval = socket.read_u32().await?;

                // Allocation bounds come from the network, so enforce them
                // before any Vec allocation. Without these a client could
                // request multi-gigabyte buffers (OOM) directly.
                if transfer_buffer_length > MAX_TRANSFER_BUFFER_LENGTH {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "transfer_buffer_length {transfer_buffer_length} exceeds limit {MAX_TRANSFER_BUFFER_LENGTH}"
                        ),
                    ));
                }
                if number_of_packets != 0
                    && number_of_packets != 0xFFFFFFFF
                    && number_of_packets > MAX_NUMBER_OF_PACKETS
                {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "number_of_packets {number_of_packets} exceeds limit {MAX_NUMBER_OF_PACKETS}"
                        ),
                    ));
                }

                let mut setup = [0; 8];
                socket.read_exact(&mut setup).await?;

                let data = if header.direction == Direction::In as u32 {
                    vec![]
                } else {
                    let mut data = vec![0; transfer_buffer_length as usize];
                    socket.read_exact(&mut data).await?;
                    data
                };

                // The kernel docs specifies that this should be set to 0xFFFFFFFF for all
                // non-ISO packets, however the actual implementation resorts to 0x00000000
                // https://stackoverflow.com/questions/76899798/usb-ip-what-is-the-size-of-the-iso-packet-descriptor
                let iso_packet_descriptor =
                    if number_of_packets != 0 && number_of_packets != 0xFFFFFFFF {
                        // checked_mul: never compute the allocation size with
                        // a raw multiply of network-supplied values.
                        let total = ISO_PACKET_DESCRIPTOR_SIZE
                            .checked_mul(number_of_packets as usize)
                            .ok_or_else(|| {
                                std::io::Error::new(
                                    ErrorKind::InvalidData,
                                    "ISO packet descriptor size overflow",
                                )
                            })?;
                        let mut result = vec![0; total];
                        socket.read_exact(&mut result).await?;
                        result
                    } else {
                        vec![]
                    };

                Ok(UsbIpCommand::UsbIpCmdSubmit {
                    header,
                    transfer_flags,
                    transfer_buffer_length,
                    start_frame,
                    number_of_packets,
                    interval,
                    setup,
                    data,
                    iso_packet_descriptor,
                })
            }
            USBIP_CMD_UNLINK => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_UNLINK)
                        .await?;
                let unlink_seqnum = socket.read_u32().await?;

                let mut _padding = [0; 24];
                socket.read_exact(&mut _padding).await?;

                Ok(UsbIpCommand::UsbIpCmdUnlink {
                    header,
                    unlink_seqnum,
                })
            }
            _ => Err(std::io::Error::other(format!(
                "Unknown command: {command:#04X}"
            ))),
        }
    }

    /// Converts the [UsbIpCommand] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            UsbIpCommand::OpReqDevlist { status } => {
                let mut result = Vec::with_capacity(8);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result
            }
            UsbIpCommand::OpReqImport { status, busid } => {
                let mut result = Vec::with_capacity(40);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&busid);
                result
            }
            UsbIpCommand::UsbIpCmdSubmit {
                ref header,
                transfer_flags,
                transfer_buffer_length,
                start_frame,
                number_of_packets,
                interval,
                setup,
                ref data,
                ref iso_packet_descriptor,
            } => {
                debug_assert!(
                    header.direction != Direction::Out as u32
                        || transfer_buffer_length == data.len() as u32
                );

                let mut result = Vec::with_capacity(48 + data.len() + iso_packet_descriptor.len());
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&transfer_flags.to_be_bytes());
                result.extend_from_slice(&transfer_buffer_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&interval.to_be_bytes());
                result.extend_from_slice(&setup);
                result.extend_from_slice(data);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            UsbIpCommand::UsbIpCmdUnlink {
                ref header,
                unlink_seqnum,
            } => {
                let mut result = Vec::with_capacity(48);
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&unlink_seqnum.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }
}

/// Server side responses from the USB Host
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum UsbIpResponse {
    OpRepDevlist {
        status: u32,
        device_count: u32,
        devices: Vec<UsbDevice>,
    },
    OpRepImport {
        status: u32,
        device: Option<UsbDevice>,
    },
    UsbIpRetSubmit {
        header: UsbIpHeaderBasic,
        status: u32,
        actual_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        error_count: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpRetUnlink {
        header: UsbIpHeaderBasic,
        status: u32,
    },
}

impl UsbIpResponse {
    /// Converts the [UsbIpResponse] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            Self::OpRepDevlist {
                status,
                device_count,
                ref devices,
            } => {
                let mut result = Vec::with_capacity(
                    12 + devices.len() * 312
                        + devices
                            .iter()
                            .map(|d| d.interfaces.len() * 4)
                            .sum::<usize>(),
                );
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&device_count.to_be_bytes());
                for dev in devices {
                    result.extend_from_slice(&dev.to_bytes_with_interfaces());
                }
                result
            }
            Self::OpRepImport { status, ref device } => {
                let mut result = Vec::with_capacity(320);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                if let Some(device) = device {
                    result.extend_from_slice(&device.to_bytes());
                }
                result
            }
            Self::UsbIpRetSubmit {
                ref header,
                status,
                actual_length,
                start_frame,
                number_of_packets,
                error_count,
                ref transfer_buffer,
                ref iso_packet_descriptor,
            } => {
                let mut result =
                    Vec::with_capacity(48 + transfer_buffer.len() + iso_packet_descriptor.len());

                debug_assert!(header.command == USBIP_RET_SUBMIT.into());
                if header.direction == Direction::In as u32 {
                    debug_assert!(actual_length == transfer_buffer.len() as u32);
                } else {
                    debug_assert!(transfer_buffer.is_empty());
                }

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&actual_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&error_count.to_be_bytes());
                result.extend_from_slice(&[0; 8]);
                result.extend_from_slice(transfer_buffer);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            Self::UsbIpRetUnlink { ref header, status } => {
                let mut result = Vec::with_capacity(48);

                debug_assert!(header.command == USBIP_RET_UNLINK.into());

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }

    pub async fn write_to_socket<T: AsyncWriteExt + Unpin>(&self, socket: &mut T) -> Result<()> {
        socket.write_all(&self.to_bytes()).await
    }

    /// Constructs a OP_REP_DEVLIST response
    pub fn op_rep_devlist(devices: &[UsbDevice]) -> Self {
        Self::OpRepDevlist {
            status: 0,
            device_count: devices.len() as u32,
            devices: devices.to_vec(),
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn op_rep_import_success(device: &UsbDevice) -> Self {
        Self::OpRepImport {
            status: 0,
            device: Some(device.clone()),
        }
    }

    /// Constructs a failed OP_REP_IMPORT response
    pub fn op_rep_import_fail() -> Self {
        Self::OpRepImport {
            status: 1,
            device: None,
        }
    }

    /// Constructs a successful USBIP_RET_SUBMIT response
    pub fn usbip_ret_submit_success(
        header: &UsbIpHeaderBasic,
        start_frame: u32,
        number_of_packets: u32,
        actual_length: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    ) -> Self {
        Self::usbip_ret_submit_result(
            header,
            0,
            start_frame,
            number_of_packets,
            actual_length,
            transfer_buffer,
            iso_packet_descriptor,
        )
    }

    /// Constructs USBIP_RET_SUBMIT with a Linux URB status value.
    pub fn usbip_ret_submit_result(
        header: &UsbIpHeaderBasic,
        status: i32,
        start_frame: u32,
        number_of_packets: u32,
        actual_length: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    ) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: status as u32,
            actual_length,
            start_frame,
            number_of_packets,
            error_count: 0,
            transfer_buffer,
            iso_packet_descriptor,
        }
    }

    /// Constructs a failed USBIP_RET_SUBMIT response
    pub fn usbip_ret_submit_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 1,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            transfer_buffer: vec![],
            iso_packet_descriptor: vec![],
        }
    }

    /// Constructs a successful USBIP_RET_UNLINK response
    pub fn usbip_ret_unlink_success(header: &UsbIpHeaderBasic) -> Self {
        Self::usbip_ret_unlink_result(header, 0)
    }

    /// Constructs USBIP_RET_UNLINK with a Linux URB status value.
    pub fn usbip_ret_unlink_result(header: &UsbIpHeaderBasic, status: i32) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: status as u32,
        }
    }

    /// Constructs a failed USBIP_RET_UNLINK response
    pub fn usbip_ret_unlink_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::util::tests::*;

    use super::*;

    fn example_device() -> UsbDevice {
        UsbDevice::default()
    }

    #[test]
    fn byte_serialize_op_req_devlist() {
        setup_test_logger();
        let cmd = UsbIpCommand::OpReqDevlist { status: 0 };
        assert_eq!(
            cmd.to_bytes(),
            [
                0x01, 0x11, // version,
                0x80, 0x05, // command
                0x00, 0x00, 0x00, 0x00, // status
            ]
        );
    }

    #[test]
    fn byte_serialize_op_req_import() {
        setup_test_logger();
        let cmd = UsbIpCommand::OpReqImport {
            status: 0,
            busid: [0; 32],
        };
        assert_eq!(
            cmd.to_bytes(),
            [
                0x01, 0x11, // version,
                0x80, 0x03, // command
                0x00, 0x00, 0x00, 0x00, // status
                0x00, 0x00, 0x00, 0x00, // busid
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
            ]
        );
    }

    #[test]
    fn byte_serialize_usbip_cmd_submit() {
        setup_test_logger();
        let cmd = UsbIpCommand::UsbIpCmdSubmit {
            header: UsbIpHeaderBasic {
                command: 1,
                seqnum: 2,
                devid: 3,
                direction: 4,
                ep: 5,
            },
            transfer_flags: 0,
            transfer_buffer_length: 0,
            start_frame: 6,
            number_of_packets: 0,
            interval: 7,
            setup: [0xFF; 8],
            data: vec![],
            iso_packet_descriptor: vec![],
        };
        assert_eq!(
            cmd.to_bytes(),
            [
                0x00, 0x00, 0x00, 0x01, // command
                0x00, 0x00, 0x00, 0x02, // seqnum
                0x00, 0x00, 0x00, 0x03, // devid
                0x00, 0x00, 0x00, 0x04, // direction
                0x00, 0x00, 0x00, 0x05, // ep
                0x00, 0x00, 0x00, 0x00, // transfer_flags
                0x00, 0x00, 0x00, 0x00, // transfer_buffer_length
                0x00, 0x00, 0x00, 0x06, // start_frame
                0x00, 0x00, 0x00, 0x00, // number_of_packets
                0x00, 0x00, 0x00, 0x07, // interval
                0xFF, 0xFF, 0xFF, 0xFF, // setup
                0xFF, 0xFF, 0xFF, 0xFF,
                // data
                // iso_packet_descriptor
            ]
        );
    }

    #[test]
    fn byte_serialize_usbip_cmd_unlink() {
        setup_test_logger();
        let cmd = UsbIpCommand::UsbIpCmdUnlink {
            header: UsbIpHeaderBasic {
                command: 0,
                seqnum: 1,
                devid: 2,
                direction: 3,
                ep: 4,
            },
            unlink_seqnum: 5,
        };
        assert_eq!(
            cmd.to_bytes(),
            [
                0x00, 0x00, 0x00, 0x00, // command
                0x00, 0x00, 0x00, 0x01, // seqnum
                0x00, 0x00, 0x00, 0x02, // devid
                0x00, 0x00, 0x00, 0x03, // direction
                0x00, 0x00, 0x00, 0x04, // ep
                0x00, 0x00, 0x00, 0x05, // unlink_seqnum
                0x00, 0x00, 0x00, 0x00, // padding
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, //
            ]
        );
    }

    #[test]
    fn byte_serialize_op_rep_devlist() {
        setup_test_logger();
        let device = example_device();
        let res = UsbIpResponse::op_rep_devlist(std::slice::from_ref(&device));
        assert_eq!(
            res.to_bytes(),
            [
                vec![0x01, 0x11],             // version
                vec![0x00, 0x05],             // command
                vec![0x00, 0x00, 0x00, 0x00], // status
                vec![0x00, 0x00, 0x00, 0x01], // device_count
                device.to_bytes()
            ]
            .concat()
            .as_slice()
        );
    }

    #[test]
    fn byte_serialize_op_rep_import() {
        setup_test_logger();
        let device = example_device();
        let res = UsbIpResponse::op_rep_import_success(&device);
        assert_eq!(
            res.to_bytes(),
            [
                vec![0x01, 0x11],             // version
                vec![0x00, 0x03],             // command
                vec![0x00, 0x00, 0x00, 0x00], // status
                device.to_bytes()
            ]
            .concat()
            .as_slice()
        );

        let res = UsbIpResponse::op_rep_import_fail();
        assert_eq!(
            res.to_bytes(),
            vec![
                0x01, 0x11, // version
                0x00, 0x03, // command
                0x00, 0x00, 0x00, 0x01, // status
            ]
        );
    }

    #[test]
    fn byte_serialize_usbip_ret_submit() {
        setup_test_logger();
        let res = UsbIpResponse::UsbIpRetSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_RET_SUBMIT.into(),
                seqnum: 2,
                devid: 3,
                direction: Direction::In as u32,
                ep: 4,
            },
            status: 5,
            actual_length: 4,
            start_frame: 7,
            number_of_packets: 8,
            error_count: 9,
            transfer_buffer: vec![0xFF; 4],
            iso_packet_descriptor: vec![],
        };

        assert_eq!(
            res.to_bytes(),
            vec![
                0x00, 0x00, 0x00, 0x03, // command
                0x00, 0x00, 0x00, 0x02, // seqnum
                0x00, 0x00, 0x00, 0x03, // devid
                0x00, 0x00, 0x00, 0x01, // direction
                0x00, 0x00, 0x00, 0x04, // ep
                0x00, 0x00, 0x00, 0x05, // status
                0x00, 0x00, 0x00, 0x04, // actual_length
                0x00, 0x00, 0x00, 0x07, // start_frame
                0x00, 0x00, 0x00, 0x08, // number_of_packets
                0x00, 0x00, 0x00, 0x09, // error_count
                0x00, 0x00, 0x00, 0x00, // padding
                0x00, 0x00, 0x00, 0x00, //
                0xFF, 0xFF, 0xFF,
                0xFF, // transfer_buffer
                      // iso_packet_descriptor
            ],
        );
    }

    #[test]
    #[should_panic]
    fn byte_serialize_invalid_usbip_ret_submit() {
        setup_test_logger();
        let res = UsbIpResponse::UsbIpRetSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_RET_SUBMIT.into(),
                seqnum: 2,
                devid: 3,
                direction: Direction::Out as u32, // data section should be empty, but is not
                ep: 4,
            },
            status: 5,
            actual_length: 4,
            start_frame: 7,
            number_of_packets: 8,
            error_count: 9,
            transfer_buffer: vec![0xFF; 4],
            iso_packet_descriptor: vec![0xFF; 16],
        };

        res.to_bytes();
    }

    #[test]
    fn byte_serialize_usbip_ret_submit_out() {
        setup_test_logger();
        // OUT transfer: actual_length can be non-zero while transfer_buffer is empty
        let res = UsbIpResponse::UsbIpRetSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_RET_SUBMIT.into(),
                seqnum: 2,
                devid: 3,
                direction: Direction::Out as u32,
                ep: 4,
            },
            status: 0,
            actual_length: 384,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            transfer_buffer: vec![],
            iso_packet_descriptor: vec![],
        };

        let bytes = res.to_bytes();
        // actual_length should be 384 (0x180) at offset 24
        assert_eq!(&bytes[24..28], &[0x00, 0x00, 0x01, 0x80]);
        // transfer_buffer should be empty (only header + fields, no payload)
        assert_eq!(bytes.len(), 48);
    }

    #[test]
    fn byte_serialize_usbip_ret_unlink() {
        setup_test_logger();
        let res = UsbIpResponse::usbip_ret_unlink_success(&UsbIpHeaderBasic {
            command: USBIP_RET_UNLINK.into(),
            seqnum: 1,
            devid: 2,
            direction: 3,
            ep: 4,
        });

        let mut expected_result = vec![
            0x00, 0x00, 0x00, 0x04, // command
            0x00, 0x00, 0x00, 0x01, // seqnum
            0x00, 0x00, 0x00, 0x02, // devid
            0x00, 0x00, 0x00, 0x03, // direction
            0x00, 0x00, 0x00, 0x04, // ep
            0x00, 0x00, 0x00, 0x00, // status
            0x00, 0x00, 0x00, 0x00, // padding
            0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
        ];

        assert_eq!(res.to_bytes(), expected_result,);

        let res = UsbIpResponse::usbip_ret_unlink_fail(&UsbIpHeaderBasic {
            command: USBIP_RET_UNLINK.into(),
            seqnum: 1,
            devid: 2,
            direction: 3,
            ep: 4,
        });

        expected_result[5 * 4 + 3] = 1; // status

        assert_eq!(res.to_bytes(), expected_result,);
    }

    #[tokio::test]
    async fn read_op_req_devlist_from_socket() -> Result<()> {
        setup_test_logger();
        let cmd = UsbIpCommand::OpReqDevlist { status: 0 };

        assert_eq!(
            cmd.to_bytes(),
            UsbIpCommand::read_from_socket(&mut MockSocket::new(cmd.to_bytes()))
                .await?
                .to_bytes()
        );

        Ok(())
    }

    #[tokio::test]
    async fn read_op_req_import_from_socket() -> Result<()> {
        setup_test_logger();
        let cmd = UsbIpCommand::OpReqImport {
            status: 0,
            busid: {
                let mut b = "0-0-0".as_bytes().to_vec();
                b.resize(32, 0);
                b.as_slice().try_into().unwrap()
            },
        };

        assert_eq!(
            cmd.to_bytes(),
            UsbIpCommand::read_from_socket(&mut MockSocket::new(cmd.to_bytes()))
                .await?
                .to_bytes()
        );

        Ok(())
    }

    #[tokio::test]
    async fn read_usbip_cmd_submit_from_socket() -> Result<()> {
        setup_test_logger();
        let cmd = UsbIpCommand::UsbIpCmdSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_CMD_SUBMIT.into(),
                seqnum: 1,
                devid: 2,
                direction: Direction::Out as u32,
                ep: 4,
            },
            transfer_flags: 5,
            transfer_buffer_length: 4,
            start_frame: 7,
            number_of_packets: 1,
            interval: 9,
            setup: [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07],
            data: vec![0x08, 0x09, 0x0A, 0x0B],
            iso_packet_descriptor: vec![0xFF; 16],
        };

        assert_eq!(
            cmd.to_bytes(),
            UsbIpCommand::read_from_socket(&mut MockSocket::new(cmd.to_bytes()))
                .await?
                .to_bytes()
        );

        Ok(())
    }

    #[tokio::test]
    async fn read_usbip_cmd_submit_from_socket_with_no_data() -> Result<()> {
        setup_test_logger();
        let cmd = UsbIpCommand::UsbIpCmdSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_CMD_SUBMIT.into(),
                seqnum: 1,
                devid: 2,
                direction: Direction::In as u32, // data section should be ignored despite transfer_buffer_length
                ep: 4,
            },
            transfer_flags: 5,
            transfer_buffer_length: 64,
            start_frame: 7,
            number_of_packets: 1,
            interval: 9,
            setup: [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0xFF],
            data: vec![],
            iso_packet_descriptor: vec![0xFF; 16],
        };

        assert_eq!(
            cmd.to_bytes(),
            UsbIpCommand::read_from_socket(&mut MockSocket::new(cmd.to_bytes()))
                .await?
                .to_bytes()
        );

        Ok(())
    }

    #[tokio::test]
    async fn read_usbip_cmd_unlink_from_socket() -> Result<()> {
        setup_test_logger();
        let cmd = UsbIpCommand::UsbIpCmdUnlink {
            header: UsbIpHeaderBasic {
                command: USBIP_CMD_UNLINK.into(),
                seqnum: 1,
                devid: 2,
                direction: 0,
                ep: 4,
            },
            unlink_seqnum: 1,
        };

        assert_eq!(
            cmd.to_bytes(),
            UsbIpCommand::read_from_socket(&mut MockSocket::new(cmd.to_bytes()))
                .await?
                .to_bytes()
        );

        Ok(())
    }

    #[tokio::test]
    async fn byte_serialization_fails_on_old_usbip_version() {
        setup_test_logger();

        let cmd = UsbIpCommand::OpReqDevlist { status: 0 };
        let mut bytes = cmd.to_bytes();
        bytes[1] = 0x10; // set version to 0x0110

        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Unknown version: 0x110".to_string()
        );
    }

    #[tokio::test]
    async fn byte_serialization_fails_on_invalid_command() {
        setup_test_logger();

        let cmd = UsbIpCommand::OpReqDevlist { status: 0 };
        let mut bytes = cmd.to_bytes();
        bytes[2] = 0x10; // set command to 0x1005

        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Unknown command: 0x1005".to_string()
        );
    }

    fn submit_with(buffer_length: u32, number_of_packets: u32) -> Vec<u8> {
        // direction In → no data section on the wire.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&USBIP_VERSION.to_be_bytes());
        bytes.extend_from_slice(&USBIP_CMD_SUBMIT.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes()); // seqnum
        bytes.extend_from_slice(&0u32.to_be_bytes()); // devid
        bytes.extend_from_slice(&1u32.to_be_bytes()); // direction = In
        bytes.extend_from_slice(&4u32.to_be_bytes()); // ep
        bytes.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        bytes.extend_from_slice(&buffer_length.to_be_bytes()); // transfer_buffer_length
        bytes.extend_from_slice(&0u32.to_be_bytes()); // start_frame
        bytes.extend_from_slice(&number_of_packets.to_be_bytes()); // number_of_packets
        bytes.extend_from_slice(&0u32.to_be_bytes()); // interval
        bytes.extend_from_slice(&[0u8; 8]); // setup
        bytes
    }

    #[tokio::test]
    async fn submit_rejects_oversized_transfer_buffer() {
        setup_test_logger();
        let bytes = submit_with(MAX_TRANSFER_BUFFER_LENGTH + 1, 0);
        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds limit"));
    }

    #[tokio::test]
    async fn submit_rejects_oversized_iso_packet_count() {
        setup_test_logger();
        let bytes = submit_with(0, MAX_NUMBER_OF_PACKETS + 1);
        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds limit"));
    }

    #[tokio::test]
    async fn submit_rejects_invalid_direction() {
        setup_test_logger();
        let bytes = submit_with(0, 0);
        let mut bytes = bytes;
        // direction field at offset 4 (version+command) + 4 (seqnum) + 4 (devid)
        let offset = 2 + 2 + 4 + 4;
        bytes[offset..offset + 4].copy_from_slice(&7u32.to_be_bytes());
        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid direction")
        );
    }

    #[tokio::test]
    async fn submit_rejects_oversized_endpoint() {
        setup_test_logger();
        let bytes = submit_with(0, 0);
        let mut bytes = bytes;
        let offset = 2 + 2 + 4 + 4 + 4;
        bytes[offset..offset + 4].copy_from_slice(&300u32.to_be_bytes());
        let mut socket = MockSocket::new(bytes);
        let result = UsbIpCommand::read_from_socket(&mut socket).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid endpoint number")
        );
    }
}
