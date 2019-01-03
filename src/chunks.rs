// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use bytes::BytesMut;
use std::collections::VecDeque;

/// A sequence of `BytesMut` values.
///
/// `Chunks::is_empty` and `Chunks::len` consider all `BytesMut` elements and
/// compute the total result, e.g. the length of all bytes by summing up the
/// lengths of all `BytesMut` elements.
#[derive(Debug)]
pub struct Chunks {
    seq: VecDeque<BytesMut>
}

impl Chunks {
    pub fn new() -> Self {
        Chunks { seq: VecDeque::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.seq.iter().all(|x| x.is_empty())
    }

    pub fn len(&self) -> Option<usize> {
        self.seq.iter().fold(Some(0), |total, x| {
            total.and_then(|n| n.checked_add(x.len()))
        })
    }

    pub fn push(&mut self, x: BytesMut) {
        self.seq.push_back(x)
    }

    pub fn pop(&mut self) -> Option<BytesMut> {
        self.seq.pop_front()
    }

    pub fn front_mut(&mut self) -> Option<&mut BytesMut> {
        self.seq.front_mut()
    }
}
