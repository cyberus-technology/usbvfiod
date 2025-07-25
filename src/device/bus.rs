//! # Memory Bus
//!
//! This module implements a memory bus that is meant to be immutable
//! after creation. See [`Bus`] for a starting point.

use std::fmt::{Debug, Display, Formatter};
use std::{
    convert::{TryFrom, TryInto},
    error::Error,
    fmt,
    num::NonZeroU64,
    ops::Range,
    sync::Arc,
    vec::Vec,
};
use tracing::{debug, warn};

use crate::device::interval::Interval;

/// The size of bus requests.
///
/// We don't use plain integers here to prevent use with illegal
/// sizes. [`RequestSize`] can be converted from and to [`u64`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum RequestSize {
    Size1 = 1,
    Size2 = 2,
    Size4 = 4,
    Size8 = 8,
}

impl From<RequestSize> for u8 {
    fn from(r: RequestSize) -> Self {
        r as Self
    }
}

impl From<RequestSize> for u32 {
    fn from(r: RequestSize) -> Self {
        r as Self
    }
}

impl From<RequestSize> for u64 {
    fn from(r: RequestSize) -> Self {
        r as Self
    }
}

#[allow(clippy::fallible_impl_from)]
impl From<RequestSize> for NonZeroU64 {
    fn from(r: RequestSize) -> Self {
        // This cannot panic as all valid [RequestSize]s are > 0.
        Self::new(r as u64).unwrap()
    }
}

impl Display for RequestSize {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let val = u8::from(*self);
        write!(f, "{val}")
    }
}

/// An attempt was made to convert a size into a [`RequestSize`] that
/// cannot be represented.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct IllegalRequestSize {}

impl TryFrom<u32> for RequestSize {
    type Error = IllegalRequestSize;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        u64::from(value).try_into()
    }
}

impl TryFrom<usize> for RequestSize {
    type Error = IllegalRequestSize;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        u64::try_from(value)
            .map_err(|_| IllegalRequestSize {})?
            .try_into()
    }
}

impl TryFrom<u64> for RequestSize {
    type Error = IllegalRequestSize;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Size1),
            2 => Ok(Self::Size2),
            4 => Ok(Self::Size4),
            8 => Ok(Self::Size8),
            _ => Err(IllegalRequestSize {}),
        }
    }
}

/// The address-size pair for [`BusDevice`] read/write operations.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Request {
    /// The address of the request. What unit this address is in, is
    /// up to the user of this bus, but it is usually bytes.
    pub addr: u64,

    /// The size of this request.
    pub size: RequestSize,
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size: u64 = self.size.into();

        write!(f, "{:#016x}+{:x}", self.addr, size)
    }
}

impl Request {
    /// Create a new request from address and size.
    #[must_use]
    pub const fn new(addr: u64, size: RequestSize) -> Self {
        Self { addr, size }
    }

    /// Split a request into individual byte requests.
    pub fn iter_bytes(&self) -> impl Iterator<Item = Self> {
        (self.addr..self.addr + u64::from(self.size))
            .map(|addr| Self::new(addr, RequestSize::Size1))
    }
}

/// A request wrapped around the address space boundary.
#[derive(Debug, PartialEq, Eq)]
pub struct WrappingRequestError {}

impl fmt::Display for WrappingRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bus request wraps around")
    }
}

impl Error for WrappingRequestError {}

impl TryInto<Range<u64>> for Request {
    type Error = WrappingRequestError;

    fn try_into(self) -> Result<Range<u64>, Self::Error> {
        let size: u64 = self.size.into();

        Ok(self.addr..self.addr.checked_add(size).ok_or(WrappingRequestError {})?)
    }
}

/// A device in a memory bus. This receives read/write requests from
/// the memory bus.
///
/// Reads and writes are assumed to be atomic in the sense that a
/// multi-byte write should write everything in one go and a read
/// cannot observe partially updated memory.
///
/// The atomicity requirement is not necessary for bulk memory
/// operations.
pub trait BusDevice: Debug {
    /// Return the size of this device. The device has to respond to
    /// requests between `0` and `size - 1`.
    ///
    /// A Bus with a `device` attached at `offset` will forward all
    /// requests in the range `offset..(offset + device.size())` to `device`.
    fn size(&self) -> u64;

