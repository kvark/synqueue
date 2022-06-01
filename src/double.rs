use super::qstd::{cell::UnsafeCell, hint, sync::atomic::AtomicUsize, thread};
use std::mem;

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

/// An internally syncrhonized (MPMC) queue.
///
/// ## Principle
/// The basic problem with MPMC queues on arrays is synchronizing
/// access to data with shifting of head/tail pointers.
/// To address this, a general solution (you can find in `crossbeam-queue` and others)
/// is to add one atomic bit per element, which serves as a barrier for advancing
/// the queue pointers.
///
/// `DoubleQueue` tries a different approach. There is no extra bits.
/// Instead, we are keeping 2 representations of the queue state: wide and narrow.
/// Pushing advances `wide.head`, writes the data, and then makes `narrow.head` to catch up.
/// Popping advances `narrow.tail`, reads the data, and then makes `wide.tail` to catch up.
/// Every operation is thus sequence of CAS loop, data operation, another CAS loop.
///
/// ## Internal invariants.
/// Considering an infinite sequence (without wraparounds):
///  `wide.tail <= narrow.tail <= narrow.head <= wide.head`
pub struct DoubleQueue<T> {
    wide: AtomicUsize,
    narrow: AtomicUsize,
    data: Box<[mem::MaybeUninit<UnsafeCell<T>>]>,
}

unsafe impl<T> Sync for DoubleQueue<T> {}

impl<T> DoubleQueue<T> {
    fn advance(&self, index: Pointer) -> Pointer {
        if index as usize + 1 == self.data.len() {
            0
        } else {
            index + 1
        }
    }
}

impl<T: Send> super::SynQueue<T> for DoubleQueue<T> {
    fn new(capacity: usize) -> Self {
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

    #[profiling::function]
    fn push(&self, value: T) -> Result<(), T> {
        // acqure a new position within the wide state
        let mut state = self.wide.load(super::LOAD_ORDER);
        let (head, next) = loop {
            log::trace!("Push pre-CAS: {:x}", state);
            let s = State::unpack(state);
            let next = self.advance(s.head);
            if next == s.tail {
                return Err(value);
            }
            match self.wide.compare_exchange_weak(
                state,
                State { head: next, ..s }.pack(),
                super::CAS_ORDER,
                super::LOAD_ORDER,
            ) {
                Ok(_) => break (s.head, next),
                Err(other) => state = other,
            }
            hint::spin_loop();
        };

        log::trace!("Push success, next head = {:x}", next);
        // write the data
        unsafe { UnsafeCell::raw_get(self.data[head as usize].as_ptr()).write(value) };

        // advance the narrow state
        state = self.narrow.load(super::LOAD_ORDER);
        log::trace!("Push narrow state: {:x}", state);
        let mut s = State::unpack(state);
        loop {
            if s.head != head {
                thread::yield_now();
            }
            match self.narrow.compare_exchange_weak(
                State { head, ..s }.pack(),
                State { head: next, ..s }.pack(),
                super::CAS_ORDER,
                super::LOAD_ORDER,
            ) {
                Ok(_) => break,
                Err(other) => {
                    log::trace!("Push post-CAS: {:x}", other);
                    hint::spin_loop();
                    s = State::unpack(other);
                }
            }
        }

        // done
        Ok(())
    }

    #[profiling::function]
    fn pop(&self) -> Option<T> {
        // acquire the oldest position within the narrow state
        let mut state = self.narrow.load(super::LOAD_ORDER);
        let (tail, next) = loop {
            log::trace!("Pop pre-CAS: {:x}", state);
            let s = State::unpack(state);
            if s.head == s.tail {
                return None;
            }
            let next = self.advance(s.tail);
            match self.narrow.compare_exchange_weak(
                state,
                State { tail: next, ..s }.pack(),
                super::CAS_ORDER,
                super::LOAD_ORDER,
            ) {
                Ok(_) => break (s.tail, next),
                Err(other) => state = other,
            }
            hint::spin_loop();
        };

        log::trace!("Pop success, next tail = {:x}", next);
        // read the data
        let value = unsafe { self.data[tail as usize].assume_init_read().into_inner() };

        // advance the wide state
        state = self.wide.load(super::LOAD_ORDER);
        let mut s = State::unpack(state);
        log::trace!("Pop wide state: {:x}", state);
        loop {
            if s.tail != tail {
                thread::yield_now();
            }
            match self.wide.compare_exchange_weak(
                State { tail, ..s }.pack(),
                State { tail: next, ..s }.pack(),
                super::CAS_ORDER,
                super::LOAD_ORDER,
            ) {
                Ok(_) => break,
                Err(other) => {
                    log::trace!("Pop post-CAS: {:x}", other);
                    hint::spin_loop();
                    s = State::unpack(other);
                }
            }
        }

        // done
        Some(value)
    }
}

impl<T> Drop for DoubleQueue<T> {
    fn drop(&mut self) {
        let state = self.wide.load(super::LOAD_ORDER);
        log::trace!("Drop state: {:x}", state);
        assert_eq!(state, self.narrow.load(super::LOAD_ORDER));
        let s = State::unpack(state);
        let mut cursor = s.tail;
        while cursor != s.head {
            unsafe { self.data[cursor as usize].assume_init_drop() };
            cursor = self.advance(cursor);
        }
    }
}

#[test]
fn overflow() {
    super::test_overflow::<DoubleQueue<i32>>();
}

#[test]
fn smoke() {
    super::test_smoke::<DoubleQueue<i32>>();
}

#[test]
fn barrage() {
    super::test_barrage::<DoubleQueue<usize>>();
}
