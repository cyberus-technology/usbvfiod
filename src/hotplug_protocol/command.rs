use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use vmm_sys_util::errno::Error;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

const COMMAND_ATTACH: u8 = 0;
const COMMAND_DETACH: u8 = 1;
const COMMAND_LIST: u8 = 2;

#[derive(Debug)]
pub enum Command {
    Attach { bus: u8, device: u8, fd: File },
    Detach { bus: u8, device: u8 },
    List,
}

impl Command {
    pub fn send_over_socket(self, socket: &UnixStream) -> Result<(), CommandSendError> {
        let id = self.variant_to_id();
        let (buf, fd) = match &self {
            Command::Attach { bus, device, fd } => ([id, *bus, *device], Some(fd.as_raw_fd())),
            Command::Detach { bus, device } => ([id, *bus, *device], None),
            Command::List => ([id, 0, 0], None),
        };

        let transmitted = if let Some(fd) = fd {
            socket.send_with_fd(&buf[..], fd)
        } else {
            socket.send_with_fds(&[&buf[..]], &[])
        }?;

        // TODO implement a transmission loop to be safe (we should not run
        // into problems with how little data we send, though).
        if transmitted == buf.len() {
            Ok(())
        } else {
            Err(CommandSendError::NotSentEnough(buf.len(), transmitted))
        }
    }

    pub fn receive_from_socket(socket: &UnixStream) -> Result<Self, CommandReceiveError> {
        let mut buf = [0u8; 3];
        let (bytes_read, file) = socket.recv_with_fd(&mut buf[..])?;
        if bytes_read != buf.len() {
            return Err(CommandReceiveError::NotEnoughData(buf.len(), bytes_read));
        }
        match (buf[0], file) {
            (COMMAND_ATTACH, Some(file)) => Ok(Command::Attach {
                bus: buf[1],
                device: buf[2],
                fd: file,
            }),
            (COMMAND_ATTACH, None) => Err(CommandReceiveError::MissingFd),
            (COMMAND_DETACH, None) => Ok(Command::Detach {
                bus: buf[1],
                device: buf[2],
            }),
            (COMMAND_LIST, None) => Ok(Command::List {}),
            (command, None) => Err(CommandReceiveError::UnknownCommand(command)),
            (_, Some(_)) => Err(CommandReceiveError::UnexpectedFd),
        }
    }

    fn variant_to_id(&self) -> u8 {
        match self {
            Command::Attach {
                bus: _,
                device: _,
                fd: _,
            } => COMMAND_ATTACH,
            Command::Detach { bus: _, device: _ } => COMMAND_DETACH,
            Command::List => COMMAND_LIST,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum CommandReceiveError {
    #[error("did not receive enough data over the socket. Expected {0}, received {1}")]
    NotEnoughData(usize, usize),
    #[error("expected to receive a file descriptor, but there was none")]
    MissingFd,
    #[error("did not expect to receive a file descriptor, but there was one")]
    UnexpectedFd,
    #[error("Unknown command")]
    UnknownCommand(u8),
    #[error("Encountered errno during socket IO")]
    ErrnoError(#[from] Error),
}

#[derive(thiserror::Error, Debug)]
pub enum CommandSendError {
    #[error("did not receive enough data over the socket. Expected to send {0}, sent {1}")]
    NotSentEnough(usize, usize),
    #[error("Encountered errno during socket IO")]
    ErrnoError(#[from] Error),
}