    /// Read a piece of memory from the bus.
    ///
    /// The read happens atomically, such that it reads either the old
    /// or the new value, if a write happened to the same memory
    /// location concurrently.
    fn read(&self, req: Request) -> u64;

    /// Write memory to the bus.
    ///
    /// The write happens atomically with respect to other accesses,
    /// i.e. a read cannot see partial updates.
    fn write(&self, req: Request, value: u64);

    /// Read large amounts of data from the bus.
    ///
    /// Bulk reads are not atomic and can interleave with writes that
    /// happen concurrently.
    ///
    /// **Note:** The default implementation is not efficient and
    /// should not be used where performance matters.
    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        for (cur, value) in data.iter_mut().enumerate() {
            // This conversion cannot panic, because usize always fits
            // into u64.
            let cur_u64: u64 = cur.try_into().expect("Failed to convert to u64");

            // The case where we read past the wrapping point is not
            // particularly useful, but there is also no harm in
            // handling it here.
            //
            // See also write_bulk.
            let read_value = self.read(Request::new(
                offset.overflowing_add(cur_u64).0,
                RequestSize::Size1,
            ));

            // This lossy conversion is safe, because only the low 8
            // bits contain useful data.
            *value = read_value as u8;
        }
    }

    /// Write large amounts of data to the bus.
    ///
    /// Bulk writes are not atomic and reads can see intermediate
    /// states.
    ///
    /// **Note:** The default implementation is not efficient and
    /// should not be used where performance matters.
    fn write_bulk(&self, offset: u64, data: &[u8]) {
        for (cur, &value) in data.iter().enumerate() {
            // This conversion cannot panic, because usize always fits
            // into u64.
            let cur_u64: u64 = cur.try_into().expect("Failed to convert to u64");

            // The case where we read past the wrapping point is not
            // particularly useful, but there is also no harm in
            // handling it here.
            //
            // See also read_bulk.
            self.write(
                Request::new(offset.overflowing_add(cur_u64).0, RequestSize::Size1),
                value.into(),
            )
        }
    }

    /// Compare and exchange a value atomically.
    ///
    /// Some [`BusDevice`] implementations might have an efficient implementation
    /// for this. The default implementation issues a warning and falls back
    /// to non-atomic read/write cycles. If you hit this warning, you should
    /// implement compare_exchange on your [`BusDevice`].
    ///
    /// The access width of the operation is encoded in the `size`-field of a [`Request`].
    ///
    /// See [`std::sync::atomic::AtomicU64::compare_exchange`].
    fn compare_exchange_request(&self, req: Request, current: u64, new: u64) -> Result<u64, u64> {
        warn!(
            "Atomic compare-exchange executed non-atomically for access to {:016x}",
            req.addr
        );
        let old = self.read(req);
        if old == current {
            self.write(req, new);
            Ok(current)
        } else {
            Err(old)
        }
    }
}

/// A version of [`BusDevice`] that does not mandate thread-safety.
///
/// This trait is meant for devices that are simple enough that they
/// don't want to care about their own thread safety and are fine when
/// they get wrapped into a [`std::sync::Mutex`].
pub trait SingleThreadedBusDevice {
    /// See [`BusDevice::size`].
    fn size(&self) -> u64;

    /// See [`BusDevice::read`].
    fn read(&mut self, req: Request) -> u64;

    /// See [`BusDevice::write`].
    fn write(&mut self, req: Request, value: u64);
}

/// Each [`SingleThreadedBusDevice`] can be easily wrapped into a mutex to
/// become a normal [`BusDevice`].
impl<T: SingleThreadedBusDevice + Debug + Send> BusDevice for std::sync::Mutex<T> {
    fn size(&self) -> u64 {
        self.lock().unwrap().size()
    }

    fn write(&self, req: Request, value: u64) {
        self.lock().unwrap().write(req, value)
    }

    fn read(&self, req: Request) -> u64 {
        self.lock().unwrap().read(req)
    }
}

