//! Lock types shared by the tree crates.
//!
//! # Poisoning is ignored
//!
//! Every acquisition takes the guard out of a `PoisonError`. A panic inside a
//! critical section is a bug that has already failed its own operation;
//! poisoning would turn that one bug into a failure of every later caller,
//! including `Drop` paths and test harnesses that catch panics.
//!
//! To make continuing safe, every critical section in the tree crates follows
//! one rule: **check before mutating**. All fallible checks and assertions
//! run before the first write to the protected data, so a panic leaves the
//! data consistent.
//!
//! ```
//! use bf_tree_sync::Mutex;
//!
//! let counter = Mutex::new(0);
//! *counter.lock() += 1;
//! assert_eq!(*counter.lock(), 1);
//! ```

use std::sync::{PoisonError, TryLockError};

pub use std::sync::{MutexGuard, RwLockReadGuard, RwLockWriteGuard};

mod table;

pub use table::Table;

/// A mutex whose `lock` never fails.
#[derive(Debug, Default)]
pub struct Mutex<T>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `None` if another thread holds the lock.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A reader-writer lock whose acquisitions never fail.
#[derive(Debug, Default)]
pub struct RwLock<T>(std::sync::RwLock<T>);

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(value))
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// `None` if a writer holds the lock.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        match self.0.try_read() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// `None` if any thread holds the lock.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        match self.0.try_write() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
