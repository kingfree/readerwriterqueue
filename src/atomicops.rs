use std::sync::atomic::{Ordering, AtomicBool};

pub struct WeakAtomicBool {
    value: AtomicBool,
}
