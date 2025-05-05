use {
    libc::{
        ftruncate, mmap, munmap, shm_open, shm_unlink, MAP_SHARED, O_CREAT, O_RDWR, PROT_READ,
        PROT_WRITE,
    },
    std::{
        ffi::CString,
        io::{self, ErrorKind},
        mem, ptr,
        sync::atomic::{AtomicU32, Ordering},
    },
};

pub struct RingConsumer {
    producer: *mut AtomicU32,
    cached_producer: u32,
    consumer: *mut AtomicU32,
    cached_consumer: u32,
}

impl RingConsumer {
    pub fn new(producer: *mut AtomicU32, consumer: *mut AtomicU32) -> Self {
        Self {
            producer,
            cached_producer: unsafe { (*producer).load(Ordering::Acquire) },
            consumer,
            cached_consumer: unsafe { (*consumer).load(Ordering::Relaxed) },
        }
    }

    #[cfg(test)]
    pub fn available(&self) -> u32 {
        self.cached_producer.wrapping_sub(self.cached_consumer)
    }

    pub fn consume(&mut self) -> Option<u32> {
        if self.cached_consumer == self.cached_producer {
            return None;
        }

        let index = self.cached_consumer;
        self.cached_consumer = self.cached_consumer.wrapping_add(1);
        Some(index)
    }

    pub fn commit(&mut self) {
        unsafe { (*self.consumer).store(self.cached_consumer, Ordering::Release) };
    }

    pub fn sync(&mut self, commit: bool) {
        if commit {
            self.commit();
        }
        self.cached_producer = unsafe { (*self.producer).load(Ordering::Acquire) };
    }
}

pub struct RingProducer {
    producer: *mut AtomicU32,
    cached_producer: u32,
    consumer: *mut AtomicU32,
    cached_consumer: u32,
    size: u32,
}

impl RingProducer {
    pub fn new(producer: *mut AtomicU32, consumer: *mut AtomicU32, size: u32) -> Self {
        Self {
            producer,
            cached_producer: unsafe { (*producer).load(Ordering::Relaxed) },
            consumer,
            cached_consumer: unsafe { (*consumer).load(Ordering::Acquire) },
            size,
        }
    }

    pub fn available(&self) -> u32 {
        self.size
            .saturating_sub(self.cached_producer.wrapping_sub(self.cached_consumer))
    }

    pub fn produce(&mut self) -> Option<u32> {
        if self.available() == 0 {
            return None;
        }

        let index = self.cached_producer;
        self.cached_producer = self.cached_producer.wrapping_add(1);
        Some(index)
    }

    pub fn commit(&mut self) {
        unsafe { (*self.producer).store(self.cached_producer, Ordering::Release) };
    }

    pub fn sync(&mut self, commit: bool) {
        if commit {
            self.commit();
        }
        self.cached_consumer = unsafe { (*self.consumer).load(Ordering::Acquire) };
    }
}

pub struct RingMmap<T> {
    pub mmap: *const u8,
    pub mmap_len: usize,
    pub producer: *mut AtomicU32,
    pub consumer: *mut AtomicU32,
    pub desc: *mut T,
}

// RingMmap is Send/Sync only if used in one-writer / one-reader pattern,
// and synchronization (sync/commit) is enforced externally.
unsafe impl<T: Send> Send for RingMmap<T> {}
unsafe impl<T: Sync> Sync for RingMmap<T> {}

impl<T> Drop for RingMmap<T> {
    fn drop(&mut self) {
        unsafe {
            munmap(self.mmap as *mut _, self.mmap_len);
        }
    }
}

pub unsafe fn mmap_ring<T>(name: &str, size: usize) -> Result<RingMmap<T>, io::Error> {
    const CACHE_LINE_SIZE: usize = 64;

    let entry_size = mem::size_of::<T>();
    let header_size = CACHE_LINE_SIZE * 2; // one full line for each of producer and consumer
    let data_size = size * entry_size;
    let mmap_len = header_size + data_size;

    let cname = CString::new(name)?;
    let fd = shm_open(cname.as_ptr(), O_RDWR | O_CREAT, 0o600);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // resize the shared memory
    if ftruncate(fd, mmap_len as i64) != 0 {
        return Err(io::Error::last_os_error());
    }

    let map_addr = mmap(
        ptr::null_mut(),
        mmap_len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0,
    );
    if map_addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // mmap layout:
    let producer = map_addr as *mut AtomicU32;
    let consumer = map_addr.add(CACHE_LINE_SIZE) as *mut AtomicU32;

    let desc = map_addr.add(header_size) as *mut T;
    Ok(RingMmap {
        mmap: map_addr as *const u8,
        mmap_len,
        producer,
        consumer,
        desc,
    })
}

pub struct PacketWriter<T: Copy> {
    mmap: RingMmap<T>,
    producer: RingProducer,
    size: u32,
}

impl<T: Copy> PacketWriter<T> {
    pub fn new(mmap: RingMmap<T>, size: u32) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            producer: RingProducer::new(mmap.producer, mmap.consumer, size),
            mmap,
            size,
        }
    }

    pub fn write(&mut self, packet: T) -> Result<(), io::Error> {
        let Some(index) = self.producer.produce() else {
            return Err(ErrorKind::StorageFull.into());
        };
        // size is always power of two.
        let index = index & self.size.saturating_sub(1);
        let desc = unsafe { self.mmap.desc.add(index as usize) };
        // Safety: index is within the ring so the pointer is valid
        unsafe {
            desc.write(packet);
        }

        Ok(())
    }

    pub fn commit(&mut self) {
        self.producer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.producer.sync(commit);
    }
}

