use std::{
    convert::TryFrom,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    SuccessfulOperation,
    ListFollowing,
    NoFreePort,
    CouldNotDetermineSpeed,
    FailedToOpenFd,
    AlreadyAttached,
    NoSuchDevice,
    Invalid,
}

impl Response {
    pub fn send_over_socket(&self, socket: &mut UnixStream) -> Result<(), io::Error> {
        socket.write(&[*self as u8]).map(|_| ())
    }

    pub fn receive_from_socket(socket: &mut UnixStream) -> Result<Self, io::Error> {
        let mut buf = [0u8; 1];
        socket
            .read(&mut buf)
            .map(|_| Self::try_from(buf[0]).unwrap())
    }

    pub fn receive_devices_list(
        &self,
        socket: &mut UnixStream,
    ) -> Result<Vec<(u8, u8)>, io::Error> {
        assert_eq!(*self, Self::ListFollowing);

        let mut buf = [0u8; 1];
        socket.read_exact(&mut buf)?;
        // bus and device number take one byte each.
        let len = buf[0] * 2;
        let mut buf = vec![0u8; len as usize];

        socket.read_exact(&mut buf)?;

        let mut devices = vec![];
        let mut iter = buf.into_iter();

        // iter's length is a multiple of 2, so we always get either both values
        // or none.
        while let (Some(bus), Some(dev)) = (iter.next(), iter.next()) {
            devices.push((bus, dev));
        }

        Ok(devices)
    }
}

impl TryFrom<u8> for Response {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::SuccessfulOperation,
            1 => Self::ListFollowing,
            2 => Self::NoFreePort,
            3 => Self::CouldNotDetermineSpeed,
            4 => Self::FailedToOpenFd,
            5 => Self::AlreadyAttached,
            6 => Self::NoSuchDevice,
            _ => Self::Invalid,
        })
    }
}
