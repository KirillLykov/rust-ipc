use std::{
    fs,
    os::{
        fd::{FromRawFd, IntoRawFd},
        unix::net::UnixListener, // only for waiting connection
    },
    process::{Child, Command},
    thread::sleep,
    time::{Duration, Instant},
};

use crate::{get_payload, ExecutionResult, KB};
use uds::{UnixSeqpacketConn, UnixSeqpacketListener};

const UNIX_SOCKET_PATH: &str = "/tmp/unix_seqpacket.sock";

pub struct UnixSeqpacketWrapper {
    pub stream: UnixSeqpacketConn,
}

impl UnixSeqpacketWrapper {
    pub fn unix_connect() -> Self {
        let stream = UnixSeqpacketConn::connect(UNIX_SOCKET_PATH).unwrap();
        stream.set_nonblocking(true).unwrap();
        Self { stream }
    }
    pub fn from_listener(listener: UnixSeqpacketListener) -> Self {
        let (server, _addr) = listener.accept_unix_addr().unwrap();
        server.set_nonblocking(true).unwrap();
        Self { stream: server }
    }
}

pub struct UnixSeqpacketRunner {
    child_proc: Option<Child>,
    wrapper: UnixSeqpacketWrapper,
    data_size: usize,
    request_data: Vec<u8>,
    response_data: Vec<u8>,
}

impl UnixSeqpacketRunner {
    pub fn new(start_child: bool, data_size: usize) -> Self {
        let _ = fs::remove_file(UNIX_SOCKET_PATH); // Clean stale socket
                                                   //let unix_listener = UnixSeqpacketListener::bind(UNIX_SOCKET_PATH).unwrap();
        let listener =
            UnixSeqpacketListener::bind(UNIX_SOCKET_PATH).expect("create seqpacket listener");

        let exe = crate::executable_path("unix_seqpacket_consumer");
        let child_proc = if start_child {
            let res = Some(
                Command::new(exe)
                    .args(&[data_size.to_string()])
                    .spawn()
                    .unwrap_or_else(|e| panic!("Failed to spawn {e}")),
            );
            sleep(Duration::from_secs(2));
            res
        } else {
            None
        };

        let wrapper = UnixSeqpacketWrapper::from_listener(listener);

        let (request_data, response_data) = get_payload(data_size);

        Self {
            child_proc,
            wrapper,
            data_size,
            request_data,
            response_data,
        }
    }

    pub fn run(&mut self, n: usize, print: bool) {
        let start = Instant::now();
        let mut sent = 0;
        let scope = std::thread::scope(|s| {
            s.spawn(|| {
                let mut sent = 0;
                while sent < n {
                    if self.wrapper.stream.send(&self.request_data).is_err() {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    sent += 1;
                }
            });

            s.spawn(|| {
                let mut recvd = 0;
                let mut buf = vec![0u8; self.data_size];
                while recvd < n {
                    let len = loop {
                        match self.wrapper.stream.recv(&mut buf) {
                            Ok(l) => {
                                break l;
                            }
                            Err(_) => {
                                std::thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                        }
                    };
                    recvd += 1;

                    #[cfg(debug_assertions)]
                    if buf[..len] != self.response_data[..len] {
                        panic!("Sent request didn't get expected response");
                    }
                }
            });
        });
        if print {
            let elapsed = start.elapsed();
            let res = ExecutionResult::new(
                format!("Unix SEQPACKET Socket - {}KB", self.data_size / KB).to_string(),
                elapsed,
                n,
            );
            res.print_info();
        }
    }
}

impl Drop for UnixSeqpacketRunner {
    fn drop(&mut self) {
        if let Some(ref mut c) = self.child_proc {
            c.kill().unwrap();
        }
        let _ = fs::remove_file(UNIX_SOCKET_PATH);
    }
}