pub struct PacketReader<T: Copy> {
    mmap: RingMmap<T>,
    consumer: RingConsumer,
    size: u32,
}

impl<T: Copy> PacketReader<T> {
    pub fn new(mmap: RingMmap<T>, size: u32) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            consumer: RingConsumer::new(mmap.producer, mmap.consumer),
            mmap,
            size,
        }
    }

    pub fn read(&mut self) -> Option<T> {
        let index = self.consumer.consume()? & self.size.saturating_sub(1);
        let data = unsafe { *self.mmap.desc.add(index as usize) };
        Some(data)
    }

    pub fn commit(&mut self) {
        self.consumer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.consumer.sync(commit);
    }
}

unsafe impl<T: Copy + Send> Send for PacketWriter<T> {}
unsafe impl<T: Copy + Send> Send for PacketReader<T> {}

#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Packet(pub [u8; 1024]);

#[cfg(test)]
mod test {
    use {
        super::*,
        std::{thread, time::Duration},
    };

    #[test]
    fn test_ring_producer() {
        let mut producer = AtomicU32::new(0);
        let mut consumer = AtomicU32::new(0);
        let size = 16;
        let mut ring = RingProducer::new(&mut producer as *mut _, &mut consumer as *mut _, size);
        assert_eq!(ring.available(), size);

        for i in 0..size {
            assert_eq!(ring.produce(), Some(i));
            assert_eq!(ring.available(), size - i - 1);
        }
        assert_eq!(ring.produce(), None);

        consumer.store(1, Ordering::Release);
        assert_eq!(ring.produce(), None);
        ring.commit();
        assert_eq!(ring.produce(), None);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(16));
        assert_eq!(ring.produce(), None);

        consumer.store(2, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(17));
    }

    #[test]
    fn test_ring_producer_wrap_around() {
        let size = 16;
        let mut producer = AtomicU32::new(u32::MAX - 1);
        let mut consumer = AtomicU32::new(u32::MAX - size - 1);
        let mut ring = RingProducer::new(&mut producer as *mut _, &mut consumer as *mut _, size);
        assert_eq!(ring.available(), 0);

        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(u32::MAX - 1));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(u32::MAX));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(0));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(1));
    }

    #[test]
    fn test_ring_consumer() {
        let mut producer = AtomicU32::new(0);
        let mut consumer = AtomicU32::new(0);
        let size = 16;
        let mut ring = RingConsumer::new(&mut producer as *mut _, &mut consumer as *mut _);
        assert_eq!(ring.available(), 0);

        producer.store(1, Ordering::Release);
        assert_eq!(ring.available(), 0);
        ring.sync(true);
        assert_eq!(ring.available(), 1);

        producer.store(size, Ordering::Release);
        ring.sync(true);

        for i in 0..size {
            assert_eq!(ring.consume(), Some(i));
            assert_eq!(ring.available(), size - i - 1);
        }
        assert_eq!(ring.consume(), None);
    }

    #[test]
    fn test_ring_consumer_wrap_around() {
        let mut producer = AtomicU32::new(u32::MAX - 1);
        let mut consumer = AtomicU32::new(u32::MAX - 1);
        let mut ring = RingConsumer::new(&mut producer as *mut _, &mut consumer as *mut _);
        assert_eq!(ring.available(), 0);
        assert_eq!(ring.consume(), None);

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(u32::MAX - 1));

        producer.store(0, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.available(), 1);
        assert_eq!(ring.consume(), Some(u32::MAX));

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(0));

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(1));
    }

    #[test]
    fn test_shared_ring_write_read() {
        const RING_NAME: &str = "/test-shm-ring";
        const CAPACITY: usize = 64;

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            // child process = writer
            let mmap = unsafe { mmap_ring::<Packet>(RING_NAME, CAPACITY).unwrap() };
            let mut writer = PacketWriter::new(mmap, CAPACITY as u32);

            let packet = Packet([0xAB; 1024]);

            for _ in 0..100 {
                loop {
                    writer.sync(true);
                    if writer.write(packet).is_ok() {
                        writer.commit();
                        break;
                    }
                    thread::sleep(Duration::from_micros(100));
                }
            }

            std::process::exit(0);
        } else {
            // parent process = reader
            thread::sleep(Duration::from_millis(50)); // allow child to write first
            let mmap = unsafe { mmap_ring::<Packet>(RING_NAME, CAPACITY).unwrap() };
            let mut reader = PacketReader::new(mmap, CAPACITY as u32);

            let expected = Packet([0xAB; 1024]);
            let mut read_count = 0;

            let mut num_attempts = 0;
            while read_count < 100 && num_attempts < 10 {
                reader.sync(true);
                if let Some(pkt) = reader.read() {
                    assert_eq!(pkt, expected);
                    read_count += 1;
                } else {
                    num_attempts += 1;
                    thread::sleep(Duration::from_micros(100));
                }
                println!("{read_count}");
                reader.commit();
            }
            assert_eq!(read_count, 100);
        }
    }
}
