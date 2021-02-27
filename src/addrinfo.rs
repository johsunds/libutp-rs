use crate::Result;

use lazy_static::*;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::net::SocketAddr;
use std::ops::Deref;
use std::os::raw::c_int;
use std::sync::{Arc, RwLock};

#[cfg(windows)]
use winapi::{
    shared::ws2def::{ADDRINFOA as c_addrinfo, AF_INET, IPPROTO_UDP, SOCK_DGRAM},
    um::ws2tcpip::getaddrinfo as c_getaddrinfo,
};

#[cfg(unix)]
use libc::{
    addrinfo as c_addrinfo, getaddrinfo as c_getaddrinfo, AF_INET, IPPROTO_UDP, SOCK_DGRAM,
};

lazy_static! {
    pub static ref ADDRINFO_CACHE: Arc<RwLock<HashMap<SocketAddr, AddrInfo>>> = Default::default();
}

fn getaddrinfo_from_cache(addr: &SocketAddr) -> Option<AddrInfo> {
    ADDRINFO_CACHE.read().unwrap().get(addr).copied()
}

#[derive(Clone, Copy)]
pub struct AddrInfo(c_addrinfo);

impl Deref for AddrInfo {
    type Target = c_addrinfo;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

unsafe impl Send for AddrInfo {}

unsafe impl Sync for AddrInfo {}

// Needs to be called before getaddrinfo on windows, otherwise we get a WSANOTINITIALISED (10093) err
#[cfg(windows)]
fn wsa_startup() {
    use std::sync::Once;

    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let _ = mio::net::UdpSocket::bind("127.0.0.1:34254".parse().unwrap());
    });
}

fn socketaddr_to_raw(addr: SocketAddr) -> (CString, CString) {
    unsafe {
        let ip = CString::from_vec_unchecked(addr.ip().to_string().into_bytes());
        let port = CString::from_vec_unchecked(addr.port().to_string().into_bytes());
        (ip, port)
    }
}

pub fn getaddrinfo_from_std(addr: SocketAddr) -> Result<AddrInfo> {
    if let Some(ai) = getaddrinfo_from_cache(&addr) {
        return Ok(ai);
    }

    #[cfg(windows)]
    wsa_startup();

    let hints = c_addrinfo {
        ai_flags: 0,
        ai_family: AF_INET as c_int,
        ai_socktype: SOCK_DGRAM as c_int,
        ai_protocol: IPPROTO_UDP as c_int,
        ai_addrlen: 0,
        ai_canonname: std::ptr::null_mut(),
        ai_addr: std::ptr::null_mut(),
        ai_next: std::ptr::null_mut(),
    };
    let mut res: *mut c_addrinfo = std::ptr::null_mut();
    let (ip, port) = socketaddr_to_raw(addr);
    unsafe {
        match c_getaddrinfo(ip.as_ptr(), port.as_ptr(), &hints, &mut res) {
            0 => {
                ADDRINFO_CACHE.write().unwrap().insert(addr, AddrInfo(*res));
                Ok(AddrInfo(*res))
            }
            other => Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Failed to look up address information: getaddrinfo error code = {}",
                    other
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_addr_to_raw() {
        let (ip, port) = socketaddr_to_raw("127.0.0.1:0".parse().unwrap());
        assert_eq!(ip.into_string().unwrap(), "127.0.0.1");
        assert_eq!(port.into_string().unwrap(), "0");
    }
}
