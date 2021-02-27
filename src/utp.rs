use crate::thread::ThreadRunner;
use crate::udp::UdpSocket;
use crate::wrappers;
use crate::Result;

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::future::Future;
use std::io;
use std::io::Read;
use std::net::SocketAddr;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::task::{Context, Poll, Waker};

use crossbeam::queue::{ArrayQueue, PushError, SegQueue};
use futures::task::AtomicWaker;
use futures::{ready, Stream};
use libutp_sys::*;
use log::*;
use std::fmt::{self, Debug, Formatter};
use tokio::io::{AsyncRead, AsyncWrite};

type InnerUtpContextHandle = wrappers::UtpContextHandle<ContextData, SocketData>;
type InnerUtpContext = wrappers::UtpContext<ContextData, SocketData>;
type InnerUtpSocketHandle = wrappers::UtpSocketHandle<SocketData>;
type UtpCallbackArgs<'a> = wrappers::UtpCallbackArgs<'a, ContextData, SocketData>;

static SOCKET_READ_BUFFER_SIZE: usize = 2 << 20;

static SOCKET_QUEUE_CAPACITY: usize = 1000;

#[derive(Clone)]
struct UtpSocketQueue {
    sockets: Arc<ArrayQueue<QueuedUtpSocket>>,
    waker: Arc<AtomicWaker>,
}

impl UtpSocketQueue {
    fn new() -> UtpSocketQueue {
        UtpSocketQueue {
            sockets: Arc::new(ArrayQueue::new(SOCKET_QUEUE_CAPACITY)),
            waker: Default::default(),
        }
    }

    fn poll_next(&self, cx: &mut Context) -> Poll<Option<Result<UtpSocket>>> {
        if let Ok(socket) = self.sockets.pop() {
            Poll::Ready(Some(Ok(socket.accept())))
        } else {
            self.waker.register(cx.waker());
            Poll::Pending
        }
    }

    fn push_socket(&self, socket: QueuedUtpSocket) {
        if let Err(PushError(s)) = self.sockets.push(socket) {
            // if socket queue is full, accept it so that it is cleanup by UtpSocket's drop impl
            s.accept();
        }
        self.waker.wake();
    }
}

#[derive(Clone, Default)]
struct DataQueue {
    queue: Arc<SegQueue<Vec<u8>>>,
    popped: Option<Vec<u8>>,
}

impl Deref for DataQueue {
    type Target = SegQueue<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &*self.queue
    }
}

impl Read for DataQueue {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0;
        while read < buf.len() && (!self.queue.is_empty() || self.popped.is_some()) {
            let mut chunk = self
                .popped
                .take()
                .unwrap_or_else(|| self.queue.pop().unwrap());
            let n = (&*chunk).read(&mut buf[read..])?;
            read += n;
            if n < chunk.len() {
                chunk.drain(..n);
                self.popped.replace(chunk);
                return Ok(read);
            }
        }
        Ok(read)
    }
}

#[derive(Clone)]
struct UtpReader {
    data: DataQueue,
    waker: Arc<AtomicWaker>,
    eof: Arc<AtomicBool>,
}

impl UtpReader {
    fn new() -> UtpReader {
        UtpReader {
            data: Default::default(),
            waker: Default::default(),
            eof: Default::default(),
        }
    }
}

impl UtpReader {
    fn poll_read(&mut self, cx: &mut Context, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let read = self.data.read(buf)?;
        if read == 0 {
            if self.eof() {
                Poll::Ready(Ok(0))
            } else {
                self.waker.register(cx.waker());
                Poll::Pending
            }
        } else {
            Poll::Ready(Ok(read))
        }
    }

    fn push(&self, data: &[u8]) {
        self.data.push(data.to_vec());
        self.waker.wake();
    }

    fn set_eof(&self) {
        self.eof.store(true, Ordering::SeqCst);
    }

