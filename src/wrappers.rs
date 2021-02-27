//! Thin wrappers around the original [libutp](https://github.com/bittorrent/libutp) interface
//!
//! [`UtpContext`] and [`UtpSocket`] are stateless wrappers around their
//! [libutp](https://github.com/bittorrent/libutp) counterparts [`utp_context`] and [`utp_socket`]
//! and provide more convenient interfaces to them.
//!
//! [`UtpContextHandle`] and [`UtpSocketHandle`] represent "ownership" of a [`UtpContext`] and [`UtpSocket`],
//! meaning that the underlying libutp entity will be cleaned up when they are dropped.

use crate::addrinfo::*;
use crate::Result;
use libutp_sys::*;

use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

type UtpCallback<C, S> = Box<(dyn for<'r> FnMut(UtpCallbackArgs<'r, C, S>) -> u64)>;

pub struct UtpContextHandle<C, S> {
    ctx: UtpContext<C, S>,
}

impl<C, S> Deref for UtpContextHandle<C, S> {
    type Target = UtpContext<C, S>;

    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl<C, S> DerefMut for UtpContextHandle<C, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

impl<C, S> Default for UtpContextHandle<C, S> {
    fn default() -> Self {
        let inner = unsafe {
            let inner = utp_init(2);
            utp_context_set_userdata(
                inner,
                Box::into_raw(Box::new(ContextData::<C, S> {
                    data: std::ptr::null_mut(),
                    callbacks: Default::default(),
                })) as *mut c_void,
            );
            inner
        };

        UtpContextHandle {
            ctx: UtpContext::wrap(inner),
        }
    }
}

impl<C, S> Drop for UtpContextHandle<C, S> {
    fn drop(&mut self) {
        unsafe {
            let ContextData::<C, S> { data, .. } =
                try_cast_ref_mut(utp_context_get_userdata(self.ctx.inner)).unwrap();
            if !data.is_null() {
                Box::from_raw(*data as *mut C);
            }
            // if we don't do `let _ctx_data`, callbacks will be dropped before the context is destroyed
            let _ctx_data =
                Box::from_raw(utp_context_get_userdata(self.ctx.inner) as *mut ContextData<C, S>);
            utp_destroy(self.ctx.inner);
        };
    }
}

pub struct UtpContext<C, S> {
    inner: *mut utp_context,
    context_data_type: PhantomData<C>,
    socket_data_type: PhantomData<S>,
}

impl<C, S> UtpContext<C, S> {
    pub fn wrap(inner: *mut utp_context) -> UtpContext<C, S> {
        UtpContext {
            inner,
            context_data_type: PhantomData,
            socket_data_type: PhantomData,
        }
    }

