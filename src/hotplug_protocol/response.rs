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
