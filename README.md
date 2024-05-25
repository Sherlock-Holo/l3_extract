# l3_extract

extract layer 4 connection from layer 3

This crate aim to provide a way to allow users manipulate TCP and UDP which is obtained from layer-3 protocol packet,
like normal async TCP stream and async UDP socket, but with complete based mode.

This crate is based on `smoltcp`, which is a single threaded, serial and event driven userspace TCP network stack
crate, but this crate allows users use `TcpStream`, `UdpSocket` and others concurrently.

## Notes:

This crate try to put ease of use first, then performance, so it absolutely slower than kernel TCP network stack, also
may slower than use `smoltcp` with hand write event loop.

## Examples:

```rust
use std::future::ready;
use std::net::Ipv4Addr;
use std::os::fd::OwnedFd;
use std::str::FromStr;
use futures_util::{AsyncRead, AsyncWrite, StreamExt, TryStreamExt};
use futures_util::stream::FuturesUnordered;
use futures_util::task::SpawnExt;
use l3_extract::{EventGenerator, Ipv4Cidr, TcpStack};
use futures_timer::Delay;

#[derive(Default)]
struct MyTimer;

impl Timer for MyTimer {
    async fn sleep(&mut self, dur: Duration) {
        Delay::new(dur).await
    }
}

async fn run() {
    let connection = create_layer3_connection();
    let (mut tcp_stack, event_generator, mut tcp_acceptor, mut udp_acceptor) = TcpStack::builder()
        .ipv4_addr(Ipv4Cidr::from_str("192.168.100.10/24").unwrap())
        .ipv4_gateway(Ipv4Addr::new(192, 168, 100, 1))
        .build()
        .unwrap();
    drive_tcp_stack(tcp_stack, connection);
    let mut futs = FuturesUnordered::new();
    futs.spawn(async {
        handle_events(event_generator).await
    }).unwrap();
    futs.spawn(async {
        let tcp_stream = tcp_acceptor.try_next().await.unwrap().unwrap();
        // do something with tcp_stream
    }).unwrap();
    futs.spawn(async {
        let udp_socket = udp_acceptor.try_next().await.unwrap().unwrap();
        // do something with udp_socket
    }).unwrap();

    futs.for_each(|_| ready(())).await;
}

async fn create_layer3_connection() -> OwnedFd {
    // create a layer3 connection
}

async fn handle_events(event_generator: EventGenerator) {
    // handle events
}

async fn drive_tcp_stack(tcp_stack: TcpStack, fd: OwnedFd) {
    // drive tcp stack on your way, such as spawn a background thread
}
```
