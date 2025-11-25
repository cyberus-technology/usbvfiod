use std::{
    fs::canonicalize,
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

pub fn resolve_path<P: AsRef<Path>>(path: P) -> Result<(u8, u8, PathBuf), ResolveError> {
    let canonical_path = canonicalize(path)?;
    let components = canonical_path.iter().collect::<Vec<_>>();
    if components.len() != 6
        || components[0] != "/"
        || components[1] != "dev"
        || components[2] != "bus"
        || components[3] != "usb"
    {
        return Err(ResolveError::UnexpectedPath(canonical_path));
    }
    let bus = components[4]
        .to_str()
        .and_then(|str| str.parse::<u8>().ok());
    let dev = components[5]
        .to_str()
        .and_then(|str| str.parse::<u8>().ok());

    if let (Some(bus), Some(dev)) = (bus, dev) {
        Ok((bus, dev, canonical_path))
    } else {
        Err(ResolveError::UnexpectedPath(canonical_path))
    }
}

#[derive(Error, Debug)]
pub enum ResolveError {
    #[error(transparent)]
    IoError(#[from] io::Error),
    #[error("Expected a path of (or symlink to) a USB device file (/dev/bus/usb/xxx/yyy), but received (symlink to) path {0}")]
    UnexpectedPath(PathBuf),
}