    fn eof(&self) -> bool {
        self.eof.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct UtpWriter {
    udp_writable: Arc<AtomicBool>,
    wakers: Arc<Mutex<HashMap<SocketAddr, Waker>>>,
}

impl UtpWriter {
    fn new() -> UtpWriter {
        UtpWriter {
            udp_writable: Arc::new(AtomicBool::new(true)),
            wakers: Default::default(),
        }
    }

    fn poll_write(
        &self,
        addr: SocketAddr,
        utp_socket: &InnerUtpSocketHandle,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.udp_writable.load(Ordering::SeqCst) {
            // if udp is writable, try to write to the virtual utp socket
            let mut write_buf = vec![0; buf.len()];
            write_buf[..].copy_from_slice(buf);
            let sent = unsafe { utp_socket.utp_write(&mut write_buf) };
            trace!("utp_write sent: {}, buf len: {}", sent, write_buf.len());
            if sent == 0 {
                // utp not writable, so register for wakeup by on_state_change
                self.wakers.lock().unwrap().insert(addr, cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(Ok(sent))
            }
        } else {
            // udp not writable, register for wakeup by on_write_ready
            self.wakers.lock().unwrap().insert(addr, cx.waker().clone());
            Poll::Pending
        }
    }

    fn set_udp_writable(&self, writable: bool) {
        self.udp_writable.store(writable, Ordering::SeqCst);
        if writable {
            self.wake_all();
        }
    }

    fn wake_addr(&self, addr: &SocketAddr) {
        if let Some(w) = self.wakers.lock().unwrap().remove(addr) {
            w.wake()
        }
    }

    fn wake_all(&self) {
        self.wakers
            .lock()
            .unwrap()
            .drain()
            .for_each(|(_, w)| w.wake());
    }
}

#[derive(Default, Clone)]
struct ErrorChannel {
    context: Arc<RwLock<Option<io::Error>>>,
    sockets: Arc<RwLock<HashMap<SocketAddr, io::Error>>>,
}

impl ErrorChannel {
    fn set_socket_err(&self, addr: SocketAddr, e: io::Error) {
        self.sockets.write().unwrap().insert(addr, e);
    }

    fn set_context_err(&self, e: io::Error) {
        self.context.write().unwrap().replace(e);
    }

    fn check_err(&self, addr: &SocketAddr) -> io::Result<()> {
        let is_err = self.sockets.read().unwrap().get(addr).is_some();
        if is_err {
            self.sockets
                .write()
                .unwrap()
                .remove(addr)
                .map_or(Ok(()), Err)?;
        }
        self.check_context_err()
    }

    fn check_context_err(&self) -> io::Result<()> {
        let is_err = { self.context.read().unwrap().is_some() };
        if is_err {
            self.context.write().unwrap().take().map_or(Ok(()), Err)
        } else {
            Ok(())
        }
    }
}

struct ContextData {
    io: UdpSocket,
    writer: UtpWriter,
    socket_queue: UtpSocketQueue,
    libutp: Weak<Mutex<LibUtp>>,
    sockets: HashMap<SocketAddr, InnerUtpSocketHandle>,
    error_channel: ErrorChannel,
    discard_addrs: HashSet<SocketAddr>,
    listening: Arc<AtomicU8>,
    _check_timeouts: ThreadRunner,
}

struct LibUtp {
    ctx: InnerUtpContextHandle,
}

impl LibUtp {
    fn new() -> LibUtp {
        LibUtp {
            ctx: wrappers::UtpContextHandle::default(),
        }
    }
}

unsafe impl Send for LibUtp {}

///
/// A UTP context
///
/// The [`UtpContext`] is the core of a UTP application. It sets up the backing UDP socket and provides
/// methods for initiating and accepting connections.
///
pub struct UtpContext {
    // libutp is not thread safe, so only one thread at a time can access the underlying utp_context
    libutp: Arc<Mutex<LibUtp>>,
}

impl UtpContext {
    ///
    /// Creates the backing UDP socket and returns a new [`UtpContext`]
    /// ## Example
    ///
    /// ```no_run
    /// use libutp_rs::UtpContext;
    ///
    /// let ctx = UtpContext::bind("127.0.0.1:5000".parse().unwrap()).unwrap();
    /// ```
    pub fn bind(addr: SocketAddr) -> Result<UtpContext> {
        let libutp = Arc::new(Mutex::new(LibUtp::new()));
        let writer = UtpWriter::new();
        // pass weak libutp references to callbacks so that they won't prevent dropping of
        // the context & sockets
        let on_read_libutp = Arc::downgrade(&libutp);
        let on_err_libutp = Arc::downgrade(&libutp);
        let on_write_writer = writer.clone();
        let io = UdpSocket::bind(
            addr,
            move |io| {
                on_read_libutp.upgrade().map_or(false, |libutp| {
                    let mut libutp = libutp.lock().unwrap();
                    Self::udp_on_read_ready(io, &mut *libutp)
                })
            },
            move |_| on_write_writer.set_udp_writable(true),
            move |err| unsafe {
                if let Some(libutp) = on_err_libutp.upgrade() {
                    let mut libutp = libutp.lock().unwrap();
                    libutp
                        .ctx
                        .get_context_data_mut()
                        .error_channel
                        .set_context_err(err);
                }
            },
        )?;

        let check_timeouts = Self::start_check_timeouts(&libutp);

        unsafe {
            let LibUtp { ctx, .. } = &*libutp.lock().unwrap();

            use wrappers::UtpEvent::*;
            ctx.set_callback(Log, on_log);
            ctx.set_callback(OnRead, on_read);
            ctx.set_callback(SendTo, on_sendto);
            ctx.set_callback(OnAccept, on_accept);
            ctx.set_callback(OnError, on_error);
            ctx.set_callback(OnStateChange, on_state_change);
            ctx.set_callback(OnFirewall, on_firewall);

            ctx.set_context_data(ContextData {
                io,
                writer,
                socket_queue: UtpSocketQueue::new(),
                libutp: Arc::downgrade(&libutp),
                sockets: HashMap::new(),
                discard_addrs: HashSet::new(),
                listening: Default::default(),
                _check_timeouts: check_timeouts,
                error_channel: Default::default(),
            });
        }

        Ok(UtpContext { libutp })
    }

    /// Returns a [`Stream`] of incoming connections.
    ///
    /// ## Example
    ///
    /// ```no_run
    /// use libutp_rs::UtpContext;
    /// use std::error::Error;
    /// use futures::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error>> {
    ///     let ctx = UtpContext::bind("127.0.0.1:5000".parse()?)?;
    ///     let mut listener = ctx.listener();
    ///     let socket = listener.next().await.unwrap()?;
    ///     Ok(())
    /// }
    /// ```
    pub fn listener(&self) -> UtpListener {
        let ctx = &*self.libutp.lock().unwrap().ctx;
        let ContextData {
            socket_queue,
            error_channel,
            listening,
            ..
        } = unsafe { ctx.get_context_data() };
        listening.fetch_add(1, Ordering::SeqCst);
        UtpListener {
            libutp: Arc::clone(&self.libutp),
            error_channel: error_channel.clone(),
            socket_queue: socket_queue.clone(),
            listening: Arc::clone(listening),
        }
    }

    /// Initiates a connection.
    ///
    /// Returns a [`Connect`] future which resolves to a [`UtpSocket`] if successful.
    ///
    /// ## Example
    ///
    /// ```no_run
    /// use libutp_rs::UtpContext;
    /// use std::error::Error;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error>> {
    ///     let ctx = UtpContext::bind("127.0.0.1:5000".parse()?)?;
    ///     let socket = ctx.connect("127.0.0.1:5001".parse()?).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn connect(&self, addr: SocketAddr) -> Connect {
        let ctx = &mut *self.libutp.lock().unwrap().ctx;
        let connector: UtpConnector = Default::default();
        let socket = unsafe {
            match ctx.connect(addr) {
                Ok(s) => {
                    Some(QueuedUtpSocket::new(&mut *ctx, s, addr, Some(connector.clone())).accept())
                }
                Err(e) => {
                    ctx.get_context_data().error_channel.set_socket_err(addr, e);
                    None
                }
            }
        };
        Connect {
            socket,
            connector,
            error_channel: unsafe { ctx.get_context_data().error_channel.clone() },
        }
    }

    pub fn set_udp_mtu(&self, mtu: u16) {
        let ctx = &mut *self.libutp.lock().unwrap().ctx;
        unsafe {
            ctx.set_callback(wrappers::UtpEvent::GetUdpMTU, move |_| mtu as u64);
        }
    }

    pub fn clear_mtu(&self) {
        let ctx = &mut *self.libutp.lock().unwrap().ctx;
        unsafe {
            ctx.clear_callback(wrappers::UtpEvent::GetUdpMTU);
        }
    }

    fn start_check_timeouts(libutp: &Arc<Mutex<LibUtp>>) -> ThreadRunner {
        let libutp_weak = Arc::downgrade(libutp);
        ThreadRunner::run(move |stop_flag| {
            while !stop_flag.is_set() {
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe {
                    if let Some(libutp) = libutp_weak.upgrade() {
                        let LibUtp { ctx, .. } = &*libutp.lock().unwrap();
                        ctx.utp_check_timeouts();
                    }
                }
            }
        })
    }

    fn udp_on_read_ready(io: &mio::net::UdpSocket, libutp: &mut LibUtp) -> bool {
        let buf = &mut [0; 65536];
        let LibUtp { ctx, .. } = &mut *libutp;
        let discard_addrs = unsafe { ctx.get_context_data().discard_addrs.clone() };
        loop {
            match io.recv_from(buf) {
                Ok((n, from)) => unsafe {
                    trace!("Received {} bytes from {}", n, from);
                    if !discard_addrs.contains(&from) && !ctx.utp_process_udp(from, &buf[..n]) {
                        trace!("Got UDP packet that could not be processed by UTP");
                    }
                },
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => unsafe {
                    ctx.utp_issue_deferred_acks();
                    return true;
                },
                Err(e) => unsafe {
                    ctx.get_context_data_mut().error_channel.set_context_err(e);
                },
            }
        }
    }
}

#[derive(Clone, Default)]
struct UtpConnector {
    connected: Arc<AtomicBool>,
    waker: Arc<AtomicWaker>,
}

impl UtpConnector {
    fn poll_connected(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.connected.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            self.waker.register(cx.waker());
            Poll::Pending
        }
    }

    fn set_connected(&mut self) {
        self.connected.store(true, Ordering::SeqCst);
        self.waker.wake();
    }
}

/// A [`Future`] representing a pending connection
pub struct Connect {
    socket: Option<UtpSocket>,
    connector: UtpConnector,
    error_channel: ErrorChannel,
}

impl Future for Connect {
    type Output = Result<UtpSocket>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.error_channel
            .check_err(&self.socket.as_ref().unwrap().addr)?;
        ready!(self.connector.poll_connected(cx))?;
        Poll::Ready(Ok(self.as_mut().socket.take().unwrap()))
    }
}

