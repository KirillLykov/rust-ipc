use {
    crate::{
        ale_ringbuf::{mmap_ring, Packet, PacketReader, PacketWriter},
        get_payload, ExecutionResult, KB,
    },
    std::{
        ffi::CString,
        process::{Child, Command},
        thread::sleep,
        time::{Duration, Instant},
    },
};

pub const SHM_REQUEST: &str = "/shm_ipc_request";
pub const SHM_RESPONSE: &str = "/shm_ipc_response";
pub const RING_CAPACITY: usize = 64;

pub struct AleSender {
    request_writer: PacketWriter<Packet>,
}

impl AleSender {
    pub fn connect() -> Self {
        let req_mmap = unsafe { mmap_ring::<Packet>(SHM_REQUEST, RING_CAPACITY).unwrap() };

        Self {
            request_writer: PacketWriter::new(req_mmap, RING_CAPACITY as u32),
        }
    }

    pub fn send(&mut self, data: &[u8]) -> Result<(), ()> {
        let mut pkt = Packet([0; 1024]);
        pkt.0[..data.len()].copy_from_slice(data);

        loop {
            self.request_writer.sync(true);
            if self.request_writer.write(pkt).is_ok() {
                self.request_writer.commit();
                return Ok(());
            }
            std::thread::sleep(Duration::from_micros(1));
        }
    }
}

pub struct AleFeedback {
    response_reader: PacketReader<Packet>,
}

impl AleFeedback {
    pub fn connect() -> Self {
        let resp_mmap = unsafe { mmap_ring::<Packet>(SHM_RESPONSE, RING_CAPACITY).unwrap() };

        Self {
            response_reader: PacketReader::new(resp_mmap, RING_CAPACITY as u32),
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
        loop {
            self.response_reader.sync(true);
            if let Some(pkt) = self.response_reader.read() {
                buf[..1024].copy_from_slice(&pkt.0);
                self.response_reader.commit();
                return Ok(1024);
            }
            std::thread::sleep(Duration::from_micros(1));
        }
    }
}

pub struct AleRunner {
    child_proc: Option<Child>,
    sender: AleSender,
    feedback: AleFeedback,
    data_size: usize,
    request_data: Vec<u8>,
    response_data: Vec<u8>,
}

impl AleRunner {
    pub fn new(start_child: bool, data_size: usize) -> Self {
        let exe = crate::executable_path("ale_consumer");
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

        let sender = AleSender::connect();
        let feedback = AleFeedback::connect();

        let (request_data, response_data) = get_payload(data_size);

        Self {
            child_proc,
            sender,
            feedback,
            data_size,
            request_data,
            response_data,
        }
    }

    pub fn run(self, n: usize, print: bool) {
        let AleRunner {
            mut child_proc,
            mut sender,
            mut feedback,
            data_size,
            request_data,
            response_data,
        } = self;
        let start = Instant::now();
        let mut sent = 0;
        let scope = std::thread::scope(|s| {
            let sender_handle = s.spawn(|| {
                let mut sent = 0;
                while sent < n {
                    if sender.send(&request_data).is_err() {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    sent += 1;
                }
            });

            let receiver_handle = s.spawn(|| {
                let mut recvd = 0;
                let mut buf = vec![0u8; data_size];
                while recvd < n {
                    let len = loop {
                        match feedback.recv(&mut buf) {
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
                    if buf[..len] != response_data[..len] {
                        for i in 0..len {
                            if buf[i] != response_data[i] {
                                println!("buf[{}]: {} != {}", i, buf[i], response_data[i]);
                            }
                        }
                        panic!("Sent request didn't get expected response");
                    }
                }
            });
            sender_handle.join().unwrap();
            receiver_handle.join().unwrap();
        });
        if print {
            let elapsed = start.elapsed();
            let res = ExecutionResult::new(
                format!("Unix Ale ringbufs - {}KB", data_size / KB).to_string(),
                elapsed,
                n,
            );
            res.print_info();
        }
        // cleanup
        if let Some(ref mut c) = child_proc {
            c.kill().unwrap();
        }
        unsafe {
            let _ = CString::new(SHM_REQUEST).and_then(|cstr| {
                libc::shm_unlink(cstr.as_ptr());
                Ok(())
            });
            let _ = CString::new(SHM_RESPONSE).and_then(|cstr| {
                libc::shm_unlink(cstr.as_ptr());
                Ok(())
            });
        }
    }
}
