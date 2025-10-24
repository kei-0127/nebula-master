use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Arc;

const DEFAULT_CAPACITY: usize = 1500;
const MAX_POOLED_CAPACITY: usize = 32 * 1024;
const PER_THREAD_LIMIT: usize = 16;

thread_local! {
    static PACKET_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
}

fn normalized_capacity(size: usize) -> usize {
    if size == 0 {
        return DEFAULT_CAPACITY;
    }
    let pow2 = size
        .checked_next_power_of_two()
        .unwrap_or(size);
    let mut cap = DEFAULT_CAPACITY.max(pow2);
    if cap > MAX_POOLED_CAPACITY {
        cap = size;
    }
    cap
}

fn release_vec(mut buf: Vec<u8>) {
    if buf.capacity() > MAX_POOLED_CAPACITY {
        return;
    }
    buf.clear();
    PACKET_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < PER_THREAD_LIMIT {
            pool.push(buf);
        }
    });
}

fn take_vec(min_capacity: usize) -> Vec<u8> {
    PACKET_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if let Some(idx) =
            pool.iter().rposition(|buf| buf.capacity() >= min_capacity)
        {
            let mut buf = pool.swap_remove(idx);
            buf.clear();
            buf
        } else {
            Vec::with_capacity(normalized_capacity(min_capacity))
        }
    })
}

// Reusable packet allocation helper backed by a thread-local pool
pub struct PacketPool;

impl PacketPool {
    // Acquire a mutable buffer with at least `min_capacity` bytes
    pub fn acquire(min_capacity: usize) -> PacketBuffer {
        PacketBuffer {
            buf: take_vec(min_capacity),
        }
    }

    // Copy data into a pooled packet, allocating once if needed
    pub fn copy_from_slice(data: &[u8]) -> PooledPacket {
        let mut buf = take_vec(data.len());
        buf.extend_from_slice(data);
        PooledPacket::from_vec(buf)
    }

    // Wrap an existing vector.
    pub fn from_vec(vec: Vec<u8>) -> PooledPacket {
        PooledPacket::from_vec(vec)
    }
}

// RAII wrapper around a pooled `Vec<u8>`
pub struct PacketBuffer {
    buf: Vec<u8>,
}

impl PacketBuffer {
    // Current length of the buffer
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    // Buffer capacity.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    // Ensure the buffer length is at least `len`, zero-filling new bytes
    pub fn ensure_len(&mut self, len: usize) {
        if self.buf.len() < len {
            self.buf.resize(len, 0);
        }
    }

    // Mutable slice over the current contents
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_mut_slice()
    }

    // Finalize into an immutable packet using the current length
    pub fn into_packet(mut self) -> PooledPacket {
        let buf = std::mem::take(&mut self.buf);
        PooledPacket::from_vec(buf)
    }

    // Finalize into an immutable packet using the provided length
    pub fn into_packet_with_len(mut self, len: usize) -> PooledPacket {
        self.buf.truncate(len);
        let buf = std::mem::take(&mut self.buf);
        PooledPacket::from_vec(buf)
    }

    // Copy from slice and finalize into an immutable packet
    pub fn copy_from_slice(mut self, data: &[u8]) -> PooledPacket {
        self.buf.clear();
        self.buf.extend_from_slice(data);
        let buf = std::mem::take(&mut self.buf);
        PooledPacket::from_vec(buf)
    }
}

impl Drop for PacketBuffer {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        if buf.capacity() > 0 {
            release_vec(buf);
        }
    }
}

#[derive(Clone, Debug)]
pub struct PooledPacket {
    inner: Arc<PacketInner>,
}

impl PooledPacket {
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    pub fn as_slice(&self) -> &[u8] {
        self.inner.as_slice()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    // Consume the packet, returning the underlying buffer when unique
    pub fn into_vec(self) -> Vec<u8> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => {
                let mut inner = ManuallyDrop::new(inner);
                unsafe { ptr::read(&inner.buf) }
            }
            Err(shared) => shared.as_slice().to_vec(),
        }
    }

    fn from_vec(buf: Vec<u8>) -> Self {
        Self {
            inner: Arc::new(PacketInner { buf }),
        }
    }
}

impl std::ops::Deref for PooledPacket {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for PooledPacket {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

#[derive(Debug)]
struct PacketInner {
    buf: Vec<u8>,
}

impl PacketInner {
    fn len(&self) -> usize {
        self.buf.len()
    }

    fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    fn as_slice(&self) -> &[u8] {
        self.buf.as_slice()
    }
}

impl Drop for PacketInner {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        release_vec(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooled_packet_reuses_allocation() {
        let ptr = {
            let packet = PacketPool::copy_from_slice(&[1u8, 2, 3, 4]);
            let ptr = packet.as_slice().as_ptr();
            assert_eq!(packet.len(), 4);
            ptr
        };

        let packet = PacketPool::copy_from_slice(&[5u8, 6, 7, 8]);
        assert_eq!(packet.as_slice(), &[5, 6, 7, 8]);
        assert_eq!(packet.len(), 4);
        assert_eq!(packet.as_slice().as_ptr(), ptr);
    }

    #[test]
    fn buffer_handles_larger_packets() {
        let mut buffer = PacketPool::acquire(64);
        buffer.ensure_len(64);
        buffer.as_mut_slice().fill(0xAA);
        let packet = buffer.into_packet_with_len(64);
        assert_eq!(packet.len(), 64);
        assert!(packet.capacity() >= 64);
    }
}