struct SocketData {
    reader: UtpReader,
    addr: SocketAddr,
    connector: Option<UtpConnector>,
}

struct QueuedUtpSocket {
    addr: SocketAddr,
    libutp: Weak<Mutex<LibUtp>>,
    reader: UtpReader,
    error_channel: ErrorChannel,
}

impl QueuedUtpSocket {
    fn new(
        ctx: &mut InnerUtpContext,
        socket: InnerUtpSocketHandle,
        addr: SocketAddr,
        connector: Option<UtpConnector>,
    ) -> QueuedUtpSocket {
        unsafe {
            let ContextData {
                libutp,
                sockets,
                error_channel,
                ..
            } = ctx.get_context_data_mut();
            let reader = UtpReader::new();
            socket.set_socket_data(SocketData {
                reader: reader.clone(),
                addr,
                connector,
            });
            sockets.insert(addr, socket);
            QueuedUtpSocket {
                addr,
                libutp: libutp.clone(),
                reader,
                error_channel: error_channel.clone(),
            }
        }
    }

    fn accept(self) -> UtpSocket {
        UtpSocket {
            addr: self.addr,
            libutp: self.libutp.upgrade().unwrap(),
            reader: self.reader,
            error_channel: self.error_channel,
        }
    }
}
///
/// A UTP socket
///
/// A [`UtpSocket`] is created either by accepting an incoming connection from a [`UtpListener`], or by
/// initiating a connection using [`UtpContext::connect`].
///
/// Writing to and reading from a [`UtpSocket`] is done using the [`AsyncWrite`] and [`AsyncRead`] traits respectively.
///
pub struct UtpSocket {
    addr: SocketAddr,
    libutp: Arc<Mutex<LibUtp>>,
    reader: UtpReader,
    error_channel: ErrorChannel,
}

