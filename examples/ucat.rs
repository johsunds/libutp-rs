use libutp_rs::UtpContext;

use std::env;
use std::error::Error;
use std::io::{self, BufRead};
use std::net::SocketAddr;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    let local_addr: SocketAddr = args[1].parse()?;
    let remote_addr: Option<SocketAddr> = args.get(2).map(|addr| addr.parse().unwrap());

    let context = UtpContext::bind(local_addr)?;

    let socket = if let Some(remote_addr) = remote_addr {
        println!("Connecting to {}", remote_addr);
        context.connect(remote_addr).await?
    } else {
        println!("Listening for connection on {}", local_addr);
        let mut listener = context.listener();
        let socket = listener.next().await.unwrap()?;
        println!("Accepted connection from {}", socket.peer_addr());
        socket
    };

    let (reader, mut writer) = tokio::io::split(socket);
    let mut reader = BufReader::new(reader);

    // Read from stdin and write it to the socket
    std::thread::spawn(move || {
        let stdin = io::stdin();
        loop {
            for line in stdin.lock().lines() {
                let line = format!("{}\n", line.unwrap());
                futures::executor::block_on(writer.write_all(line.as_bytes())).unwrap();
            }
        }
    });

    // Read from the socket and write to stdout
    let mut buf = String::new();
    loop {
        reader.read_line(&mut buf).await?;
        print!("{}", buf);
        buf.clear();
    }
}
