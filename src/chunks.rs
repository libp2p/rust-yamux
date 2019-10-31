// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::Bytes;
use std::collections::VecDeque;

/// A sequence of [`Bytes`] values.
///
/// [`Chunks::len`] considers all [`Bytes`] elements and computes the total
/// result, e.g. the length of all bytes by summing up the lengths of all
/// [`Bytes`] elements.
#[derive(Debug)]
pub(crate) struct Chunks {
    seq: VecDeque<Bytes>
}

impl Chunks {
    pub(crate) fn new() -> Self {
        Chunks { seq: VecDeque::new() }
    }

    pub(crate) fn len(&self) -> Option<usize> {
        self.seq.iter().fold(Some(0), |total, x| {
            total.and_then(|n| n.checked_add(x.len()))
        })
    }

    pub(crate) fn push(&mut self, x: Bytes) {
        if !x.is_empty() {
            self.seq.push_back(x)
        }
    }

    pub(crate) fn pop(&mut self) -> Option<Bytes> {
        self.seq.pop_front()
    }

    pub(crate) fn front_mut(&mut self) -> Option<&mut Bytes> {
        self.seq.front_mut()
    }
}
