// Copyright 2024 The Drasi Authors.
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

mod distinct;
mod index_of;
mod insert;
mod range;
mod reduce;
mod tail;

pub use distinct::Distinct;
pub use index_of::IndexOf;
pub use insert::Insert;
pub use range::Range;
pub use reduce::Reduce;
pub use tail::Tail;
