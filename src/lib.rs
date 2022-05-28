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
type Pointer = u32;
const _BITS_CHECK: usize = (mem::size_of::<usize>() == 2 * mem::size_of::<Pointer>()) as usize - 1;

#[derive(Clone, Copy, Debug)]
struct State {
    head: Pointer,
    tail: Pointer,
}
impl State {
    const HEAD_BITS: usize = mem::size_of::<Pointer>() * 8;
    #[inline(always)]
    fn unpack(raw: usize) -> Self {
        Self {
            head: raw as Pointer,
            tail: (raw >> Self::HEAD_BITS) as Pointer,
        }
    }
    #[inline(always)]
    fn pack(self) -> usize {
        (self.head as usize) | ((self.tail as usize) << Self::HEAD_BITS)
    }
}

/// Fast internally syncrhonized MPMC queue.
///
/// ## Principle
/// The basic problem with MPMC queues on arrays is synchronizing
/// access to data with shifting of head/tail pointers.
/// To address this, a general solution (you can find in `crossbeam-queue` and others)
/// is to add one atomic bit per element, which serves as a barrier for advancing
/// the queue pointers.
///
/// `SynQueue` tries a different approach. There is no extra bits.
/// Instead, we are keeping 2 representations of the queue state: wide and narrow.
/// Pushing advances `wide.head`, writes the data, and then makes `narrow.head` to catch up.
/// Popping advances `narrow.tail`, reads the data, and then makes `wide.tail` to catch up.
/// Every operation is thus sequence of CAS loop, data operation, another CAS loop.
///
/// ## Internal invariants.
/// Considering an infinite sequence (without wraparounds):
///  `wide.tail <= narrow.tail <= narrow.head <= wide.head`
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

    fn advance(&self, index: Pointer) -> Pointer {
        if index as usize + 1 == self.data.len() {
            0
        } else {
            index + 1
        }
    }

    pub fn push(&self, value: T) -> Result<(), T> {
        // acqure a new position within the wide state
        let mut state = self.wide.load(LOAD_ORDER);
        let (head, next) = loop {
            //println!("Push pre-CAS: {:x}", state);
            let s = State::unpack(state);
            let next = self.advance(s.head);
            if next == s.tail {
                return Err(value);
            }
            match self.wide.compare_exchange_weak(
                state,
                State { head: next, ..s }.pack(),
                CAS_ORDER,
                LOAD_ORDER,
            ) {
                Ok(_) => break (s.head, next),
                Err(other) => state = other,
            }
            hint::spin_loop();
        };

        // write the data
        unsafe { UnsafeCell::raw_get(self.data[head as usize].as_ptr()).write(value) };

        // advance the narrow state
        state = self.narrow.load(LOAD_ORDER);
        let mut tail = State::unpack(state).tail;
        while let Err(other) = self.narrow.compare_exchange_weak(
            State { head, tail }.pack(),
            State { head: next, tail }.pack(),
            CAS_ORDER,
            LOAD_ORDER,
        ) {
            //println!("Push post-CAS: {:x}", other);
            hint::spin_loop();
            tail = State::unpack(other).tail;
        }
        Ok(())
    }

    pub fn pop(&self) -> Option<T> {
        // acquire the oldest position within the narrow state
        let mut state = self.narrow.load(LOAD_ORDER);
        let (tail, next) = loop {
            //println!("Pop pre-CAS: {:x}", state);
            let s = State::unpack(state);
            if s.head == s.tail {
                return None;
            }
            let next = self.advance(s.tail);
            match self.narrow.compare_exchange_weak(
                state,
                State { tail: next, ..s }.pack(),
                CAS_ORDER,
                LOAD_ORDER,
            ) {
                Ok(_) => break (s.tail, next),
                Err(other) => state = other,
            }
            hint::spin_loop();
        };

        // read the data
        let value = unsafe { self.data[tail as usize].assume_init_read().into_inner() };

        // advance the wide state
        state = self.wide.load(LOAD_ORDER);
        let mut head = State::unpack(state).head;
        while let Err(other) = self.wide.compare_exchange_weak(
            State { head, tail }.pack(),
            State { head, tail: next }.pack(),
            CAS_ORDER,
            LOAD_ORDER,
        ) {
            //println!("Pop post-CAS: {:x}", other);
            hint::spin_loop();
            head = State::unpack(other).head;
        }
        Some(value)
    }
}

impl<T> Drop for SynQueue<T> {
    fn drop(&mut self) {
        let state = self.wide.load(LOAD_ORDER);
        //println!("Drop state: {:x}", state);
        assert_eq!(state, self.narrow.load(LOAD_ORDER));
        let s = State::unpack(state);
        let mut cursor = s.tail;
        while cursor != s.head {
            unsafe { self.data[cursor as usize].assume_init_drop() };
            cursor = self.advance(cursor);
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
