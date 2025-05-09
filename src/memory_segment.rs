//! Provide a [`BusDevice`] abstraction over a piece of a mmap'able
//! file.

use std::{
    fs::File,
    sync::{
        atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
};

use memmap2::{Mmap, MmapMut, MmapOptions};
use usbvfiod::device::bus::{BusDevice, Request, RequestSize};
use vfio_user::DmaMapFlags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessRights {
    ReadOnly,
    ReadWrite,
}

impl TryFrom<DmaMapFlags> for AccessRights {
    type Error = ();

    fn try_from(value: DmaMapFlags) -> Result<Self, Self::Error> {
        match value {
            DmaMapFlags::READ_ONLY => Ok(AccessRights::ReadOnly),
            DmaMapFlags::READ_WRITE => Ok(AccessRights::ReadWrite),
            unsupported => todo!("unsupported DmaMapFlags {unsupported:?}"),
        }
    }
}

#[derive(Debug)]
enum Mapping {
    ReadOnly(Mmap),
    ReadWrite(MmapMut),
}

impl Mapping {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Mapping::ReadOnly(map) => map.as_ptr(),
            Mapping::ReadWrite(map) => map.as_ptr(),
        }
    }

    fn is_writable(&self) -> bool {
        match self {
            Mapping::ReadWrite(_) => true,
            Mapping::ReadOnly(_) => false,
        }
    }
}

/// A contiguous piece of mmap'ed memory.
#[derive(Debug, Clone)]
pub struct MemorySegment {
    size: u64,
    mapping: Arc<Mapping>,
}

impl MemorySegment {
    /// Creates a memory segment from a file.
    ///
    /// The file is only used for mapping and will not be read or
    /// written to.
    pub fn new_from_fd(
        fd: &File,
        file_offset: u64,
        size: u64,
        access_rights: AccessRights,
    ) -> Result<Self, std::io::Error> {
        Ok(MemorySegment {
            size,
            mapping: Arc::new({
                let mut mmap = MmapOptions::new();

                mmap.len(size.try_into().unwrap());
                mmap.offset(file_offset);

                match access_rights {
                    // SAFETY: We only access mmap'ed memory via atomics, so the warnings
                    // around UB in the Mmap and MmapMut documentation do not apply.
                    AccessRights::ReadOnly => unsafe { Mapping::ReadOnly(mmap.map(fd)?) },

                    // SAFETY: See above.
                    AccessRights::ReadWrite => unsafe { Mapping::ReadWrite(mmap.map_mut(fd)?) },
                }
            }),
        })
    }
}

impl BusDevice for MemorySegment {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, req: Request) -> u64 {
        assert!(req.addr.checked_add(req.size.into()).unwrap() <= self.size);

        // SAFETY: We check whether the request fits into the memory region above.
        let ptr = unsafe { self.mapping.as_ptr().add(req.addr.try_into().unwrap()) };

        match req.size {
            RequestSize::Size1 => {
                // SAFETY:
                //
                // We make sure all accesses to the memory happen via
                // atomics, because the pointer never escapes from
                // MemorySegment. We also ensure above that the
                // pointer points to valid memory.
                let atomic = unsafe { &*(ptr as *const AtomicU8) };

                atomic.load(Ordering::Relaxed).into()
            }
            RequestSize::Size2 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU16) };

                atomic.load(Ordering::Relaxed).into()
            }
            RequestSize::Size4 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU32) };

                atomic.load(Ordering::Relaxed).into()
            }
            RequestSize::Size8 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU64) };

                atomic.load(Ordering::Relaxed)
            }
        }
    }

    fn write(&self, req: Request, value: u64) {
        assert!(req.addr.checked_add(req.size.into()).unwrap() <= self.size);

        if !self.mapping.is_writable() {
            return;
        }

        // SAFETY: We check whether the request fits into the memory region above.
        let ptr = unsafe { self.mapping.as_ptr().add(req.addr.try_into().unwrap()) };

        match req.size {
            RequestSize::Size1 => {
                // SAFETY:
                //
                // We make sure all accesses to the memory happen via
                // atomics, because the pointer never escapes from
                // MemorySegment. We also ensure above that the
                // pointer points to valid memory.
                let atomic = unsafe { &*(ptr as *const AtomicU8) };

                atomic.store(value as u8, Ordering::Relaxed);
            }
            RequestSize::Size2 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU16) };

                atomic.store(value as u16, Ordering::Relaxed);
            }
            RequestSize::Size4 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU32) };

                atomic.store(value as u32, Ordering::Relaxed);
            }
            RequestSize::Size8 => {
                // SAFETY: See above.
                let atomic = unsafe { &*(ptr as *const AtomicU64) };

                atomic.store(value, Ordering::Relaxed)
            }
        }
    }

    // TODO Implement read_bulk/write_bulk for efficiency.
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::CString,
        io::{Read, Seek},
        os::fd::FromRawFd,
    };

    use super::*;

    fn create_memfd(size: u64) -> Result<File, std::io::Error> {
        let fd = unsafe { libc::memfd_create(CString::new("unittest").unwrap().as_ptr(), 0) };

        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: fd is a valid file descriptor, because we created it above.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size)?;

        Ok(file)
    }

    #[test]
    fn can_read_write() -> Result<(), std::io::Error> {
        let memfd = create_memfd(0x1000)?;
        let mseg = MemorySegment::new_from_fd(&memfd, 0, 0x1000, AccessRights::ReadWrite)?;

        assert_eq!(mseg.read(Request::new(0, RequestSize::Size8)), 0);

        mseg.write(Request::new(0, RequestSize::Size8), 0xcafed00dfeedface);
        assert_eq!(
            mseg.read(Request::new(0, RequestSize::Size8)),
            0xcafed00dfeedface
        );

        Ok(())
    }

    #[test]
    fn cant_write_to_read_only() -> Result<(), std::io::Error> {
        let memfd = create_memfd(0x1000)?;
        let mseg = MemorySegment::new_from_fd(&memfd, 0, 0x1000, AccessRights::ReadOnly)?;

        mseg.write(Request::new(0, RequestSize::Size8), 0xcafed00dfeedface);
        assert_eq!(mseg.read(Request::new(0, RequestSize::Size8)), 0);

        Ok(())
    }

    #[test]
    fn file_offset_is_respected() -> Result<(), std::io::Error> {
        let mut memfd = create_memfd(0x2000)?;
        let mseg = MemorySegment::new_from_fd(&memfd, 0x1000, 0x1000, AccessRights::ReadWrite)?;

        let data = 0xcafed00dfeedface_u64.to_le_bytes();

        mseg.write(
            Request::new(0x10, RequestSize::Size8),
            u64::from_le_bytes(data),
        );

        let mut check_data = [0; 8];
        memfd.seek(std::io::SeekFrom::Start(0x1010))?;
        memfd.read_exact(&mut check_data)?;

        assert_eq!(data, check_data);

        Ok(())
    }
}
