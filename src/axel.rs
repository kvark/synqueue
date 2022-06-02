use super::qstd::{cell::UnsafeCell, hint, sync::atomic::AtomicUsize};
use std::{mem, ptr};

type Pointer = u32;
const _BITS_CHECK: usize = (mem::size_of::<usize>() == 2 * mem::size_of::<Pointer>()) as usize - 1;
const MASK_BITS: usize = mem::size_of::<usize>() * 8;

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

pub struct AxelQueue<T> {
    state: AtomicUsize,
    occupation: Box<[AtomicUsize]>,
    data: Box<[mem::MaybeUninit<UnsafeCell<T>>]>,
}

unsafe impl<T> Sync for AxelQueue<T> {}

impl<T> AxelQueue<T> {
    fn advance(&self, index: Pointer) -> Pointer {
        if index as usize + 1 == self.data.len() {
            0
        } else {
            index + 1
        }
    }
}

impl<T: Send> super::SynQueue<T> for AxelQueue<T> {
    fn new(capacity: usize) -> Self {
        let num_words = 1 + capacity / MASK_BITS;
        Self {
            state: AtomicUsize::new(0),
            occupation: (0..num_words).map(|_| AtomicUsize::new(0)).collect(),
            data: (0..=capacity).map(|_| mem::MaybeUninit::uninit()).collect(),
        }
    }

    #[profiling::function]
    fn push(&self, value: T) -> Result<(), T> {
        let mut state = self.state.load(super::LOAD_ORDER);
        let mut index;
        let mut bit;
        let next = loop {
            log::trace!("Push CAS: {:x}", state);
            let s = State::unpack(state);
            let next = self.advance(s.head);
            if next == s.tail {
                return Err(value);
            }

            index = s.head as usize;
            bit = 1 << (index % MASK_BITS);
            let mask =
                unsafe { self.occupation.get_unchecked(index / MASK_BITS) }.load(super::LOAD_ORDER);
            if mask & bit == 0 {
                match self.state.compare_exchange_weak(
                    state,
                    State { head: next, ..s }.pack(),
                    super::CAS_ORDER,
                    super::LOAD_ORDER,
                ) {
                    Ok(_) => break next,
                    Err(other) => state = other,
                }
            } else {
                // some `pop` is not finished reading the value?
                state = self.state.load(super::LOAD_ORDER);
            }
            hint::spin_loop();
        };

        log::trace!("Push success, next head = {:x}", next);
        // write the data
        unsafe { super::UnsafeCellHelper::write(self.data.get_unchecked(index).as_ptr(), value) };

        let old = unsafe { self.occupation.get_unchecked(index / MASK_BITS) }
            .fetch_or(bit, super::CAS_ORDER);
        debug_assert_eq!(old & bit, 0);

        // done
        Ok(())
    }

    #[profiling::function]
    fn pop(&self) -> Option<T> {
        let mut state = self.state.load(super::LOAD_ORDER);
        let mut index;
        let mut bit;
        let next = loop {
            log::trace!("Pop CAS: {:x}", state);
            let s = State::unpack(state);
            if s.head == s.tail {
                return None;
            }

            index = s.tail as usize;
            bit = 1 << (index % MASK_BITS);
            let mask =
                unsafe { self.occupation.get_unchecked(index / MASK_BITS) }.load(super::LOAD_ORDER);
            if mask & bit != 0 {
                let next = self.advance(s.tail);
                match self.state.compare_exchange_weak(
                    state,
                    State { tail: next, ..s }.pack(),
                    super::CAS_ORDER,
                    super::LOAD_ORDER,
                ) {
                    Ok(_) => break next,
                    Err(other) => state = other,
                }
            } else {
                // some `push` is not finished writing the value?
                state = self.state.load(super::LOAD_ORDER);
            }
            hint::spin_loop();
        };

        log::trace!("Pop success, next tail = {:x}", next);
        // read the data
        let value = unsafe { ptr::read(self.data.get_unchecked(index).as_ptr()).into_inner() };

        let old = unsafe { self.occupation.get_unchecked(index / MASK_BITS) }
            .fetch_and(!bit, super::CAS_ORDER);
        debug_assert_ne!(old & bit, 0);

        // done
        Some(value)
    }
}

impl<T> Drop for AxelQueue<T> {
    fn drop(&mut self) {
        let state = self.state.load(super::LOAD_ORDER);
        log::trace!("Drop state: {:x}", state);
        let s = State::unpack(state);
        let mut cursor = s.tail;
        while cursor != s.head {
            unsafe { ptr::read(self.data[cursor as usize].as_ptr()) };
            cursor = self.advance(cursor);
        }
    }
}

#[test]
fn overflow() {
    super::test_overflow::<AxelQueue<i32>>();
}

#[test]
fn smoke() {
    super::test_smoke::<AxelQueue<i32>>();
}

#[test]
fn barrage() {
    super::test_barrage::<AxelQueue<usize>>();
}
