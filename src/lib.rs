use std::mem;
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

const CAS_ORDER: Ordering = Ordering::AcqRel;
const LOAD_ORDER: Ordering = Ordering::Acquire;
const HEAD_BITS: usize = 32;
const HEAD_MASK: usize = (1 << HEAD_BITS) - 1;
const _BITS_CHECK: usize = (mem::size_of::<usize>() * 8 == HEAD_BITS * 2) as usize - 1;

/*
struct Snake {
    head: u32,
    tail: u32,
}
fn Snake {
    fn unpack(raw: usize) -> Self {
        Self {
            head: state as u32,
            tail: (state >> HEAD_BITS) as u32,
        }
    }
    fn pack(self) -> usize {
        (self.head as usize) | (self.tail as usize << HEAD_BITS)
    }
}*/

/// Fast MPMC queue.
pub struct SynQueue<T> {
    wide: AtomicUsize,
    narrow: AtomicUsize,
    data: Box<[mem::MaybeUninit<UnsafeCell<T>>]>,
}

unsafe impl<T> Sync for SynQueue<T> {}

impl<T> SynQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            /// State used first on push, last on pop.
            wide: AtomicUsize::new(0),
            /// State used first on pop, last on push.
            narrow: AtomicUsize::new(0),
            /// In order to differentiate between empty and full states, we
            /// are never going to use the full array, so get one extra element.
            data: (0..=capacity).map(|_| mem::MaybeUninit::uninit()).collect(),
        }
    }

    fn advance(&self, index: usize) -> usize {
        if index as usize + 1 == self.data.len() {
            0
        } else {
            index + 1
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        // acqure a new position first
        let mut state = self.wide.load(LOAD_ORDER);
        let (head, next) = loop {
            //println!("Push pre-CAS: {:x}", state);
            let head = state & HEAD_MASK;
            let tail = state >> HEAD_BITS;
            let next = self.advance(head);
            if next == tail {
                return Err(value);
            }
            match self.wide.compare_exchange_weak(
                state,
                next | (tail << HEAD_BITS),
                CAS_ORDER,
                LOAD_ORDER,
            ) {
                // write the value on success
                Ok(_) => {
                    unsafe { UnsafeCell::raw_get(self.data[head].as_ptr()).write(value) };
                    break (head, next);
                }
                Err(other) => {
                    state = other;
                }
            }
            hint::spin_loop();
        };
        // advance the narrow state
        state = self.narrow.load(LOAD_ORDER);
        let mut tail_high = state & !HEAD_MASK;
        while let Err(other) = self.narrow.compare_exchange_weak(
            tail_high | head,
            tail_high | next,
            CAS_ORDER,
            LOAD_ORDER,
        ) {
            //println!("Push post-CAS: {:x}", other);
            hint::spin_loop();
            tail_high = other & !HEAD_MASK;
        }
        Ok(())
    }

    pub fn pop(&self) -> Option<T> {
        let mut state = self.narrow.load(LOAD_ORDER);
        let (value, tail, next) = loop {
            //println!("Pop pre-CAS: {:x}", state);
            let head = state & HEAD_MASK;
            let tail = state >> HEAD_BITS;
            if head == tail {
                return None;
            }
            let next = if tail + 1 == self.data.len() {
                0
            } else {
                tail + 1
            };
            match self.narrow.compare_exchange_weak(
                state,
                head | (next << HEAD_BITS),
                CAS_ORDER,
                LOAD_ORDER,
            ) {
                // extract the value on success
                Ok(_) => {
                    let value = unsafe { self.data[tail].assume_init_read().into_inner() };
                    break (value, tail, next);
                }
                Err(other) => {
                    state = other;
                }
            }
            hint::spin_loop();
        };
        // advance the wide state
        state = self.wide.load(LOAD_ORDER);
        let mut head = state & HEAD_MASK;
        while let Err(other) = self.wide.compare_exchange_weak(
            head | (tail << HEAD_BITS),
            head | (next << HEAD_BITS),
            CAS_ORDER,
            LOAD_ORDER,
        ) {
            //println!("Pop post-CAS: {:x}", other);
            hint::spin_loop();
            head = other & HEAD_MASK;
        }
        Some(value)
    }
}

impl<T> Drop for SynQueue<T> {
    fn drop(&mut self) {
        let state = self.wide.load(LOAD_ORDER);
        //println!("Drop state: {:x}", state);
        assert_eq!(state, self.narrow.load(LOAD_ORDER));
        let head = state & HEAD_MASK;
        let mut tail = state >> HEAD_BITS;
        while tail != head {
            unsafe { self.data[tail].assume_init_drop() };
            tail += 1;
            if tail == self.data.len() {
                tail = 0;
            }
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
        const NUM_THREADS: usize = if cfg!(miri) { 2 } else { 8 };
        const NUM_ELEMENTS: usize = if cfg!(miri) { 100 } else { 10000 };
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
