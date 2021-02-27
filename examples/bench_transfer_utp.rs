// Benchmarks the UTP transfer rate
// Known to hang on linux sometimes when the last data sent isn't flushed properly

use libutp_rs::UtpContext;

use std::error::Error;
use std::net::SocketAddr;

use bytesize::ByteSize;
use futures::StreamExt;
use log::*;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHUNKS: u64 = 1000;
const CHUNK_SIZE: u64 = 1000000;
const TOTAL: u64 = CHUNKS * CHUNK_SIZE;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let sender_addr: SocketAddr = "127.0.0.1:5000".parse()?;
    let receiver_addr: SocketAddr = "127.0.0.1:5001".parse()?;

    let sender_ctx = UtpContext::bind(sender_addr)?;
    let receiver_ctx = UtpContext::bind(receiver_addr)?;
    sender_ctx.set_udp_mtu(50000);
    receiver_ctx.set_udp_mtu(50000);

    let mut receiver_listener = receiver_ctx.listener();

    println!(
        "Starting bench_transfer_utp. Sending {} chunks of size {} for a total of {}",
        CHUNKS,
        ByteSize::b(CHUNK_SIZE),
        ByteSize::b(TOTAL)
    );
    let before = Instant::now();

    tokio::spawn(async move {
        let mut sender = sender_ctx.connect(receiver_addr).await.unwrap();
        let mut send_buf = vec![0; CHUNK_SIZE as usize];
        for i in 0..CHUNKS {
            sender.write_all(&mut send_buf).await.unwrap();
            debug!("Bytes sent: {:?}", (i + 1) * CHUNK_SIZE);
        }
    });

    let mut receiver = receiver_listener.next().await.unwrap().unwrap();

    let mut read_buf = vec![0; CHUNK_SIZE as usize];
    let mut bytes_received: u64 = 0;
    let mut chunks_received = 0;
    loop {
        bytes_received += receiver.read(&mut read_buf).await? as u64;
        chunks_received += 1;
        debug!("Bytes received: {:?}", bytes_received);
        if bytes_received == CHUNKS * CHUNK_SIZE {
            break;
        }
    }
    let elapsed = before.elapsed();
    let bps = 1000 * bytes_received as u128 / elapsed.as_millis();
    println!(
        "Finished bench_transfer_utp. \
    Chunks received: {}, \
    Transfer rate: {} / s, \
    Elapsed: {} ms",
        chunks_received,
        ByteSize::b(bps as u64),
        before.elapsed().as_millis()
    );

    Ok(())
}
