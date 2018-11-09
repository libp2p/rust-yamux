// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::{executor, task, task_local};
use nohash_hasher::IntMap;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TASK_ID: AtomicUsize = AtomicUsize::new(0);

task_local!{
    static TASK_ID: usize = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

/// A notifier maintains a collection of tasks which should be
/// notified at some point. Useful in conjuction with `futures::executor::Spawn`.
pub struct Notifier {
    tasks: Mutex<IntMap<usize, task::Task>>
}

impl Notifier {
    pub fn new() -> Self {
        Notifier { tasks: Mutex::new(IntMap::default()) }
    }

    /// Insert the current task to the set of tasks to be notified.
    ///
    /// # Panics
    ///
    /// If called outside of a futures task.
    pub fn insert_current(&self) {
        self.tasks.lock().insert(TASK_ID.with(|&t| t), task::current());
    }

    /// Notify all registered tasks.
    pub fn notify_all(&self) {
        let mut tasks = self.tasks.lock();
        for (_, t) in tasks.drain() {
            t.notify();
        }
    }

    /// Return the number of currently registered tasks.
    pub fn len(&self) -> usize {
        self.tasks.lock().len()
    }
}

impl executor::Notify for Notifier {
    fn notify(&self, _: usize) {
        self.notify_all()
    }
}

