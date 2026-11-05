use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

#[test]
fn a_panic_while_holding_a_mutex_leaves_it_usable() {
    let mut mutex = Mutex::new(1);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _guard = mutex.lock();
        panic!("poisoned");
    }));
    assert!(mutex.0.is_poisoned());
    assert_eq!(*mutex.lock(), 1);
    assert_eq!(mutex.try_lock().map(|guard| *guard), Some(1));
    *mutex.get_mut() = 3;
    assert_eq!(mutex.into_inner(), 3);
}

#[test]
fn a_panic_while_holding_an_rwlock_leaves_it_usable() {
    let mut lock = RwLock::new(1);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _guard = lock.write();
        panic!("poisoned");
    }));
    assert!(lock.0.is_poisoned());
    assert_eq!(*lock.read(), 1);
    *lock.write() = 2;
    assert_eq!(lock.try_read().map(|guard| *guard), Some(2));
    assert_eq!(lock.try_write().map(|guard| *guard), Some(2));
    *lock.get_mut() = 3;
    assert_eq!(lock.into_inner(), 3);
}

#[test]
fn try_variants_report_contention() {
    let mutex = Mutex::new(());
    let held = mutex.lock();
    assert!(mutex.try_lock().is_none());
    drop(held);
    assert!(mutex.try_lock().is_some());

    let lock = RwLock::new(());
    let reader = lock.read();
    assert!(lock.try_read().is_some());
    assert!(lock.try_write().is_none());
    drop(reader);
    let writer = lock.write();
    assert!(lock.try_read().is_none());
    assert!(lock.try_write().is_none());
    drop(writer);
}

#[test]
fn locks_default_to_the_default_value() {
    assert_eq!(*Mutex::<u32>::default().lock(), 0);
    assert_eq!(*RwLock::<Vec<u8>>::default().read(), Vec::<u8>::new());
}
