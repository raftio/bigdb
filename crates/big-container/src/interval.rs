// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/// A contiguous run of bits; both ends inclusive.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(C)]
pub struct Interval {
    pub start: u16,
    pub last: u16,
}

impl Interval {
    pub fn new(start: u16, last: u16) -> Self {
        Self { start, last }
    }

    /// Always >= 1 because both ends are inclusive, hence no `is_empty`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u32 {
        (self.last as u32) - (self.start as u32) + 1
    }

    pub fn contains(&self, v: u16) -> bool {
        self.start <= v && v <= self.last
    }
}

/// `Interval` is two adjacent repr(C) u16s: no padding, every bit pattern is valid.
#[allow(unsafe_code)]
mod pod {
    use super::Interval;
    unsafe impl bytemuck::Zeroable for Interval {}
    unsafe impl bytemuck::Pod for Interval {}
}
