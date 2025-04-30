use {
    ipc::{
        ale_ringbuf::{mmap_ring, Packet, PacketReader, PacketWriter},
        ale_runner::{RING_CAPACITY, SHM_REQUEST, SHM_RESPONSE},
        cpu_warmup, get_payload,
    },
    std::{
        io::{Read, Write},
        str::FromStr,
        time::Duration,
    },
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_size = usize::from_str(&args[1]).unwrap();

    core_affinity::set_for_current(core_affinity::CoreId { id: 0 });

    let req_mmap = unsafe { mmap_ring::<Packet>(SHM_REQUEST, RING_CAPACITY).unwrap() };
    let resp_mmap = unsafe { mmap_ring::<Packet>(SHM_RESPONSE, RING_CAPACITY).unwrap() };

    let mut reader = PacketReader::new(req_mmap, RING_CAPACITY as u32);
    let mut writer = PacketWriter::new(resp_mmap, RING_CAPACITY as u32);

    let (request_data, response_data) = get_payload(data_size);
    cpu_warmup();

    loop {
        reader.sync(true);
        if let Some(pkt) = reader.read() {
            #[cfg(debug_assertions)]
            assert_eq!(&pkt.0[..data_size], &request_data[..data_size]);

            reader.commit();

            let mut response = Packet([0; 1024]);
            response.0[..data_size].copy_from_slice(&response_data);

            loop {
                writer.sync(true);
                if writer.write(response).is_ok() {
                    writer.commit();
                    break;
                }
                std::thread::sleep(Duration::from_micros(1));
            }
        } else {
            std::thread::sleep(Duration::from_micros(1));
        }
    }
}
