use crate::thread::{StopFlag, ThreadRunner};

use mio::{Events, Interest, Token};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// An event loop for a mio::net::UdpSocket
///
/// Operates using three closures:
///
/// on_read_ready:   called when the socket is ready to read, and should return a bool indicating if
///                  the socket was read until WouldBlock
/// on_write_ready:  called when the socket is ready to write
/// on_err:          called if an error occurs
struct EventLoop<R, W, E> {
    io: Arc<mio::net::UdpSocket>,
    poll: mio::Poll,
    on_read_ready: R,
    on_write_ready: W,
    on_err: E,
}

impl<R, W, E> EventLoop<R, W, E>
where
    R: FnMut(&mio::net::UdpSocket) -> bool + 'static + Send,
    W: FnMut(&mio::net::UdpSocket) + 'static + Send,
    E: FnMut(io::Error) + 'static + Send,
{
    fn new(
        io: &mut Arc<mio::net::UdpSocket>,
        on_read_ready: R,
        on_write_ready: W,
        on_err: E,
    ) -> io::Result<EventLoop<R, W, E>> {
        let poll = mio::Poll::new()?;
        poll.registry().register(
            Arc::get_mut(io).unwrap(),
            Token(0),
            Interest::READABLE | Interest::WRITABLE,
        )?;
        let io = Arc::clone(&io);
        let event_loop = EventLoop {
            io,
            poll,
            on_read_ready,
            on_write_ready,
            on_err,
        };
        Ok(event_loop)
    }

    fn run(&mut self, stop_flag: StopFlag) {
        let mut events = Events::with_capacity(128);
        let mut read_ready = false;
        let mut send_ready = false;
        while !stop_flag.is_set() {
            if let Err(e) = self
                .poll
                .poll(&mut events, Some(Duration::from_millis(100)))
            {
                (self.on_err)(e)
            }

            for event in events.iter() {
                read_ready = event.is_readable();
                send_ready = event.is_writable();
            }

            if read_ready {
                read_ready = !(self.on_read_ready)(&self.io);
            }

            if send_ready {
                (self.on_write_ready)(&self.io);
                send_ready = false;
            }
        }
    }
}

pub(crate) struct UdpSocket {
    io: Arc<mio::net::UdpSocket>,
    _event_loop: ThreadRunner,
}

impl UdpSocket {
    /// Binds a udp socket to addr and spawns a thread running an event loop that is
    /// responsible for waking the receiver and senders when they are ready to send/receive
    pub(crate) fn bind<R, W, E>(
        addr: SocketAddr,
        on_read_ready: R,
        on_write_ready: W,
        on_err: E,
    ) -> io::Result<UdpSocket>
    where
        R: FnMut(&mio::net::UdpSocket) -> bool + 'static + Send,
        W: FnMut(&mio::net::UdpSocket) + 'static + Send,
        E: FnMut(io::Error) + 'static + Send,
    {
        let mut io = Arc::new(mio::net::UdpSocket::bind(addr)?);
        let mut _event_loop = EventLoop::new(&mut io, on_read_ready, on_write_ready, on_err)?;
        Ok(UdpSocket {
            io,
            _event_loop: ThreadRunner::run(move |stop_flag| _event_loop.run(stop_flag)),
        })
    }

    pub(crate) fn inner(&self) -> &Arc<mio::net::UdpSocket> {
        &self.io
    }
}