    pub unsafe fn connect(&self, addr: SocketAddr) -> Result<UtpSocketHandle<S>> {
        let socket = utp_create_socket(self.inner);
        let sockaddrinfo = *getaddrinfo_from_std(addr)?;
        match utp_connect(
            socket,
            sockaddrinfo.ai_addr,
            sockaddrinfo.ai_addrlen.try_into().unwrap(),
        ) {
            0 => Ok(UtpSocketHandle {
                socket: UtpSocket::wrap(socket).unwrap(),
            }),
            _ => {
                utp_close(socket);
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "utp_connect returned non-zero error code",
                ))
            }
        }
    }

    pub unsafe fn utp_issue_deferred_acks(&self) {
        utp_issue_deferred_acks(self.inner);
    }

    pub unsafe fn utp_process_udp(&self, from: SocketAddr, buf: &[u8]) -> bool {
        let ai = *getaddrinfo_from_std(from).unwrap();
        utp_process_udp(
            self.inner,
            buf.as_ptr(),
            buf.len().try_into().unwrap(),
            ai.ai_addr,
            ai.ai_addrlen.try_into().unwrap(),
        ) != 0
    }

    pub unsafe fn utp_check_timeouts(&self) {
        utp_check_timeouts(self.inner);
    }

    pub unsafe fn set_context_data(&self, data: C) {
        let ContextData::<C, S> { data: old_data, .. } =
            try_cast_ref_mut(utp_context_get_userdata(self.inner)).unwrap();
        if !old_data.is_null() {
            Box::from_raw(*old_data as *mut C);
        }
        *old_data = Box::into_raw(Box::new(data)) as *mut c_void;
    }

    unsafe fn get_callback(&self, event: UtpEvent) -> Option<&mut UtpCallback<C, S>> {
        let ContextData::<C, S> { callbacks, .. } =
            try_cast_ref_mut(utp_context_get_userdata(self.inner)).unwrap();
        callbacks.get_mut(&event)
    }

    pub unsafe fn get_context_data(&self) -> &C {
        let ContextData::<C, S> { data, .. } =
            try_cast_ref(utp_context_get_userdata(self.inner)).unwrap();
        try_cast_ref(*data).unwrap()
    }

    pub unsafe fn get_context_data_mut(&mut self) -> &mut C {
        let ContextData::<C, S> { data, .. } =
            try_cast_ref_mut(utp_context_get_userdata(self.inner)).unwrap();
        try_cast_ref_mut(*data).unwrap()
    }

    pub unsafe fn clear_callback(&self, event: UtpEvent) {
        let ContextData::<C, S> { callbacks, .. } =
            try_cast_ref_mut(utp_context_get_userdata(self.inner)).unwrap();
        utp_set_callback(self.inner, event as i32, None);
        callbacks.remove(&event);
    }

    pub unsafe fn set_callback<F>(&self, event: UtpEvent, cb: F)
    where
        F: FnMut(UtpCallbackArgs<C, S>) -> u64 + 'static,
    {
        let ContextData { callbacks, .. } =
            try_cast_ref_mut(utp_context_get_userdata(self.inner)).unwrap();
        callbacks.insert(event, Box::new(cb));

        macro_rules! set_callback {
            ($cb_type:expr) => {{
                unsafe extern "C" fn cb<C, S>(args: *mut utp_callback_arguments) -> uint64 {
                    let wrapped_args: UtpCallbackArgs<'_, C, S> = UtpCallbackArgs::new(args);
                    let cb = wrapped_args
                        .context
                        .get_callback($cb_type)
                        .expect("Callback was not set");
                    (cb)(UtpCallbackArgs::new(args))
                }
                utp_set_callback(self.inner, $cb_type as i32, Some(cb::<C, S>));
            }};
        }

        match event {
            UtpEvent::Log => set_callback!(UtpEvent::Log),
            UtpEvent::OnRead => set_callback!(UtpEvent::OnRead),
            UtpEvent::SendTo => set_callback!(UtpEvent::SendTo),
            UtpEvent::OnAccept => set_callback!(UtpEvent::OnAccept),
            UtpEvent::OnError => set_callback!(UtpEvent::OnError),
            UtpEvent::OnFirewall => set_callback!(UtpEvent::OnFirewall),
            UtpEvent::GetUdpMTU => set_callback!(UtpEvent::GetUdpMTU),
            UtpEvent::OnStateChange => set_callback!(UtpEvent::OnStateChange),
        }
    }
}

