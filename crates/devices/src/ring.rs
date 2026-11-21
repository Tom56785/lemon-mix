//! A lock-free single-producer, single-consumer ring of stereo samples,
//! between a device thread and the engine.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One stereo sample.
pub type Stereo = [f32; 2];

/// The ring itself. Exactly one thread may push and exactly one may pop at a
/// time; the ports that own rings guarantee it. Neither side blocks or
/// allocates.
pub struct Ring {
    buffer: Box<[UnsafeCell<Stereo>]>,
    /// Total samples ever pushed and popped; the difference is the fill.
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: slots between tail and head belong to the consumer, the others to
// the producer; the release/acquire pairs on head and tail hand them over.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub fn new(capacity: usize) -> Ring {
        Ring {
            buffer: (0..capacity.max(1)).map(|_| UnsafeCell::new([0.0; 2])).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Samples ready to pop.
    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Producer: appends as many samples as fit and returns how many did.
    pub fn push(&self, samples: &[Stereo]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = self.capacity() - head.wrapping_sub(tail);
        let n = samples.len().min(free);
        for (i, sample) in samples[..n].iter().enumerate() {
            let slot = head.wrapping_add(i) % self.capacity();
            // SAFETY: the slot is free, so only the producer touches it.
            unsafe { *self.buffer[slot].get() = *sample };
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: removes up to `out.len()` samples into `out` and returns how
    /// many it removed.
    pub fn pop(&self, out: &mut [Stereo]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let n = out.len().min(head.wrapping_sub(tail));
        for (i, sample) in out[..n].iter_mut().enumerate() {
            let slot = tail.wrapping_add(i) % self.capacity();
            // SAFETY: the slot is filled, so only the consumer touches it.
            *sample = unsafe { *self.buffer[slot].get() };
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: discards everything queued.
    pub fn clear(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn pushes_and_pops_in_order_and_wraps() {
        let ring = Ring::new(5);
        assert_eq!(ring.push(&[[1.0, 1.0], [2.0, 2.0], [3.0, 3.0]]), 3);
        let mut out = [[0.0; 2]; 2];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out, [[1.0, 1.0], [2.0, 2.0]]);
        assert_eq!(ring.push(&[[4.0; 2], [5.0; 2], [6.0; 2], [7.0; 2], [8.0; 2]]), 4, "only the free space");
        let mut all = [[0.0; 2]; 8];
        assert_eq!(ring.pop(&mut all), 5);
        assert_eq!(all[..5].iter().map(|s| s[0]).collect::<Vec<_>>(), [3.0, 4.0, 5.0, 6.0, 7.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn threads_see_every_sample_once() {
        let ring = Arc::new(Ring::new(64));
        let producer = Arc::clone(&ring);
        let thread = std::thread::spawn(move || {
            let mut next = 0u32;
            while next < 100_000 {
                let batch: Vec<Stereo> = (next..next + 7).map(|v| [v as f32, -(v as f32)]).collect();
                next += producer.push(&batch) as u32;
            }
        });
        let mut expected = 0u32;
        let mut out = [[0.0; 2]; 13];
        while expected < 100_000 {
            let n = ring.pop(&mut out);
            for sample in &out[..n] {
                assert_eq!(*sample, [expected as f32, -(expected as f32)]);
                expected += 1;
            }
        }
        thread.join().unwrap();
    }
}
