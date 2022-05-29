# synqueue
[![Build Status](https://github.com/kvark/synqueue/workflows/check/badge.svg?branch=main)](https://github.com/kvark/synqueue/actions)

This is an experimental queue to be used in multi-threaded scenarios, like the task processors. More specifically:
  - internally synchronized for both consumers and producers (MPMC).
  - backed by an array, which is fast to access
  - bounded: the capacity is specified at creation

Unlike other implementations, such as `crossbeam-queue`, it doesn't carry a atomic bit per element.
Checked by both [Miri](https://github.com/rust-lang/miri) and [Loom](https://github.com/tokio-rs/loom) on CI.

**Note**: experimental and currently slower than alternatives.