struct ContextData<C, S> {
    data: *mut c_void,
    callbacks: HashMap<UtpEvent, UtpCallback<C, S>>,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub enum UtpEvent {
    Log = UTP_LOG as isize,
    OnRead = UTP_ON_READ as isize,
    SendTo = UTP_SENDTO as isize,
    OnAccept = UTP_ON_ACCEPT as isize,
    OnError = UTP_ON_ERROR as isize,
    OnFirewall = UTP_ON_FIREWALL as isize,
    GetUdpMTU = UTP_GET_UDP_MTU as isize,
    OnStateChange = UTP_ON_STATE_CHANGE as isize,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum UtpState {
    UtpStateConnect = UTP_STATE_CONNECT as isize,
    UtpStateWritable = UTP_STATE_WRITABLE as isize,
    UtpStateEOF = UTP_STATE_EOF as isize,
    UtpStateDestroying = UTP_STATE_DESTROYING as isize,
    UtpInvalid,
}

impl From<i32> for UtpState {
    fn from(val: i32) -> Self {
        use UtpState::*;
        match val {
            1 => UtpStateConnect,
            2 => UtpStateWritable,
            3 => UtpStateEOF,
            4 => UtpStateDestroying,
            _ => UtpInvalid,
        }
    }
}

#[derive(Debug)]
pub enum UtpErrorCode {
    UtpConnRefused = UTP_ECONNREFUSED as isize,
    UtpConnReset = UTP_ECONNRESET as isize,
    UtpETimedOut = UTP_ETIMEDOUT as isize,
    Invalid,
}

impl From<i32> for UtpErrorCode {
    fn from(val: i32) -> Self {
        use UtpErrorCode::*;
        match val {
            0 => UtpConnRefused,
            1 => UtpConnReset,
            2 => UtpETimedOut,
            _ => Invalid,
        }
    }
}

pub struct UtpSocketHandle<S> {
    socket: UtpSocket<S>,
}

impl<S> Deref for UtpSocketHandle<S> {
    type Target = UtpSocket<S>;

    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl<S> DerefMut for UtpSocketHandle<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.socket
    }
}

impl<S> Drop for UtpSocketHandle<S> {
    fn drop(&mut self) {
        unsafe {
            let socket_data = utp_get_userdata(self.socket.inner);
            if !socket_data.is_null() {
                Box::from_raw(socket_data as *mut S);
            }
            utp_close(self.socket.inner);
        };
    }
}

pub struct UtpSocket<S> {
    inner: *mut utp_socket,
    socket_data_type: PhantomData<S>,
}

impl<S> UtpSocket<S> {
    pub fn wrap(inner: *mut utp_socket) -> Option<UtpSocket<S>> {
        if !inner.is_null() {
            Some(UtpSocket {
                inner,
                socket_data_type: PhantomData,
            })
        } else {
            None
        }
    }

    pub unsafe fn accept(self) -> UtpSocketHandle<S> {
        UtpSocketHandle { socket: self }
    }

    pub unsafe fn utp_write(&self, buf: &mut [u8]) -> usize {
        utp_write(
            self.inner,
            buf.as_mut_ptr() as *mut c_void,
            buf.len().try_into().unwrap(),
        ) as usize
    }

    pub unsafe fn utp_read_drained(&self) {
        utp_read_drained(self.inner);
    }

    pub unsafe fn set_socket_data(&self, data: S) {
        let old_data = utp_get_userdata(self.inner);
        if !old_data.is_null() {
            Box::from_raw(old_data as *mut S);
        }
        utp_set_userdata(self.inner, Box::into_raw(Box::new(data)) as *mut c_void);
    }

    pub unsafe fn get_socket_data(&self) -> &S {
        try_cast_ref(utp_get_userdata(self.inner)).unwrap()
    }

    pub unsafe fn get_socket_data_mut(&mut self) -> &mut S {
        try_cast_ref_mut(utp_get_userdata(self.inner)).unwrap()
    }
}

pub struct UtpCallbackArgs<'a, C, S> {
    pub context: UtpContext<C, S>,
    pub socket: Option<UtpSocket<S>>,
    pub buf: Option<&'a [u8]>,
    pub raw: *mut utp_callback_arguments,
}

