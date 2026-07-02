//! `/dev/uhid` device forwarding for HID-level controller pass-through.
//!
//! For each HID device the client forwards (see the client's `hidpass`
//! module), this creates a uhid device carrying the original identity
//! and report descriptor. The host kernel attaches the *real* driver —
//! for Valve controllers that is `hid-steam`, which builds the full
//! stack (input node + hidraw), so Steam Input adopts the device as if
//! it were plugged in locally.
//!
//! Input reports from the client are replayed with `UHID_INPUT2`.
//! Requests the host driver makes on the device (`UHID_OUTPUT`,
//! `UHID_GET_REPORT`, `UHID_SET_REPORT` — hid-steam probes the serial
//! via feature reports at attach time) are forwarded to the client
//! over the control stream and answered with
//! [`InputEvent::HidPassthroughReply`].
//!
//! The uhid wire structs are `__attribute__((packed))`, so events are
//! built and parsed as explicit byte buffers instead of repr(C) types.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::input::{HidDeviceDescriptor, HidHostRequest, HidReportType};
use stargaze_core::transport::ControlMessage;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// `enum uhid_event_type` values (include/uapi/linux/uhid.h).
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_STOP: u32 = 3;
const UHID_OPEN: u32 = 4;
const UHID_CLOSE: u32 = 5;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

/// `HID_MAX_DESCRIPTOR_SIZE`; also the data cap in uhid payloads.
const UHID_DATA_MAX: usize = 4096;

/// Size of `struct uhid_event`: 4-byte type + the largest (packed)
/// union member, `uhid_create2_req` (128+64+64+2+2+4+4+4+4+4096).
const UHID_EVENT_SIZE: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 + 4 + 4 + 4 + UHID_DATA_MAX;

/// `enum uhid_report_type` values.
#[cfg(test)]
fn report_type_code(rtype: HidReportType) -> u8 {
    match rtype {
        HidReportType::Feature => 0,
        HidReportType::Output => 1,
        HidReportType::Input => 2,
    }
}

fn report_type_from_code(code: u8) -> HidReportType {
    match code {
        1 => HidReportType::Output,
        2 => HidReportType::Input,
        _ => HidReportType::Feature,
    }
}

/// Copies `s` into a fixed-size NUL-padded field, truncating to keep a
/// terminating NUL (uhid requires NUL-terminated strings).
fn put_fixed_str(buf: &mut Vec<u8>, s: &str, field_len: usize) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(field_len - 1);
    buf.extend_from_slice(&bytes[..n]);
    buf.resize(buf.len() + (field_len - n), 0);
}

/// Builds a `UHID_CREATE2` event from a forwarded descriptor.
fn create2_event(desc: &HidDeviceDescriptor) -> Vec<u8> {
    let rd = &desc.report_descriptor[..desc.report_descriptor.len().min(UHID_DATA_MAX)];
    let mut buf = Vec::with_capacity(280 + rd.len());
    buf.extend_from_slice(&UHID_CREATE2.to_le_bytes());
    put_fixed_str(&mut buf, &desc.name, 128);
    put_fixed_str(&mut buf, &desc.phys, 64);
    put_fixed_str(&mut buf, &desc.uniq, 64);
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(rd.len() as u16).to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(desc.bus_type as u16).to_le_bytes());
    buf.extend_from_slice(&u32::from(desc.vendor).to_le_bytes());
    buf.extend_from_slice(&u32::from(desc.product).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // version (not exposed by hidraw)
    buf.extend_from_slice(&0u32.to_le_bytes()); // country
    buf.extend_from_slice(rd);
    buf
}