impl UtpSocket {
    pub fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Debug for UtpSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "UtpSocket: {:?}", self.addr)
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        let ctx = &mut *self.libutp.lock().unwrap().ctx;
        unsafe {
            ctx.get_context_data_mut().sockets.remove(&self.addr);
        }
    }
}

impl AsyncRead for UtpSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.error_channel.check_err(&self.addr)?;
        self.as_mut().reader.poll_read(cx, buf)
    }
}

impl AsyncWrite for UtpSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.error_channel.check_err(&self.addr)?;
        let ctx = &self.libutp.lock().unwrap().ctx;
        let (socket, writer) = unsafe {
            let ContextData {
                sockets, writer, ..
            } = ctx.get_context_data();
            (sockets.get(&self.addr).unwrap(), writer)
        };
        writer.poll_write(self.addr, socket, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A [`Stream`] of incoming UTP connections
#[derive(Clone)]
pub struct UtpListener {
    libutp: Arc<Mutex<LibUtp>>,
    error_channel: ErrorChannel,
    socket_queue: UtpSocketQueue,
    listening: Arc<AtomicU8>,
}

impl Drop for UtpListener {
    fn drop(&mut self) {
        self.listening.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Stream for UtpListener {
    type Item = Result<UtpSocket>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.error_channel.check_context_err()?;
        self.socket_queue.poll_next(cx)
    }
}

fn on_log(args: UtpCallbackArgs) -> uint64 {
    if let Some(Ok(msg)) = args.buf.map(|b| CStr::from_bytes_with_nul(b)) {
        trace!("{:?}", msg);
    }
    0
}

fn on_read(mut args: UtpCallbackArgs) -> uint64 {
    if let (Some(socket), Some(buf)) = (args.socket, args.buf) {
        trace!("on_read got {} bytes", buf.len());
        unsafe {
            let SocketData { reader, addr, .. } = socket.get_socket_data();
            let discard_addrs = &mut args.context.get_context_data_mut().discard_addrs;
            reader.push(buf);
            if reader.data.len() > SOCKET_READ_BUFFER_SIZE {
                discard_addrs.insert(*addr);
            } else {
                discard_addrs.remove(addr);
            }
            socket.utp_read_drained();
        }
    }
    0
}

fn on_error(mut args: UtpCallbackArgs) -> uint64 {
    trace!("on_error: {}", unsafe { args.error_code() } as isize);
    if let Some(mut socket) = args.socket.take() {
        let error_code = unsafe { args.error_code() };
        let SocketData {
            reader,
            connector,
            addr,
            ..
        } = unsafe { socket.get_socket_data_mut() };
        let ContextData {
            socket_queue,
            writer,
            error_channel,
            ..
        } = unsafe { args.context.get_context_data_mut() };

        use wrappers::UtpErrorCode::*;
        error_channel.set_socket_err(
            *addr,
            match error_code {
                UtpConnRefused => io::Error::from(io::ErrorKind::ConnectionRefused),
                UtpConnReset => io::Error::from(io::ErrorKind::ConnectionReset),
                UtpETimedOut => io::Error::from(io::ErrorKind::TimedOut),
                Invalid => io::Error::from(io::ErrorKind::Other),
            },
        );
        writer.wake_all();
        reader.waker.wake();
        socket_queue.waker.wake();
        if let Some(w) = connector.as_ref() {
            w.waker.wake()
        };
    }
    0
}

fn on_sendto(mut args: UtpCallbackArgs) -> uint64 {
    if let (Some(addr), Some(buf)) = (unsafe { args.address() }, args.buf) {
        let ContextData {
            io,
            writer,
            error_channel,
            ..
        } = unsafe { args.context.get_context_data_mut() };
        let buf_len = buf.len();
        let mut sent = 0;
        loop {
            match io.inner().send_to(&buf[sent..], addr) {
                Ok(n) => {
                    trace!("on_sendto: sent {} bytes to {}", n, addr);
                    sent += n;
                    if sent == buf_len {
                        break;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    writer.set_udp_writable(false);
                    break;
                }
                Err(e) => {
                    error_channel.set_context_err(e);
                }
            }
        }
    }
    0
}

fn on_accept(mut args: UtpCallbackArgs) -> uint64 {
    if let (Some(socket), Some(addr)) = (args.socket.take(), unsafe { args.address() }) {
        trace!("on_accept: addr = {}", addr);
        unsafe {
            let socket_handle = socket.accept();
            let queued_socket = QueuedUtpSocket::new(&mut args.context, socket_handle, addr, None);
            args.context
                .get_context_data_mut()
                .socket_queue
                .push_socket(queued_socket);
        }
    }
    0
}

fn on_state_change(args: UtpCallbackArgs) -> uint64 {
    unsafe {
        trace!("on_state_change: code = {}", args.state() as isize);
    }
    use wrappers::UtpState::*;
    match unsafe { args.state() } {
        UtpStateConnect => unsafe {
            trace!("socket connected!");
            if let Some(c) = args
                .socket
                .unwrap()
                .get_socket_data_mut()
                .connector
                .as_mut()
            {
                c.set_connected()
            }
        },
        UtpStateWritable => unsafe {
            let writer = &args.context.get_context_data().writer;
            writer.wake_addr(&args.socket.unwrap().get_socket_data().addr);
        },
        UtpStateEOF => unsafe {
            args.socket.unwrap().get_socket_data_mut().reader.set_eof();
        },
        UtpStateDestroying => trace!("utp_socket is being destroyed"),
        val @ UtpInvalid => trace!("got invalid state {}", val as isize),
    }
    0
}

fn on_firewall(args: UtpCallbackArgs) -> uint64 {
    let context_data = unsafe { args.context.get_context_data() };
    if context_data.listening.load(Ordering::SeqCst) > 0 {
        0 // Allow incoming connection
    } else {
        1 // Block
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::get_free_socketaddr;
    use futures::stream::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn init_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_transfer() {
        init_logger();

        // Create two sockets and send a bunch of data between them
        let addr1 = get_free_socketaddr();
        let ctx1 = UtpContext::bind(addr1).unwrap();
        let addr2 = get_free_socketaddr();
        let ctx2 = UtpContext::bind(addr2).unwrap();

        let mut listener = ctx1.listener();
        let mut s2 = ctx2.connect(addr1).await.unwrap();
        let mut s1 = listener.next().await.unwrap().unwrap();

        tokio::try_join!(
            tokio::spawn(async move {
                for _ in 0..100 {
                    s1.write_all(&[1; 1000]).await.unwrap();
                    let buf = &mut [0; 1000];
                    s1.read_exact(buf).await.unwrap();
                    assert_eq!(&buf[..], &[2; 1000][..]);
                }
            }),
            tokio::spawn(async move {
                for _ in 0..100 {
                    s2.write_all(&[2; 1000]).await.unwrap();
                    let buf = &mut [0; 1000];
                    s2.read_exact(buf).await.unwrap();
                    assert_eq!(&buf[..], &[1; 1000][..]);
                }
            })
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_timeout() {
        init_logger();
        let addr1 = get_free_socketaddr();
        let addr2 = get_free_socketaddr();
        let ctx2 = UtpContext::bind(addr2).unwrap();
        assert_eq!(
            ctx2.connect(addr1).await.err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn test_drop_socket() {
        init_logger();
        let addr1 = get_free_socketaddr();
        let addr2 = get_free_socketaddr();
        let ctx1 = UtpContext::bind(addr1).unwrap();
        let ctx2 = UtpContext::bind(addr2).unwrap();
        let mut listener = ctx1.listener();
        let mut s2 = ctx2.connect(addr1).await.unwrap();
        {
            let mut _s1 = listener.next().await.unwrap().unwrap();
        }
        assert_eq!(
            s2.read_exact(&mut [0]).await.err().unwrap().kind(),
            io::ErrorKind::ConnectionReset
        );
    }

    #[tokio::test]
    async fn test_drop_inner() {
        init_logger();
        let addr = get_free_socketaddr();
        let ctx = UtpContext::bind(addr).unwrap();
        let inner = Arc::clone(&ctx.libutp);
        let listener = ctx.listener();
        std::mem::drop(ctx);
        std::mem::drop(listener);
        assert_eq!(Arc::strong_count(&inner), 1);
    }
}
