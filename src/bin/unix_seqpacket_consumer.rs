use ipc::{cpu_warmup, get_payload};
use std::str::FromStr;
use uds::{UnixSeqpacketConn, UnixSeqpacketListener};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_size = usize::from_str(&args[1]).unwrap();

    core_affinity::set_for_current(core_affinity::CoreId { id: 0 });

    let socket_path = "/tmp/unix_seqpacket.sock";

    // Connect to the server
    //let listener = UnixSeqpacketListener::bind(socket_path).unwrap();
    //let (server, _addr) = listener.accept_unix_addr().unwrap();
    let conn = UnixSeqpacketConn::connect(socket_path).expect("connect to seqpacket listener");

    let (request_data, response_data) = get_payload(data_size);

    cpu_warmup();

    let mut buf = vec![0; data_size];

    loop {
        let n = match conn.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => break, // Connection closed
        };

        #[cfg(debug_assertions)]
        if buf[..n] != request_data[..n] {
            panic!("Didn't receive valid request");
        }

        conn.send(&response_data).unwrap();
    }
}
