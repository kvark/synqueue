mod axel;
mod double;
mod masked;

pub use axel::AxelQueue;
pub use double::DoubleQueue;
pub use masked::MaskedQueue;

#[cfg(feature = "loom")]
use loom as qstd;
#[cfg(not(feature = "loom"))]
use std as qstd;

use qstd::sync::atomic::Ordering;

const CAS_ORDER: Ordering = Ordering::AcqRel;
const LOAD_ORDER: Ordering = Ordering::Acquire;

pub trait SynQueue<T>: Send + Sync {
    fn new(capacity: usize) -> Self;
    fn push(&self, value: T) -> Result<(), T>;
    fn pop(&self) -> Option<T>;
    fn is_empty(&self) -> bool;
}

trait UnsafeCellHelper<T> {
    unsafe fn write(this: *const Self, value: T);
}

impl<T> UnsafeCellHelper<T> for std::cell::UnsafeCell<T> {
    unsafe fn write(this: *const Self, value: T) {
        std::cell::UnsafeCell::raw_get(this).write(value);
    }
}

#[cfg(feature = "loom")]
impl<T> UnsafeCellHelper<T> for loom::cell::UnsafeCell<T> {
    unsafe fn write(this: *const Self, value: T) {
        (*this).with_mut(|pointer| std::ptr::write(pointer, value));
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod loom {
    pub fn model(mut fun: impl FnMut()) {
        fun();
    }
}

#[cfg(test)]
fn test_overflow<Q: SynQueue<i32>>() {
    loom::model(|| {
        let sq = Q::new(2);
        sq.push(2).unwrap();
        sq.push(3).unwrap();
        assert_eq!(sq.push(4), Err(4));
    })
}

#[cfg(test)]
fn test_smoke<Q: SynQueue<i32>>() {
    loom::model(|| {
        let sq = Q::new(16);
        assert_eq!(sq.pop(), None);
        sq.push(5).unwrap();
        sq.push(10).unwrap();
        assert_eq!(sq.pop(), Some(5));
        assert_eq!(sq.pop(), Some(10));
    })
}

#[cfg(test)]
fn test_barrage<Q: SynQueue<usize> + 'static>() {
    use qstd::{sync::Arc, thread};

    loom::model(|| {
        const NUM_THREADS: usize = if cfg!(miri) { 2 } else { 8 };
        const NUM_ELEMENTS: usize = if cfg!(miri) { 1 << 7 } else { 1 << 16 };
        let sq = Arc::new(Q::new(NUM_ELEMENTS));
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
