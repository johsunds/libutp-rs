# libutp-rs

[Docs](https://docs.rs/libutp-rs/0.1.0/libutp_rs/)

An async rust interface to [libutp](https://github.com/bittorrent/libutp).

## Examples

A simple ucat implementation is provided in `examples/ucat.rs`

``` 
// Bind to 5000
cargo run --example ucat 127.0.0.1:5000

Listening for connection on 127.0.0.1:5000
Hi!
```

``` 
// Bind to 5001 and connect to 5000
cargo run --example ucat 127.0.0.1:5001 127.0.0.1:5000

Connecting to 127.0.0.1:5000
Hi!
```
    