/// The bus device that handles the case where no one wants to answer
/// a request. This device is used when a bus is constructed with
/// [`Bus::new()`].
///
/// The usual semantics is to return all bits set for reads and ignore
/// writes.
#[derive(Debug, Clone, Default)]
pub struct DefaultDevice {
    /// The size of the default device in bytes.
    size: u64,
    name: &'static str,
}

impl DefaultDevice {
    /// Construct a default device that spans the complete address space.
    #[must_use]
    #[allow(unused)]
    pub const fn new(name: &'static str) -> Self {
        Self {
            size: u64::MAX,
            name,
        }
    }

    /// Construct a default device that spans a specific size in bytes.
    #[must_use]
    pub const fn new_with_size(name: &'static str, size: u64) -> Self {
        Self { size, name }
    }
}

impl BusDevice for DefaultDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn write(&self, req: Request, v: u64) {
        debug!(
            "Ignored {} write: {:#016x}+{:x} <- {:#016x}",
            self.name,
            req.addr,
            u64::from(req.size),
            v
        );
    }

    /// Return a "all-bits-set" value for the given request size.
    fn read(&self, req: Request) -> u64 {
        let bytes: u8 = req.size.into();
        let empty_bits = u64::BITS - u8::BITS * u32::from(bytes);

        debug!(
            // The extra space aligns the output with the
            // corresponding write debug log.
            "Ignored {} read:  {:#016x}+{:x}",
            self.name,
            req.addr,
            u64::from(req.size)
        );

        !0 >> empty_bits
    }
}

/// A reference-counting and thread-safe pointer to a generic bus
/// device.
pub type BusDeviceRef = Arc<dyn BusDevice + Send + Sync>;

#[derive(Clone, Debug)]
struct DeviceEntry {
    range: Range<u64>,
    device: BusDeviceRef,
}

/// A memory bus implementation.
///
/// The bus looks to the outside like a [`BusDevice`], but will multiplex
/// incoming requests to the devices that are added to it.  The idea is
/// that busses can be stacked on top of each other and are immutable
/// after an initial construction phase.
///
/// **Note:** To simplify implementation, we've made the choice to not
/// split requests when they match multiple devices, but treat them as
/// non-matching requests.
#[derive(Clone, Debug)]
pub struct Bus {
    /// A vector of device together with the range they claim. When we
    /// add devices, we make sure there is no overlap.
    devices: Vec<DeviceEntry>,

    /// This device handles any "weird" requests that are not claimed
    /// by any device and also should not be passed on.
    error_device: DefaultDevice,

    /// Any request that was valid but is not claimed ends up being
    /// forwarded here.
    default: BusDeviceRef,
}

/// An error that is thrown when a device could not be added to a bus.
#[derive(Debug, PartialEq, Eq)]
pub enum AddBusDeviceError {
    /// The new device overlaps an existing one.
    OverlapsExistingDevice {
        /// The range that already existed on the bus.
        existing_range: Range<u64>,

        /// The range that was attempted to be added.
        added_range: Range<u64>,
    },
    /// The new device overflows the bounds of the bus.
    DeviceOutOfRange {
        /// The size of the bus that was too small to add a new device to.
        bus_size: u64,

        /// The range that was attempted to be added.
        added_range: Range<u64>,
    },
}

impl fmt::Display for AddBusDeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverlapsExistingDevice {
                existing_range,
                added_range,
            } => write!(
                f,
                "New device for {:x}-{:x} overlaps existing device at {:x}-{:x}",
                added_range.start, added_range.end, existing_range.start, existing_range.end,
            ),
            Self::DeviceOutOfRange {
                bus_size,
                added_range,
            } => write!(
                f,
                "New device for {:x}-{:x} overflows size of bus {:x}",
                added_range.start, added_range.end, bus_size,
            ),
        }
    }
}

impl Error for AddBusDeviceError {}

impl Default for Bus {
    fn default() -> Self {
        Self::new("<unnamed>", u64::MAX)
    }
}

