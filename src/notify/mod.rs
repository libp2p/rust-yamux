// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

mod hash;

use futures::{executor, task};
use parking_lot::Mutex;
use self::hash::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};


static NEXT_TASK_ID: AtomicUsize = AtomicUsize::new(0);

task_local!{
    static TASK_ID: usize = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}


pub struct Notifier {
    tasks: Mutex<HashMap<usize, task::Task>>
}

impl Notifier {
    pub fn new() -> Self {
        Notifier { tasks: Mutex::new(HashMap::default()) }
    }

    pub fn insert_current(&self) {
        self.tasks.lock().insert(TASK_ID.with(|&t| t), task::current());
    }

    pub fn notify_all(&self) {
        let mut tasks = self.tasks.lock();
        for (_, t) in tasks.drain() {
            t.notify();
        }
    }

    pub fn len(&self) -> usize {
        self.tasks.lock().len()
    }
}

impl executor::Notify for Notifier {
    fn notify(&self, _: usize) {
        self.notify_all()
    }
}

