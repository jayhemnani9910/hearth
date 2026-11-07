//! Locks that survive a panic elsewhere.
//!
//! A panic while a `Mutex` is held marks it poisoned, and `lock().unwrap()`
//! would then panic in every other thread, taking the whole server (and every
//! shell) down. The server instead keeps using the data: the code that
//! panicked closes only its own terminal.

use std::sync::{Mutex, MutexGuard};

pub trait Lock<T> {
    fn lk(&self) -> MutexGuard<'_, T>;
}

impl<T> Lock<T> for Mutex<T> {
    fn lk(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}