/// Information for a single bulk request to a specific device.
///
/// See [`Bus::iter_bulk_request`].
#[derive(Debug, Clone)]
struct BulkRequestChunk<'a> {
    /// The device to perform the bulk request on.
    device: &'a dyn BusDevice,

    /// The offset of the bulk request relative to the address range that the device claims.
    device_offset: u64,

    /// The range in the original data slice.
    data_range: Range<usize>,
}

/// An iterator to split bulk requests.
///
/// See [`Bus::iter_bulk_request`].
#[derive(Debug)]
struct BulkRequestIterator<'a> {
    bus: &'a Bus,

    /// The start of the bulk request as bus address.
    request_start: u64,

    /// The (non-inclusive) end of the bulk request as a bus address.
    request_end: u64,

    /// The current position of the iterator in the request as a bus address.
    cur_offset: u64,
}

impl<'a> BulkRequestIterator<'a> {
    fn new(bus: &'a Bus, offset: u64, slice: &[u8]) -> Self {
        Self {
            bus,
            request_start: offset,
            request_end: offset + u64::try_from(slice.len()).unwrap(),
            cur_offset: offset,
        }
    }
}

impl<'a> Iterator for BulkRequestIterator<'a> {
    type Item = BulkRequestChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        assert!(self.cur_offset >= self.request_start && self.cur_offset <= self.request_end);

        (self.cur_offset < self.request_end).then(|| {
            // The offset inside the data slice.
            let data_offset: usize = (self.cur_offset - self.request_start).try_into().unwrap();

            // The amount of space in the data slice starting from data_offset.
            let remaining_data_size: usize =
                (self.request_end - self.request_start - u64::try_from(data_offset).unwrap())
                    .try_into()
                    .unwrap();
            assert!(remaining_data_size > 0);

            let chunk = if let Some(entry) = self
                .bus
                .devices
                .iter()
                .find(|entry| entry.range.contains(&self.cur_offset))
            {
                let device_offset = self.cur_offset - entry.range.start;
                let chunk_size = usize::min(
                    remaining_data_size,
                    (entry.range.end - entry.range.start - device_offset)
                        .try_into()
                        .unwrap(),
                );
                BulkRequestChunk {
                    device: entry.device.as_ref(),
                    device_offset,
                    data_range: data_offset..(data_offset + chunk_size),
                }
            } else {
                // If no device matches, we fall back to byte-to-byte accesses to the default device.
                //
                // Instead of byte-by-byte accesses we could use bulk requests for the "gap" to the
                // next actual device. But as this is not a performance-critical code path, we do it
                // the simple way.
                BulkRequestChunk {
                    device: self.bus.default.as_ref(),
                    device_offset: self.cur_offset,
                    data_range: data_offset..(data_offset + 1),
                }
            };

            let chunk_size = chunk.data_range.end - chunk.data_range.start;
            assert!(chunk_size > 0);

            self.cur_offset += u64::try_from(chunk_size).unwrap();
            chunk
        })
    }
}

