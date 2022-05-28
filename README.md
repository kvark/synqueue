# synqueue
[![Build Status](https://github.com/kvark/synqueue/workflows/check/badge.svg?branch=main)](https://github.com/kvark/synqueue/actions)

Internally synchronized queue (aka MPSC queue). Unlike other implementations, doesn't carry a atomic bit per element.
Checked by both [Miri](https://github.com/rust-lang/miri) and [Loom](https://github.com/tokio-rs/loom) on CI.

Doesn't exactly work yet :)