//! The seam between the control session and whatever moves its bytes.
//!
//! [`Dock`] (libusb) is one transport; a host that brings its own USB stack (the macOS app's
//! IOUSBHost transport, reached through the oracle's C vtable) is another. The session is written
//! against this trait so the protocol can be proven on one transport and then moved to the next
//! without touching a line of it. The trait carries exactly what the session, the EDID fetch and
//! DDC/CI call, nothing more: every method keeps the meaning, the timeouts and the error variants
//! [`Dock`]'s method of the same name has, because the session's control flow branches on them
//! (most of all on [`Error::Timeout`], which it treats as "nothing arrived" rather than failure).

use crate::usb::{Dock, Error};
use std::time::Duration;

/// A DL3 dock that has been opened and placed (see [`Dock::open`]).
///
/// `Send` because the host engines engage on one thread and may hand the session to another; a
/// session never calls a transport from two threads at once.
pub trait Transport: Send {
    /// How many connectors this device actually backs. Loops must use this, never `MAX_HEADS`.
    fn connectors(&self) -> usize;

    /// Vendor IN control transfer (`bmRequestType` 0xc1) with the transport's default timeout.
    fn vendor_in(&self, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, Error>;

    /// Vendor OUT control transfer (`bmRequestType` 0x40) with the transport's default timeout.
    fn vendor_out(&self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), Error>;

    /// Standard IN control transfer to the device (`bmRequestType` 0x80).
    fn std_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, Error>;

    /// Standard OUT control transfer to an interface (`bmRequestType` 0x01).
    fn std_out_iface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Error>;

    /// Claim an interface beyond the ones opening claimed.
    fn claim_interface(&self, iface: u8) -> Result<(), Error>;

    /// One synchronous bulk write to the control endpoint with the default timeout. Returns the
    /// bytes the device accepted, which can be fewer than offered without an error.
    fn write_ctrl_raw(&self, bytes: &[u8]) -> Result<usize, Error>;

    /// One control-endpoint write with no transfer timeout and a two-second completion deadline,
    /// after which it is cancelled and drained. A short write is [`Error::ShortWrite`].
    fn write_ctrl_dlm(&self, bytes: &[u8]) -> Result<usize, Error>;

    /// A whole frame's transfers on one connector's video endpoint, up to eight in flight, each
    /// chunk one transfer exactly as given. Returns the bytes written.
    fn write_video_frame(&self, head: usize, chunks: &[Vec<u8>]) -> Result<usize, Error>;

    /// Clear the halt condition on one connector's video endpoint.
    fn clear_video_halt(&self, head: usize) -> Result<(), Error>;

    /// The next dock reply from the always-armed reply queue, or [`Error::Timeout`].
    fn recv_frame_raw_timeout(&self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, Error>;
}

/// Every method forwards to the inherent one, so the libusb path runs the code it always ran.
impl Transport for Dock {
    fn connectors(&self) -> usize {
        Dock::connectors(self)
    }

    fn vendor_in(&self, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, Error> {
        Dock::vendor_in(self, request, value, index, length)
    }

    fn vendor_out(&self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), Error> {
        Dock::vendor_out(self, request, value, index, data)
    }

    fn std_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, Error> {
        Dock::std_in(self, request, value, index, length, timeout)
    }

    fn std_out_iface(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Error> {
        Dock::std_out_iface(self, request, value, index, data, timeout)
    }

    fn claim_interface(&self, iface: u8) -> Result<(), Error> {
        Dock::claim_interface(self, iface)
    }

    fn write_ctrl_raw(&self, bytes: &[u8]) -> Result<usize, Error> {
        Dock::write_ctrl_raw(self, bytes)
    }

    fn write_ctrl_dlm(&self, bytes: &[u8]) -> Result<usize, Error> {
        Dock::write_ctrl_dlm(self, bytes)
    }

    fn write_video_frame(&self, head: usize, chunks: &[Vec<u8>]) -> Result<usize, Error> {
        Dock::write_video_frame(self, head, chunks)
    }

    fn clear_video_halt(&self, head: usize) -> Result<(), Error> {
        Dock::clear_video_halt(self, head)
    }

    fn recv_frame_raw_timeout(&self, max_len: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        Dock::recv_frame_raw_timeout(self, max_len, timeout)
    }
}