impl<'a> Bus {
    /// Construct a new bus with a custom default handler.
    #[must_use]
    pub fn new_with_default(name: &'static str, default_device: BusDeviceRef) -> Self {
        Self {
            devices: Default::default(),
            error_device: DefaultDevice::new_with_size(name, default_device.size()),
            default: default_device,
        }
    }

    /// Construct a new bus with the standard default handler.
    ///
    /// See [`DefaultDevice`] for a description of how it handles
    /// requests that are not claimed by other devices.
    #[must_use]
    pub fn new(name: &'static str, size: u64) -> Self {
        Self::new_with_default(name, Arc::new(DefaultDevice::new_with_size(name, size)))
    }

    /// Add a new item to the bus that claims the given range of
    /// addresses.
    pub fn add(&mut self, start_addr: u64, device: BusDeviceRef) -> Result<(), AddBusDeviceError> {
        let range = start_addr..start_addr.checked_add(device.size()).ok_or_else(|| {
            AddBusDeviceError::DeviceOutOfRange {
                bus_size: self.size(),
                added_range: start_addr..start_addr.overflowing_add(device.size()).0,
            }
        })?;
        if range.end > self.size() {
            Err(AddBusDeviceError::DeviceOutOfRange {
                bus_size: self.size(),
                added_range: range,
            })
        } else if let Some(overlap) = self.devices.iter().find(|e| e.range.overlaps(&range)) {
            Err(AddBusDeviceError::OverlapsExistingDevice {
                existing_range: overlap.range.clone(),
                added_range: range,
            })
        } else {
            self.devices.push(DeviceEntry { range, device });
            Ok(())
        }
    }

    /// Try to find a device that can handle this request.
    ///
    /// We return a transformed request (relative to the device's
    /// claimed region) and a reference to the device itself.
    fn to_device_request(&'a self, req: Request) -> Option<(Request, &'a dyn BusDevice)> {
        let req_range: Range<u64> = req.try_into().ok()?;

        for entry in &self.devices {
            // If a device fully claims the request, we have found
            // what we came for.
            if entry.range.contains_interval(&req_range) {
                return Some((
                    Request {
                        addr: req.addr - entry.range.start,
                        ..req
                    },
                    entry.device.as_ref(),
                ));
            }

            // If a device partially claims the request, we consider
            // this weird and let the error handler deal with this.
            if entry.range.overlaps(&req_range) {
                return Some((req, &self.error_device));
            }
        }

        None
    }

    /// Create an iterator that iterates over all chunks of a bulk request.
    ///
    /// Each element yielded by the iterator is one bulk request that can be made to a specific
    /// device.
    fn iter_bulk_request(
        &'a self,
        offset: u64,
        slice: &[u8],
    ) -> impl Iterator<Item = BulkRequestChunk<'a>> {
        BulkRequestIterator::new(self, offset, slice)
    }
}

impl BusDevice for Bus {
    fn size(&self) -> u64 {
        self.default.size()
    }

    fn write(&self, req: Request, value: u64) {
        match self.to_device_request(req) {
            Option::Some((rel_req, device)) => device.write(rel_req, value),
            None => self.default.write(req, value),
        }
    }

