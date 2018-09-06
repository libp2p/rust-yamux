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

use std::{self, hash::{BuildHasherDefault, Hasher}, marker::PhantomData};

pub type HashMap<K, V> =
    std::collections::HashMap<K, V, BuildHasherDefault<Stateless<K>>>;

#[derive(Copy, Clone, Debug, Default)]
pub struct Stateless<T>(u64, PhantomData<T>);

impl Hasher for Stateless<usize> {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, _: &[u8]) { unimplemented!() }
    fn write_u8(&mut self, _: u8) {unimplemented!() }
    fn write_u16(&mut self, _: u16) { unimplemented!() }
    fn write_u32(&mut self, _: u32) { unimplemented!() }
    fn write_u64(&mut self, _: u64) { unimplemented!() }
    fn write_u128(&mut self, _: u128) { unimplemented!() }
    fn write_usize(&mut self, n: usize) { self.0 = n as u64 }
    fn write_i8(&mut self, _: i8) { unimplemented!() }
    fn write_i16(&mut self, _: i16) { unimplemented!() }
    fn write_i32(&mut self, _: i32) { unimplemented!() }
    fn write_i64(&mut self, _: i64) { unimplemented!() }
    fn write_i128(&mut self, _: i128) { unimplemented!() }
    fn write_isize(&mut self, _: isize) { unimplemented!() }
}

