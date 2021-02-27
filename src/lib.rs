//! An async interface to [libutp](https://github.com/bittorrent/libutp).
//!
//!
//! The main interface of this crate is through the [`UtpContext`], [`UtpSocket`] and [`UtpListener`] structs.
//!
//! In addition to this, the [`wrappers`] module exposes the unsafe lower level API that the crate is built on,
//! which is more in line with the original [libutp](https://github.com/bittorrent/libutp) interface.
//!
//! **CAUTION**: *Use at your own risk! This crate is a best-effort attempt to provide a safe interface,
//! but FFI is inherently unsafe and I (the author) do not have expert knowledge of unsafe rust nor
//! the [libutp](https://github.com/bittorrent/libutp) codebase.*

mod addrinfo;
mod thread;
mod udp;
mod utp;

#[allow(clippy::missing_safety_doc)]
pub mod wrappers;

pub use utp::Connect;
pub use utp::UtpContext;
pub use utp::UtpListener;
pub use utp::UtpSocket;

use std::io;

pub type Result<T> = io::Result<T>;

#[cfg(test)]
mod test_utils {
    use std::net::SocketAddr;

    pub fn get_free_socketaddr() -> SocketAddr {
        use std::net::{IpAddr, Ipv4Addr};
        portpicker::pick_unused_port().unwrap();
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            portpicker::pick_unused_port().unwrap(),
        )
    }
}
