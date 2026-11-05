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

#[test]
fn locate_maps_chunk_boundaries() {
    use super::table::locate;
    assert_eq!(locate(0), (0, 0));
    assert_eq!(locate(63), (0, 63));
    assert_eq!(locate(64), (1, 0));
    assert_eq!(locate(191), (1, 127));
    assert_eq!(locate(192), (2, 0));
    assert_eq!(locate(447), (2, 255));
    assert_eq!(locate(448), (3, 0));
    // Every index maps inside its chunk, and consecutive indices are dense.
    let mut previous = (0, 0);
    for index in 1..100_000 {
        let (chunk, offset) = locate(index);
        assert!(offset < 64 << chunk);
        assert!(
            (chunk, offset) == (previous.0, previous.1 + 1)
                || (chunk == previous.0 + 1 && offset == 0)
        );
        previous = (chunk, offset);
    }
}

#[test]
fn growth_is_idempotent_out_of_order_and_keeps_references() {
    let table: Table<Mutex<usize>> = Table::new();
    assert!(table.get(0).is_none());
    table.grow_to(7);
    let first = table.get(3).unwrap();
    *first.lock() = 33;

    // A smaller target changes nothing; a larger one crosses chunks.
    table.grow_to(5);
    assert_eq!(table.len(), 7);
    table.grow_to(1000);
    assert_eq!(table.len(), 1000);
    assert!(table.get(1000).is_none());

    // The reference taken before growth still points at the same entry.
    assert_eq!(*first.lock(), 33);
    assert!(std::ptr::eq(first, table.get(3).unwrap()));
    assert_eq!(table.iter().count(), 1000);
}

#[test]
fn threads_grow_and_read_concurrently() {
    let table: Table<AtomicCounter> = Table::new();
    std::thread::scope(|scope| {
        for thread in 0..4usize {
            let table = &table;
            scope.spawn(move || {
                for step in 0..2000usize {
                    let index = step * 4 + thread;
                    table.grow_to(index + 1);
                    table.get(index).unwrap().bump();
                    // Any index below len is readable.
                    let len = table.len();
                    assert!(table.get(len - 1).is_some());
                }
            });
        }
    });
    assert_eq!(table.len(), 8000);
    assert!(table.iter().all(|counter| counter.get() == 1));
}

#[derive(Debug, Default)]
struct AtomicCounter(std::sync::atomic::AtomicUsize);

impl AtomicCounter {
    fn bump(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn get(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[test]
fn spin_counts_attempts_and_saturates() {
    let mut attempt = 0;
    for _ in 0..100 {
        spin(&mut attempt);
    }
    assert_eq!(attempt, 100);
    let mut attempt = u32::MAX;
    spin(&mut attempt);
    assert_eq!(attempt, u32::MAX);
}
