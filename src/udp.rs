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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::get_free_socketaddr;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn write_until_wouldblock(io: &mio::net::UdpSocket, buf: &mut Vec<u8>, to: SocketAddr) {
        let datagram_size = 500;
        loop {
            if buf.is_empty() {
                break;
            };
            let cap = std::cmp::min(datagram_size, buf.len());
            match io.send_to(&buf[..cap], to) {
                Ok(n) => {
                    buf.drain(..n);
                }
                Err(e) => {
                    assert_eq!(io::ErrorKind::WouldBlock, e.kind());
                    break;
                }
            }
        }
    }

    fn read_until_wouldblock(
        io: &mio::net::UdpSocket,
        buf: &mut Vec<u8>,
        expected_from: SocketAddr,
    ) -> bool {
        let read_buf = &mut [0; 100000];
        loop {
            match io.recv_from(read_buf) {
                Ok((n, from)) => {
                    buf.extend_from_slice(&read_buf[..n]);
                    assert_eq!(from, expected_from);
                }
                Err(e) => {
                    assert_eq!(io::ErrorKind::WouldBlock, e.kind());
                    break;
                }
            }
        }
        true
    }

    #[tokio::test]
    async fn test_udp() {
        let addr1 = get_free_socketaddr();
        let addr2 = get_free_socketaddr();

        let payload: Vec<u8> = (0..500000).map(|i: u32| (i % 256) as u8).collect();
        let mut payload2 = payload.clone();
        let mut recv_buf = vec![];
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);

        let _s1 = UdpSocket::bind(
            addr1,
            |_io| false,
            move |io| {
                write_until_wouldblock(io, &mut payload2, addr2);
            },
            |_| (),
        )
        .unwrap();
        let _s2 = UdpSocket::bind(
            addr2,
            move |io| {
                let would_block = read_until_wouldblock(io, &mut recv_buf, addr1);
                if recv_buf.len() == payload.len() {
                    assert_eq!(recv_buf, payload);
                    done_clone.store(true, Ordering::SeqCst);
                }
                would_block
            },
            |_| (),
            |_| (),
        )
        .unwrap();

        while !done.load(Ordering::SeqCst) {}
    }
}
