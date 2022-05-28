use std::mem::MaybeUninit;
#[cfg(not(feature = "loom"))]
use std::{
    cell::UnsafeCell,
    hint,
    sync::atomic::{AtomicUsize, Ordering},
};
#[cfg(feature = "loom")]
use std::{
    cell::UnsafeCell,
    hint,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Fast MPMC queue.
pub struct SynQueue<T> {
    m1: AtomicUsize,
    m2: AtomicUsize,
    data: Box<[MaybeUninit<UnsafeCell<T>>]>,
}

unsafe impl<T> Sync for SynQueue<T> {}

impl<T> SynQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            /// State used first on push, last on pop.
            m1: AtomicUsize::new(0),
            /// State used first on pop, last on push.
            m2: AtomicUsize::new(0),
            /// In order to differentiate between empty and full states, we
            /// are never going to use the full array, so get one extra element.
            data: (0..=capacity).map(|_| MaybeUninit::uninit()).collect(),
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        // acqure a new position first
        let mut state = self.m1.load(Ordering::Acquire);
        let head = loop {
            let head = state & 0xFFFFFFFF;
            let tail = state >> 32;
            let next = if head + 1 == self.data.len() {
                0
            } else {
                head + 1
            };
            if next == tail {
                return Err(value);
            }
            match self.m1.compare_exchange_weak(
                state,
                next | (tail << 32),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                // write the value on success
                Ok(_) => {
                    unsafe { UnsafeCell::raw_get(self.data[head].as_ptr()).write(value) };
                    break next;
                }
                Err(other) => {
                    state = other;
                }
            }
            hint::spin_loop();
        };
        // advance the secondary state
        state = self.m2.load(Ordering::Acquire);
        while let Err(other) = self.m2.compare_exchange_weak(
            state,
            (state & !0xFFFFFFFF) | head,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            hint::spin_loop();
            state = other;
        }
        Ok(())
    }

    pub fn pop(&self) -> Option<T> {
        let mut state = self.m2.load(Ordering::Acquire);
        let (value, tail) = loop {
            let head = state & 0xFFFFFFFF;
            let tail = state >> 32;
            if head == tail {
                return None;
            }
            let next = if tail + 1 == self.data.len() {
                0
            } else {
                tail + 1
            };
            match self.m2.compare_exchange_weak(
                state,
                head | (next << 32),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                // extract the value on success
                Ok(_) => {
                    let value = unsafe { self.data[tail].assume_init_read().into_inner() };
                    break (value, next);
                }
                Err(other) => {
                    state = other;
                }
            }
            hint::spin_loop();
        };
        // advance the primary state
        state = self.m1.load(Ordering::Acquire);
        while let Err(other) = self.m1.compare_exchange_weak(
            state,
            (state & 0xFFFFFFFF) | (tail << 32),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            hint::spin_loop();
            state = other;
        }
        Some(value)
    }
}

impl<T> Drop for SynQueue<T> {
    fn drop(&mut self) {
        let state = self.m1.load(Ordering::Acquire);
        assert_eq!(state, self.m2.load(Ordering::Acquire));
        let head = state & 0xFFFFFFFF;
        let mut tail = state >> 32;
        while tail != head {
            unsafe { self.data[tail].assume_init_drop() };
            if tail + 1 == self.data.len() {
                tail = 0;
            } else {
                tail += 1;
            };
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod loom {
    pub fn model(mut fun: impl FnMut()) {
        fun();
    }
}

#[test]
fn overflow() {
    loom::model(|| {
        let sq = SynQueue::<i32>::new(1);
        sq.push(2).unwrap();
        assert_eq!(sq.push(3), Err(3));
    })
}

#[test]
fn smoke() {
    loom::model(|| {
        let sq = SynQueue::<i32>::new(10);
        assert_eq!(sq.pop(), None);
        sq.push(5).unwrap();
        sq.push(10).unwrap();
        assert_eq!(sq.pop(), Some(5));
        assert_eq!(sq.pop(), Some(10));
    })
}

#[test]
fn barrage() {
    #[cfg(feature = "loom")]
    use loom::{sync::Arc, thread};
    #[cfg(not(feature = "loom"))]
    use std::{sync::Arc, thread};

    loom::model(|| {
        const NUM_THREADS: usize = 8;
        const NUM_ELEMENTS: usize = 10000;
        let sq = Arc::new(SynQueue::<usize>::new(NUM_ELEMENTS));
        let mut handles = Vec::new();

        for _ in 0..NUM_THREADS {
            let sq2 = Arc::clone(&sq);
            handles.push(thread::spawn(move || {
                for i in 0..NUM_ELEMENTS {
                    let _ = sq2.push(i);
                }
            }));
        }
        for _ in 0..NUM_THREADS {
            let sq3 = Arc::clone(&sq);
            handles.push(thread::spawn(move || {
                for _ in 0..NUM_ELEMENTS {
                    let _ = sq3.pop();
                }
            }));
        }

        for jt in handles {
            let _ = jt.join();
        }
    })
}