/// Builds a `UHID_INPUT2` event from one raw input report.
fn input2_event(data: &[u8]) -> Vec<u8> {
    let data = &data[..data.len().min(UHID_DATA_MAX)];
    let mut buf = Vec::with_capacity(6 + data.len());
    buf.extend_from_slice(&UHID_INPUT2.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Builds a `UHID_GET_REPORT_REPLY` event.
fn get_report_reply_event(request: u32, err: bool, data: &[u8]) -> Vec<u8> {
    let data = &data[..data.len().min(UHID_DATA_MAX)];
    let mut buf = Vec::with_capacity(12 + data.len());
    buf.extend_from_slice(&UHID_GET_REPORT_REPLY.to_le_bytes());
    buf.extend_from_slice(&request.to_le_bytes());
    buf.extend_from_slice(&u16::from(err).to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Builds a `UHID_SET_REPORT_REPLY` event.
fn set_report_reply_event(request: u32, err: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    buf.extend_from_slice(&UHID_SET_REPORT_REPLY.to_le_bytes());
    buf.extend_from_slice(&request.to_le_bytes());
    buf.extend_from_slice(&u16::from(err).to_le_bytes());
    buf
}

/// A kernel-side request parsed from a uhid event read off the fd.
#[derive(Debug, PartialEq, Eq)]
enum UhidKernelEvent {
    Output {
        data: Vec<u8>,
    },
    GetReport {
        id: u32,
        rnum: u8,
        rtype: u8,
    },
    SetReport {
        id: u32,
        rnum: u8,
        rtype: u8,
        data: Vec<u8>,
    },
    Start,
    Stop,
    Other(u32),
}

/// Parses one event read from `/dev/uhid`.
fn parse_kernel_event(buf: &[u8]) -> Option<UhidKernelEvent> {
    if buf.len() < 4 {
        return None;
    }
    let ev_type = u32::from_le_bytes(buf[..4].try_into().ok()?);
    Some(match ev_type {
        // struct uhid_output_req { u8 data[4096]; u16 size; u8 rtype; }
        UHID_OUTPUT => {
            let size = usize::from(u16::from_le_bytes(
                buf.get(4 + UHID_DATA_MAX..4 + UHID_DATA_MAX + 2)?
                    .try_into()
                    .ok()?,
            ));
            UhidKernelEvent::Output {
                data: buf.get(4..4 + size.min(UHID_DATA_MAX))?.to_vec(),
            }
        }
        // struct uhid_get_report_req { u32 id; u8 rnum; u8 rtype; }
        UHID_GET_REPORT => UhidKernelEvent::GetReport {
            id: u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?),
            rnum: *buf.get(8)?,
            rtype: *buf.get(9)?,
        },
        // struct uhid_set_report_req { u32 id; u8 rnum; u8 rtype; u16 size; u8 data[4096]; }
        UHID_SET_REPORT => {
            let size = usize::from(u16::from_le_bytes(buf.get(10..12)?.try_into().ok()?));
            UhidKernelEvent::SetReport {
                id: u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?),
                rnum: *buf.get(8)?,
                rtype: *buf.get(9)?,
                data: buf.get(12..12 + size.min(UHID_DATA_MAX))?.to_vec(),
            }
        }
        UHID_START => UhidKernelEvent::Start,
        UHID_STOP => UhidKernelEvent::Stop,
        other => UhidKernelEvent::Other(other),
    })
}

/// One live uhid device mirroring a client-side HID controller.
pub(crate) struct UhidDevice {
    file: File,
    /// Signals the kernel-event reader thread to stop after destroy.
    stopping: Arc<AtomicBool>,
}

impl UhidDevice {
    /// Creates the uhid device and spawns a reader thread that forwards
    /// kernel requests (output/get/set report) to the client via
    /// `hid_out_tx` as [`ControlMessage::HidRequest`].
    ///
    /// # Errors
    ///
    /// Fails if `/dev/uhid` cannot be opened (missing permissions or
    /// the `uhid` kernel module) or the create event is rejected.
    pub(crate) fn create(
        hid: u8,
        desc: &HidDeviceDescriptor,
        hid_out_tx: mpsc::Sender<ControlMessage>,
    ) -> std::io::Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uhid")?;
        file.write_all(&create2_event(desc))?;

        let stopping = Arc::new(AtomicBool::new(false));
        let reader = file.try_clone()?;
        let reader_stopping = Arc::clone(&stopping);
        let spawned = std::thread::Builder::new()
            .name(format!("stargaze-uhid{hid}"))
            .spawn(move || kernel_event_loop(hid, reader, &hid_out_tx, &reader_stopping));
        if let Err(e) = spawned {
            warn!(hid, "Failed to spawn uhid reader thread: {e}");
        }
        Ok(Self { file, stopping })
    }

    /// Replays one raw input report from the client.
    pub(crate) fn input(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(&input2_event(data))
    }

    /// Answers a pending `UHID_GET_REPORT` from the kernel.
    pub(crate) fn get_report_reply(
        &mut self,
        request: u32,
        err: bool,
        data: &[u8],
    ) -> std::io::Result<()> {
        self.file
            .write_all(&get_report_reply_event(request, err, data))
    }

    /// Answers a pending `UHID_SET_REPORT` from the kernel.
    pub(crate) fn set_report_reply(&mut self, request: u32, err: bool) -> std::io::Result<()> {
        self.file.write_all(&set_report_reply_event(request, err))
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let mut destroy = Vec::with_capacity(4);
        destroy.extend_from_slice(&UHID_DESTROY.to_le_bytes());
        if let Err(e) = self.file.write_all(&destroy) {
            warn!("Failed to destroy uhid device: {e}");
        }
        // Closing our fd (and the reader's clone once its read returns)
        // finishes the teardown.
    }
}

/// Blocking reader for kernel-originated uhid events on one device.
fn kernel_event_loop(
    hid: u8,
    mut file: File,
    hid_out_tx: &mpsc::Sender<ControlMessage>,
    stopping: &AtomicBool,
) {
    let mut buf = vec![0u8; UHID_EVENT_SIZE];
    loop {
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if !stopping.load(Ordering::Relaxed) {
                    debug!(hid, "uhid read ended: {e}");
                }
                return;
            }
        };
        let request = match parse_kernel_event(&buf[..n]) {
            Some(UhidKernelEvent::Output { data }) => HidHostRequest::Output { hid, data },
            Some(UhidKernelEvent::GetReport { id, rnum, rtype }) => HidHostRequest::GetReport {
                hid,
                request: id,
                report_number: rnum,
                report_type: report_type_from_code(rtype),
            },
            Some(UhidKernelEvent::SetReport {
                id,
                rnum,
                rtype,
                data,
            }) => HidHostRequest::SetReport {
                hid,
                request: id,
                report_number: rnum,
                report_type: report_type_from_code(rtype),
                data,
            },
            Some(UhidKernelEvent::Start) => {
                info!(hid, "uhid device started (host driver attached)");
                continue;
            }
            Some(UhidKernelEvent::Stop) => {
                debug!(hid, "uhid device stopped");
                return;
            }
            Some(UhidKernelEvent::Other(t)) => {
                let _ = matches!(t, UHID_OPEN | UHID_CLOSE); // uninteresting
                continue;
            }
            None => continue,
        };
        if hid_out_tx
            .blocking_send(ControlMessage::HidRequest(request))
            .is_err()
        {
            debug!(hid, "Control channel closed, stopping uhid reader");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> HidDeviceDescriptor {
        HidDeviceDescriptor {
            name: "Wireless Steam Controller".to_string(),
            phys: "usb-0000:00:14.0-2/input1".to_string(),
            uniq: "SERIAL42".to_string(),
            bus_type: 0x03,
            vendor: 0x28de,
            product: 0x1142,
            report_descriptor: vec![0x06, 0x00, 0xff],
        }
    }

    #[test]
    fn create2_layout_matches_kernel_struct() {
        let desc = descriptor();
        let buf = create2_event(&desc);
        // 4 (type) + 128 + 64 + 64 + 2 + 2 + 4 + 4 + 4 + 4 + rd_size.
        assert_eq!(buf.len(), 280 + 3);
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()),
            UHID_CREATE2
        );
        // name at offset 4, NUL-terminated.
        assert_eq!(&buf[4..29], b"Wireless Steam Controller");
        assert_eq!(buf[29], 0);
        // rd_size at 4+128+64+64 = 260, bus at 262, vendor at 264.
        assert_eq!(u16::from_le_bytes(buf[260..262].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(buf[262..264].try_into().unwrap()), 0x03);
        assert_eq!(
            u32::from_le_bytes(buf[264..268].try_into().unwrap()),
            0x28de
        );
        assert_eq!(
            u32::from_le_bytes(buf[268..272].try_into().unwrap()),
            0x1142
        );
        // rd_data at 280.
        assert_eq!(&buf[280..], &[0x06, 0x00, 0xff]);
    }

    #[test]
    fn create2_truncates_overlong_name_with_nul() {
        let mut desc = descriptor();
        desc.name = "x".repeat(200);
        let buf = create2_event(&desc);
        // Field is 128 bytes: 127 payload + forced NUL.
        assert_eq!(buf[4 + 127], 0);
        assert_eq!(buf[4 + 126], b'x');
    }

    #[test]
    fn input2_layout() {
        let buf = input2_event(&[0xde, 0xad]);
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()),
            UHID_INPUT2
        );
        assert_eq!(u16::from_le_bytes(buf[4..6].try_into().unwrap()), 2);
        assert_eq!(&buf[6..], &[0xde, 0xad]);
    }

    #[test]
    fn report_replies_layout() {
        let buf = get_report_reply_event(7, false, &[0, 1]);
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()),
            UHID_GET_REPORT_REPLY
        );
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 7);
        assert_eq!(u16::from_le_bytes(buf[8..10].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(buf[10..12].try_into().unwrap()), 2);
        assert_eq!(&buf[12..], &[0, 1]);

        let buf = set_report_reply_event(9, true);
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()),
            UHID_SET_REPORT_REPLY
        );
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 9);
        assert_eq!(u16::from_le_bytes(buf[8..10].try_into().unwrap()), 1);
    }

    #[test]
    fn parses_kernel_get_and_set_report() {
        let mut buf = vec![0u8; UHID_EVENT_SIZE];
        buf[..4].copy_from_slice(&UHID_GET_REPORT.to_le_bytes());
        buf[4..8].copy_from_slice(&11u32.to_le_bytes());
        buf[8] = 0; // rnum
        buf[9] = 0; // rtype = feature
        assert_eq!(
            parse_kernel_event(&buf),
            Some(UhidKernelEvent::GetReport {
                id: 11,
                rnum: 0,
                rtype: 0
            })
        );

        let mut buf = vec![0u8; UHID_EVENT_SIZE];
        buf[..4].copy_from_slice(&UHID_SET_REPORT.to_le_bytes());
        buf[4..8].copy_from_slice(&12u32.to_le_bytes());
        buf[8] = 0;
        buf[9] = 0;
        buf[10..12].copy_from_slice(&3u16.to_le_bytes());
        buf[12..15].copy_from_slice(&[9, 8, 7]);
        assert_eq!(
            parse_kernel_event(&buf),
            Some(UhidKernelEvent::SetReport {
                id: 12,
                rnum: 0,
                rtype: 0,
                data: vec![9, 8, 7]
            })
        );
    }

    #[test]
    fn parses_kernel_output_event() {
        let mut buf = vec![0u8; UHID_EVENT_SIZE];
        buf[..4].copy_from_slice(&UHID_OUTPUT.to_le_bytes());
        buf[4..7].copy_from_slice(&[1, 2, 3]);
        buf[4 + UHID_DATA_MAX..4 + UHID_DATA_MAX + 2].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            parse_kernel_event(&buf),
            Some(UhidKernelEvent::Output {
                data: vec![1, 2, 3]
            })
        );
    }

    #[test]
    fn report_type_codes_round_trip() {
        for rtype in [
            HidReportType::Feature,
            HidReportType::Output,
            HidReportType::Input,
        ] {
            assert_eq!(report_type_from_code(report_type_code(rtype)), rtype);
        }
    }
}