    fn read(&self, req: Request) -> u64 {
        match self.to_device_request(req) {
            Option::Some((rel_req, device)) => device.read(rel_req),
            None => self.default.read(req),
        }
    }

    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        self.iter_bulk_request(offset, data).for_each(|breq| {
            breq.device
                .read_bulk(breq.device_offset, &mut data[breq.data_range])
        });
    }

    fn write_bulk(&self, offset: u64, data: &[u8]) {
        self.iter_bulk_request(offset, data).for_each(|breq| {
            breq.device
                .write_bulk(breq.device_offset, &data[breq.data_range])
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Mutex,
    };

    use super::*;
    use proptest::prelude::*;

    impl Arbitrary for RequestSize {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            Strategy::boxed(prop_oneof![
                Just(Self::Size1),
                Just(Self::Size2),
                Just(Self::Size4),
                Just(Self::Size8),
            ])
        }
    }

    #[test]
    fn invalid_sizes_are_not_converted_to_request_size() {
        for invalid_size in [0, 3, 7, 300, u64::MAX] {
            assert_eq!(
                RequestSize::try_from(invalid_size),
                Err(IllegalRequestSize {})
            );
        }
    }

    proptest! {
        #[test]
        fn request_sizes_to_integer_and_back_conversion_is_identity(rs: RequestSize) {
            assert_eq!(u64::from(rs).try_into(), Ok(rs));
        }
    }

    #[test]
    fn requests_convert_into_ranges() -> Result<(), WrappingRequestError> {
        let request = Request {
            addr: 0x17,
            size: RequestSize::Size2,
        };
        let request_range: Range<u64> = request.try_into()?;

        assert_eq!(request_range, 0x17..0x19);

        Ok(())
    }

    #[test]
    fn wrapping_requests_are_rejected() {
        let request = Request {
            addr: 0xffff_ffff_ffff_ffff,
            size: RequestSize::Size2,
        };

        let err_range: Result<Range<u64>, _> = request.try_into();

        assert_eq!(err_range, Err(WrappingRequestError {}));
    }

    #[test]
    fn request_byte_iterator_works() {
        let request = Request {
            addr: 0x100,
            size: RequestSize::Size2,
        };

        let split_request = request.iter_bytes().collect::<Vec<_>>();
        let addresses = split_request.iter().map(|r| r.addr).collect::<Vec<_>>();
        let sizes = split_request.iter().map(|r| r.size).collect::<Vec<_>>();

        assert_eq!(addresses, vec![0x100, 0x101]);
        assert!(sizes.iter().all(|&s| s == RequestSize::Size1));
    }

    #[test]
    fn default_device_responds_with_pci_semantics() {
        let def = DefaultDevice::new("test");

        assert_eq!(def.read(Request::new(0, RequestSize::Size1)), 0xFF);
        assert_eq!(def.read(Request::new(0, RequestSize::Size2)), 0xFFFF);
        assert_eq!(def.read(Request::new(0, RequestSize::Size4)), 0xFFFF_FFFF);
        assert_eq!(
            def.read(Request::new(0, RequestSize::Size8)),
            0xFFFF_FFFF_FFFF_FFFF
        );
    }

    #[test]
    fn unmatched_requests_are_handled_by_default() {
        let bus = Bus::default();

        assert_eq!(bus.read(Request::new(17, RequestSize::Size1)), 0xFF);
    }

    /// A device that returns a constant value for all read requests
    /// and expects all writes to have that value as well.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ConstDevice {
        value: u64,
        size: u64,
    }

    impl BusDevice for ConstDevice {
        fn size(&self) -> u64 {
            self.size
        }

        fn write(&self, _: Request, value: u64) {
            assert_eq!(value, self.value)
        }

        fn read(&self, _: Request) -> u64 {
            self.value
        }
    }

    #[test]
    fn bus_multiplexes_to_correct_device() -> Result<(), AddBusDeviceError> {
        let mut bus = Bus::new_with_default(
            "test",
            Arc::new(ConstDevice {
                value: 3,
                size: u64::MAX,
            }),
        );

        bus.add(10, Arc::new(ConstDevice { value: 1, size: 10 }))?;
        bus.add(20, Arc::new(ConstDevice { value: 2, size: 10 }))?;

        assert_eq!(bus.read(Request::new(15, RequestSize::Size1)), 1);
        assert_eq!(bus.read(Request::new(25, RequestSize::Size1)), 2);

        // Split requests are handled with error semantics.
        assert_eq!(bus.read(Request::new(19, RequestSize::Size2)), 0xFFFF);
        assert_eq!(bus.read(Request::new(29, RequestSize::Size2)), 0xFFFF);

        // Unmatched requests are forwarded.
        assert_eq!(bus.read(Request::new(5, RequestSize::Size1)), 3);
        assert_eq!(bus.read(Request::new(35, RequestSize::Size1)), 3);

        Ok(())
    }

    #[test]
    fn bulk_reads_are_like_multiple_byte_reads() -> Result<(), AddBusDeviceError> {
        let mut bus = Bus::default();

        bus.add(10, Arc::new(ConstDevice { value: 1, size: 10 }))?;
        bus.add(20, Arc::new(ConstDevice { value: 2, size: 10 }))?;

        let mut data = vec![0, 0];
        bus.read_bulk(19, &mut data);

        assert_eq!(
            data,
            vec![
                bus.read(Request::new(19, RequestSize::Size1)) as u8,
                bus.read(Request::new(20, RequestSize::Size1)) as u8
            ]
        );

        Ok(())
    }

    /// A device that only accepts bulk reads and writes.
    #[derive(Debug)]
    struct BulkOnlyDevice {
        data: Mutex<Vec<u8>>,
    }

    impl BulkOnlyDevice {
        fn new(data: &[u8]) -> Self {
            Self {
                data: Mutex::new(data.to_vec()),
            }
        }
    }

    impl BusDevice for BulkOnlyDevice {
        fn size(&self) -> u64 {
            self.data.lock().unwrap().len().try_into().unwrap()
        }

        fn read(&self, _req: Request) -> u64 {
            panic!("Must not call byte read on this device")
        }

        fn write(&self, _req: Request, _value: u64) {
            panic!("Must not call byte write on this device")
        }

        fn read_bulk(&self, offset: u64, data: &mut [u8]) {
            let offset: usize = offset.try_into().unwrap();
            data.copy_from_slice(&self.data.lock().unwrap()[offset..(offset + data.len())])
        }

        fn write_bulk(&self, offset: u64, data: &[u8]) {
            let offset: usize = offset.try_into().unwrap();
            self.data.lock().unwrap()[offset..(offset + data.len())].copy_from_slice(data)
        }
    }

    #[test]
    fn bulk_reads_are_bulk_reads_on_devices() -> Result<(), AddBusDeviceError> {
        let mut bus = Bus::default();
        let device_1 = Arc::new(BulkOnlyDevice::new(&[3, 4, 5]));
        let device_2 = Arc::new(BulkOnlyDevice::new(&[7, 8]));

        bus.add(3, device_1)?;
        bus.add(7, device_2)?;

        // Read inside a single device.
        let mut data = [0; 2];
        bus.read_bulk(4, &mut data);
        assert_eq!(data, [4, 5]);

        // Read over multiple devices.
        let mut data = [0; 8];
        bus.read_bulk(2, &mut data);
        assert_eq!(data, [0xff, 3, 4, 5, 0xff, 7, 8, 0xff]);

        // Read outside of any device.
        let mut data = [0; 2];
        bus.read_bulk(1234, &mut data);
        assert_eq!(data, [0xff, 0xff]);

        Ok(())
    }

    #[test]
    fn bulk_writes_are_bulk_writes_on_devices() -> Result<(), AddBusDeviceError> {
        let mut bus = Bus::default();
        let device_1 = Arc::new(BulkOnlyDevice::new(&[3, 4, 5]));
        let device_2 = Arc::new(BulkOnlyDevice::new(&[7, 8]));

        bus.add(3, device_1.clone())?;
        bus.add(7, device_2.clone())?;

        // Write to a single device.
        let data = [24, 25];
        bus.write_bulk(4, &data);
        assert_eq!(device_1.data.lock().unwrap().as_slice(), [3, 24, 25]);
        assert_eq!(device_2.data.lock().unwrap().as_slice(), [7, 8]);

        // Write over multiple devices.
        let data = [12, 13, 14, 15, 16, 17, 18, 19, 20];
        bus.write_bulk(2, &data);
        assert_eq!(device_1.data.lock().unwrap().as_slice(), [13, 14, 15]);
        assert_eq!(device_2.data.lock().unwrap().as_slice(), [17, 18]);

        Ok(())
    }

    /// A device that asserts all read and write requests are
    /// for a configured address. It returns constant 0 on read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct AddressCheckDevice {
        expected_address: u64,
        size: u64,
    }

    impl BusDevice for AddressCheckDevice {
        fn size(&self) -> u64 {
            self.size
        }

        fn write(&self, req: Request, _: u64) {
            assert!(req.addr == self.expected_address)
        }

        fn read(&self, req: Request) -> u64 {
            assert!(req.addr == self.expected_address);
            0
        }
    }

    #[test]
    fn devices_receive_relative_addresses() -> Result<(), AddBusDeviceError> {
        let mut bus = Bus::default();
        let req = Request::new(15, RequestSize::Size1);

        bus.add(
            10,
            Arc::new(AddressCheckDevice {
                expected_address: 5,
                size: 10,
            }),
        )?;

        assert_eq!(bus.read(req), 0);
        bus.write(req, 0);

        Ok(())
    }

    #[test]
    fn busses_can_be_stacked() -> Result<(), AddBusDeviceError> {
        let mut device_bus = Bus::default();

        device_bus.add(10, Arc::new(ConstDevice { value: 1, size: 10 }))?;

        let master_bus = Bus::new_with_default("test", Arc::new(device_bus));

        assert_eq!(master_bus.read(Request::new(10, RequestSize::Size1)), 1);

        Ok(())
    }

    #[test]
    fn overlapping_devices_are_rejected() -> Result<(), AddBusDeviceError> {
        let mut device_bus = Bus::default();
        let some_device = Arc::new(ConstDevice { value: 1, size: 10 });

        device_bus.add(10, some_device)?;

        assert_eq!(
            device_bus.add(12, Arc::new(ConstDevice { value: 1, size: 2 })),
            Err(AddBusDeviceError::OverlapsExistingDevice {
                existing_range: 10..20,
                added_range: 12..14,
            })
        );

        Ok(())
    }

    #[test]
    fn devices_cannot_be_attached_out_of_range() -> Result<(), AddBusDeviceError> {
        let mut device_bus = Bus::new("test", 32);
        let some_device = Arc::new(ConstDevice { value: 1, size: 10 });

        assert_eq!(
            device_bus.add(30, some_device),
            Err(AddBusDeviceError::DeviceOutOfRange {
                bus_size: 32,
                added_range: 30..40,
            })
        );

        Ok(())
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn devices_overflowing_the_u64_range_are_rejected() -> Result<(), AddBusDeviceError> {
        let mut device_bus = Bus::default();
        let some_device = Arc::new(ConstDevice { value: 1, size: 10 });

        assert_eq!(
            device_bus.add(u64::MAX, some_device),
            Err(AddBusDeviceError::DeviceOutOfRange {
                bus_size: u64::MAX,
                // This malformed empty range is intentional.
                added_range: u64::MAX..9,
            })
        );

        Ok(())
    }

    /// A "device" that stores and returns the last value written.
    impl BusDevice for Arc<AtomicU64> {
        fn size(&self) -> u64 {
            0
        }

        fn read(&self, req: Request) -> u64 {
            let res = self.load(SeqCst);
            match req.size {
                RequestSize::Size8 => res,
                RequestSize::Size4 => res as u32 as u64,
                RequestSize::Size2 => res as u16 as u64,
                RequestSize::Size1 => res as u8 as u64,
            }
        }

        fn write(&self, req: Request, value: u64) {
            let value = match req.size {
                RequestSize::Size8 => value,
                RequestSize::Size4 => value as u32 as u64,
                RequestSize::Size2 => value as u16 as u64,
                RequestSize::Size1 => value as u8 as u64,
            };
            self.store(value, SeqCst);
        }

        // Note: we deliberately do not implement compare_exchange
        // to test the default implementation.
    }

    #[test]
    fn compare_exchange_returns_ok_current_on_correct_value_and_updates() {
        let current: u64 = 0x0123_4567_89ab_cdef;
        let device = Arc::new(AtomicU64::new(current));
        let addr = 0;

        // Successful cmpxchg returns the supplied current value for 64 bit
        let new = current as u32 as u64;
        assert_eq!(
            device.compare_exchange_request(
                Request {
                    addr,
                    size: RequestSize::Size8
                },
                current,
                new
            ),
            Ok(current)
        );

        // Successful cmpxchg returns the supplied current value for 32 bit
        let current = new;
        let new = current as u16 as u64;
        assert_eq!(
            device.compare_exchange_request(
                Request {
                    addr,
                    size: RequestSize::Size4
                },
                current,
                new
            ),
            Ok(current)
        );

        // Value got updated
        assert_eq!(device.load(SeqCst), new);
    }

    #[test]
    fn compare_exchange_returns_err_current_on_wrong_value_and_bails() {
        let current: u64 = 0x0123_4567_89ab_cdef;
        let device = Arc::new(AtomicU64::new(current));
        let addr = 0;

        // Unsuccessful cmpxchg returns the actual current value as Err for 64 bit
        assert_eq!(
            device.compare_exchange_request(
                Request {
                    addr,
                    size: RequestSize::Size8
                },
                !current,
                0
            ),
            Err(current)
        );

        // Unsuccessful cmpxchg returns the actual current value as Err for 32 bit
        assert_eq!(
            device.compare_exchange_request(
                Request {
                    addr,
                    size: RequestSize::Size4
                },
                !current,
                0
            ),
            Err(current as u32 as u64)
        );

        // The value did not get updated
        assert_eq!(device.load(SeqCst), current);
    }
}