impl<'a, C, S> UtpCallbackArgs<'a, C, S> {
    unsafe fn new(args: *mut utp_callback_arguments) -> UtpCallbackArgs<'a, C, S> {
        UtpCallbackArgs {
            context: UtpContext::wrap((*args).context),
            socket: UtpSocket::wrap((*args).socket),
            buf: buf_to_slice((*args).buf as *const u8, (*args).len as usize),
            raw: args,
        }
    }

    pub unsafe fn address(&self) -> Option<SocketAddr> {
        socket_addr_from_parts((*self.raw).args1.address, (*self.raw).args2.address_len)
    }

    pub unsafe fn send(&self) -> i32 {
        (*self.raw).args1.send
    }

    pub unsafe fn sample_ms(&self) -> i32 {
        (*self.raw).args1.sample_ms
    }

    pub unsafe fn error_code(&self) -> UtpErrorCode {
        (*self.raw).args1.error_code.into()
    }

    pub unsafe fn state(&self) -> UtpState {
        (*self.raw).args1.state.into()
    }

    pub unsafe fn bandwidth_type(&self) -> i32 {
        (*self.raw).args2.type_
    }
}

unsafe fn try_cast_ref<'a, T>(ptr: *mut c_void) -> Option<&'a T> {
    (ptr as *const T).as_ref()
}

unsafe fn try_cast_ref_mut<'a, T>(ptr: *mut c_void) -> Option<&'a mut T> {
    (ptr as *mut T).as_mut()
}

unsafe fn buf_to_slice<'a>(buf: *const u8, len: usize) -> Option<&'a [u8]> {
    if !buf.is_null() {
        Some(std::slice::from_raw_parts(buf, len))
    } else {
        None
    }
}

unsafe fn socket_addr_from_parts(addr: *const sockaddr, len: socklen_t) -> Option<SocketAddr> {
    if !addr.is_null() {
        socket2::SockAddr::from_raw_parts(addr, len).as_std()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::get_free_socketaddr;
    use std::rc::Rc;

    #[test]
    fn test_context_data() {
        unsafe {
            let mut ctx = UtpContextHandle::<u32, u32>::default();
            ctx.set_context_data(42);
            let data: u32 = *ctx.get_context_data();
            let data_mut: u32 = *ctx.get_context_data_mut();
            assert_eq!(data, 42);
            assert_eq!(data_mut, 42);
        }
    }

    #[test]
    fn test_socket_data() {
        unsafe {
            let ctx = UtpContextHandle::<u32, u32>::default();
            {
                let sock = ctx.connect(get_free_socketaddr()).unwrap();
                sock.set_socket_data(42);
                let data: u32 = *sock.get_socket_data();
                let data_mut: u32 = *sock.get_socket_data();
                assert_eq!(data, 42);
                assert_eq!(data_mut, 42);
            }
        }
    }

    #[test]
    fn test_context_data_drop() {
        let data = Rc::new(42);
        unsafe {
            let ctx = UtpContextHandle::<Rc<u32>, ()>::default();
            ctx.set_context_data(Rc::clone(&data));
            assert_eq!(Rc::strong_count(&data), 2);
            ctx.set_context_data(Rc::clone(&data));
            assert_eq!(Rc::strong_count(&data), 2);
        }
        assert_eq!(Rc::strong_count(&data), 1);
    }

    #[test]
    fn test_socket_data_drop() {
        let data = Rc::new(42);
        unsafe {
            let ctx = UtpContextHandle::<(), Rc<u32>>::default();
            {
                let sock = ctx.connect(get_free_socketaddr()).unwrap();
                sock.set_socket_data(Rc::clone(&data));
                assert_eq!(Rc::strong_count(&data), 2);
                sock.set_socket_data(Rc::clone(&data));
                assert_eq!(Rc::strong_count(&data), 2);
            }
            assert_eq!(Rc::strong_count(&data), 1);
        }
    }

    #[test]
    fn test_callback() {
        unsafe {
            let ctx = UtpContextHandle::<bool, ()>::default();
            ctx.set_context_data(false);
            ctx.set_callback(UtpEvent::SendTo, |mut args| {
                *args.context.get_context_data_mut() = true;
                0
            });
            assert_eq!(false, *ctx.get_context_data());
            // Calling connect will cause the SENDTO callback to fire
            let _sock = ctx.connect(get_free_socketaddr()).unwrap();
            assert_eq!(true, *ctx.get_context_data());
        }
    }
}
