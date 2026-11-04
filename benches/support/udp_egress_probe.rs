//! Exact channel send helpers; rejection cost is not node throughput.
use bytes::Bytes;
use std::hint::black_box;
use std::io;
use std::net::SocketAddr;
use std::sync::LazyLock as Lazy;
use std::time::Instant;
use tokio::sync::mpsc;

include!("common.rs");
include!("allocator.rs");
mod baseline;
mod candidate;

trait Sender {
    fn new(address: SocketAddr, tx: mpsc::Sender<DownstreamUdpDatagram>) -> Self;
    fn copied(&self, data: &[u8], target: SocketAddr) -> io::Result<usize>;
    fn owned(&self, data: Bytes, target: SocketAddr) -> io::Result<usize>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Full,
    Closed,
}

struct Case {
    name: String,
    size: usize,
    owned: bool,
    state: State,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (owned, size) in [(false, 64), (false, 1200), (false, 65507), (true, 1200)] {
        for (state, label) in [
            (State::Ready, "ready"),
            (State::Full, "full"),
            (State::Closed, "closed"),
        ] {
            cases.push(Case {
                name: format!("{}-{size}-{label}", if owned { "owned" } else { "copied" }),
                size,
                owned,
                state,
            });
        }
    }
    cases
}

struct Fixture<S> {
    sender: S,
    rx: Option<mpsc::Receiver<DownstreamUdpDatagram>>,
    payload: Bytes,
    peer: SocketAddr,
    listen: SocketAddr,
}

impl<S: Sender> Fixture<S> {
    fn new(case: &Case) -> Self {
        let listen = "127.0.0.1:443".parse().unwrap();
        let peer = "127.0.0.1:53000".parse().unwrap();
        let (tx, rx) = mpsc::channel(1);
        let sender = S::new(listen, tx);
        if case.state == State::Full {
            sender.copied(b"already queued", peer).unwrap();
        }
        let rx = if case.state == State::Closed {
            drop(rx);
            None
        } else {
            Some(rx)
        };
        let payload = Bytes::from(vec![0xa5; case.size]);
        drop(payload.clone()); // Warm Bytes' shared-owner promotion outside counters.
        Self {
            sender,
            rx,
            payload,
            peer,
            listen,
        }
    }

    fn send(&self, case: &Case) -> io::Result<usize> {
        if case.owned {
            self.sender
                .owned(black_box(self.payload.clone()), self.peer)
        } else {
            self.sender.copied(black_box(&self.payload), self.peer)
        }
    }

    fn step(&mut self, case: &Case) {
        let result = self.send(case);
        match case.state {
            State::Ready => {
                assert_eq!(result.unwrap(), case.size);
                let data = self.rx.as_mut().unwrap().try_recv().unwrap();
                black_box(data);
            }
            State::Full => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock),
            State::Closed => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe),
        }
    }
}

fn controls<S: Sender>() {
    println!("case\tstatus\tbytes\towned_pointer_checked\trecovered\tclosed");
    for case in cases() {
        let mut fixture = Fixture::<S>::new(&case);
        let before = udp_activity_now_ms();
        match case.state {
            State::Ready => assert_eq!(fixture.send(&case).unwrap(), case.size),
            State::Full => {
                let error = fixture.send(&case).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                assert_eq!(error.to_string(), "UDP downstream channel is full");
                let queued = fixture.rx.as_mut().unwrap().try_recv().unwrap();
                assert_eq!(queued.payload.as_ref(), b"already queued");
                assert_eq!(fixture.send(&case).unwrap(), case.size);
            }
            State::Closed => {
                let error = fixture.send(&case).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                assert_eq!(error.to_string(), "UDP downstream channel is closed");
            }
        }
        if case.state != State::Closed {
            let data = fixture.rx.as_mut().unwrap().try_recv().unwrap();
            assert_eq!(data.listen_addr, fixture.listen);
            assert_eq!(data.peer_addr, fixture.peer);
            assert_eq!(data.payload, fixture.payload);
            assert!(data.enqueued_ms >= before && data.enqueued_ms <= udp_activity_now_ms());
            if case.owned {
                assert_eq!(data.payload.as_ptr(), fixture.payload.as_ptr());
            }
            assert!(fixture.rx.as_mut().unwrap().try_recv().is_err());
            fixture.rx.take();
        }
        assert_eq!(
            fixture.send(&case).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        println!(
            "{}\tPASS\t{}\t{}\t{}\ttrue",
            case.name,
            case.size,
            case.owned,
            if case.state == State::Full {
                "true"
            } else {
                "n/a"
            }
        );
    }
}

fn timing<S: Sender>() {
    println!("case\tblock\titerations\tnanoseconds");
    for case in cases() {
        let mut fixture = Fixture::<S>::new(&case);
        for _ in 0..1000 {
            fixture.step(&case);
        }
        let iterations = if case.size > 1200 { 5000 } else { 50000 };
        for block in 0..20 {
            let started = Instant::now();
            for _ in 0..iterations {
                fixture.step(&case);
            }
            println!(
                "{}\t{block}\t{iterations}\t{}",
                case.name,
                started.elapsed().as_nanos()
            );
        }
    }
}

#[cfg(feature = "count-allocations")]
fn allocation_counts<S: Sender>() {
    println!("case\titerations\tallocations\treallocations\tbytes\tlargest");
    for case in cases() {
        let mut fixture = Fixture::<S>::new(&case);
        for _ in 0..1000 {
            fixture.step(&case);
        }
        allocations::start();
        for _ in 0..1000 {
            fixture.step(&case);
        }
        let [count, reallocations, bytes, largest] = allocations::finish();
        println!(
            "{}\t1000\t{count}\t{reallocations}\t{bytes}\t{largest}",
            case.name
        );
    }
}

fn run<S: Sender>(mode: &str) {
    match mode {
        "controls" => controls::<S>(),
        "timing" => timing::<S>(),
        #[cfg(feature = "count-allocations")]
        "allocations" => allocation_counts::<S>(),
        _ => panic!("unsupported mode"),
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    match args[2].as_str() {
        "baseline" => run::<baseline::ChannelUdpDownstreamSender>(&args[1]),
        "candidate" => run::<candidate::ChannelUdpDownstreamSender>(&args[1]),
        _ => panic!("unsupported variant"),
    }
}
