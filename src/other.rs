use std::mem;
#[cfg(not(feature = "loom"))]
use std::{cell::UnsafeCell, hint, sync::atomic::AtomicUsize, thread};
#[cfg(feature = "loom")]
use std::{cell::UnsafeCell, hint, sync::atomic::AtomicUsize, thread};

const INDEX_BITS: usize = 20;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;
const TOTAL_BITS: usize = mem::size_of::<usize>() * 8;

/// Another internally syncrhonized (MPMC) queue.
///
/// ## Principle
/// This is a hybrid between the bitmask approach of crossbeam and the `SynQueue`.
/// It maintans the mask as a part of the atomic, keeping head and tail separate.
/// This makes `OtherQueue` to also do 2 CAS operations every time, but unlike
/// `SynQueue` the bit releases can complete out of order.
pub struct OtherQueue<T> {
    head: AtomicUsize,
    tail: AtomicUsize,
    data: Box<[mem::MaybeUninit<UnsafeCell<T>>]>,
}

unsafe impl<T> Sync for OtherQueue<T> {}

enum BoundsCheck {
    OldValue,
    NewValue,
}

impl<T> OtherQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two());
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            /// In order to differentiate between empty and full states, we
            /// are never going to use the full array, so get one extra element.
            data: (0..=capacity).map(|_| mem::MaybeUninit::uninit()).collect(),
        }
    }

    fn get_last_used_index(&self, rich_index: usize) -> usize {
        let index = rich_index & INDEX_MASK;
        let offset = (TOTAL_BITS - INDEX_BITS).saturating_sub(rich_index.leading_zeros() as usize);
        if index >= offset {
            index - offset
        } else {
            index + self.data.len() - offset
        }
    }

    fn cas_acquire(
        &self,
        main_ref: &AtomicUsize,
        guard_ref: &AtomicUsize,
        bounds_check: BoundsCheck,
    ) -> Option<(usize, usize)> {
        let mut guard = guard_ref.load(super::LOAD_ORDER);
        let mut last_used_index = self.get_last_used_index(guard);
        let mut main = main_ref.load(super::LOAD_ORDER);
        let mut next;
        loop {
            while main >= (1 << (TOTAL_BITS - 1)) {
                // too many operations in flight
                thread::yield_now();
                main = main_ref.load(super::LOAD_ORDER);
            }

            next = ((main & !INDEX_MASK) << 1) | (1 << INDEX_BITS);
            if (main & INDEX_MASK) + 1 != self.data.len() {
                next |= (main & INDEX_MASK) + 1;
            };

            let check_index = match bounds_check {
                BoundsCheck::OldValue => main & INDEX_MASK,
                BoundsCheck::NewValue => next & INDEX_MASK,
            };
            if check_index == last_used_index {
                guard = guard_ref.load(super::LOAD_ORDER);
                last_used_index = self.get_last_used_index(guard);
                if check_index == last_used_index {
                    return None;
                }
            }

            match main_ref.compare_exchange_weak(main, next, super::CAS_ORDER, super::LOAD_ORDER) {
                Ok(_) => break,
                Err(other) => {
                    main = other;
                }
            }
            hint::spin_loop();
        }
        Some((main & INDEX_MASK, next))
    }

    fn cas_release(&self, atomic_ref: &AtomicUsize, mut current: usize, done_index: usize) {
        loop {
            let cur_index = current & INDEX_MASK;
            let offset = if cur_index > done_index {
                cur_index - done_index
            } else {
                cur_index + self.data.len() - done_index
            };
            assert!(offset + INDEX_BITS <= TOTAL_BITS);
            let bit = 1 << (INDEX_BITS - 1 + offset);
            assert!(current & bit != 0);
            match atomic_ref.compare_exchange_weak(
                current,
                current ^ bit,
                super::CAS_ORDER,
                super::LOAD_ORDER,
            ) {
                Ok(_) => break,
                Err(other) => {
                    current = other;
                    hint::spin_loop();
                }
            }
        }
    }

    #[profiling::function]
    pub fn push(&self, value: T) -> Result<(), T> {
        let (index, next) = match self.cas_acquire(&self.head, &self.tail, BoundsCheck::NewValue) {
            Some(pair) => pair,
            None => return Err(value),
        };
        unsafe { UnsafeCell::raw_get(self.data[index].as_ptr()).write(value) };
        self.cas_release(&self.head, next, index);
        return Ok(());
    }

    #[profiling::function]
    pub fn pop(&self) -> Option<T> {
        let (index, next) = self.cas_acquire(&self.tail, &self.head, BoundsCheck::OldValue)?;
        let value = unsafe { self.data[index].assume_init_read().into_inner() };
        self.cas_release(&self.tail, next, index);
        Some(value)
    }
}

impl<T> Drop for OtherQueue<T> {
    fn drop(&mut self) {
        let head = self.head.load(super::LOAD_ORDER);
        let tail = self.tail.load(super::LOAD_ORDER);
        assert_eq!(head & !INDEX_MASK, 0);
        assert_eq!(tail & !INDEX_MASK, 0);
        let mut cursor = tail;
        while cursor != head {
            unsafe { self.data[cursor].assume_init_drop() };
            cursor += 1;
            if cursor == self.data.len() {
                cursor = 0;
            }
        }
    }
}

#[test]
fn overflow() {
    let sq = OtherQueue::<i32>::new(2);
    sq.push(2).unwrap();
    sq.push(3).unwrap();
    assert_eq!(sq.push(4), Err(4));
}

#[test]
fn smoke() {
    let sq = OtherQueue::<i32>::new(16);
    assert_eq!(sq.pop(), None);
    sq.push(5).unwrap();
    sq.push(10).unwrap();
    assert_eq!(sq.pop(), Some(5));
    assert_eq!(sq.pop(), Some(10));
}

#[test]
fn barrage() {
    #[cfg(feature = "loom")]
    use loom::sync::Arc;
    #[cfg(not(feature = "loom"))]
    use std::sync::Arc;

    const NUM_THREADS: usize = if cfg!(miri) { 2 } else { 8 };
    const NUM_ELEMENTS: usize = if cfg!(miri) { 1 << 7 } else { 1 << 17 };
    let sq = Arc::new(OtherQueue::<usize>::new(NUM_ELEMENTS));
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
}
