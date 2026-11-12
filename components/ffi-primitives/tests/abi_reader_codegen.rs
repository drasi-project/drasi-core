// Copyright 2026 The Drasi Authors.
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

use std::{fs, path::Path, process::Command, time::SystemTime};

#[test]
fn unused_abi_reader_does_not_emit_marker_search_string() {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("drasi-abi-codegen-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).expect("create codegen directory");
    let object = directory.join("ffi-primitives.o");
    let output = Command::new("rustc")
        .args([
            "--edition=2021",
            "--crate-name=drasi_ffi_primitives",
            "--crate-type=rlib",
            "--emit=obj",
            "-C",
            "opt-level=3",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .arg("-o")
        .arg(&object)
        .output()
        .expect("compile FFI primitives object");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let binary = fs::read(&object).expect("read compiled object");
    fs::remove_dir_all(&directory).expect("remove codegen directory");

    let prefix = drasi_ffi_primitives::PLUGIN_ABI_VERSION_MARKER_PREFIX;
    assert!(
        !binary.windows(prefix.len()).any(|window| window == prefix),
        "unused ABI reader must not emit a search string that MinGW retains as a second marker"
    );
}
