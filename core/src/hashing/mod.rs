// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Stable persisted hashing shared by query evaluation and index backends.
//!
//! The SpookyHash implementation is imported from `hashers` 1.0.1 under its
//! MIT license (`LICENSE-MIT`). Its diagnostic stdout writes are removed;
//! the hashing operations, field order and seeds are preserved.

macro_rules! load_int_le {
    ($buf:expr, $i:expr, $int_ty:ident) => {{
        unsafe {
            debug_assert!($i + mem::size_of::<$int_ty>() <= $buf.len());
            let mut data = 0 as $int_ty;
            ptr::copy_nonoverlapping(
                $buf.get_unchecked($i),
                &mut data as *mut _ as *mut u8,
                mem::size_of::<$int_ty>(),
            );
            data.to_le()
        }
    }};
}

#[cfg(test)]
macro_rules! hasher_to_fcn {
    ($(#[$attr:meta])* $name:ident, $hasher:ident) => {
        $(#[$attr])*
        #[inline]
        pub fn $name(bytes: &[u8]) -> u64 {
            let mut hasher = $hasher::default();
            hasher.write(bytes);
            hasher.finish()
        }
    };
}

mod spooky;
pub use spooky::SpookyHasher;

#[cfg(test)]
mod tests {
    use super::SpookyHasher;
    use std::hash::{Hash, Hasher};

    #[test]
    fn persisted_hashes_match_the_original_for_fragmented_and_unaligned_input() {
        use hashers::jenkins::spooky_hash::SpookyHasher as Original;

        for length in [
            0, 1, 8, 15, 16, 17, 31, 32, 33, 63, 64, 95, 96, 97, 191, 192, 193, 255, 384, 385,
            1024, 4096,
        ] {
            let bytes: Vec<u8> = (0..length + 8)
                .map(|index| (index * 31 + 17) as u8)
                .collect();
            for offset in [0, 1, 3] {
                for width in [1, 7, 16, 31, 96, 192, 257] {
                    for seed in [(0, 0), (1, u64::MAX)] {
                        let mut original = Original::new(seed.0, seed.1);
                        let mut quiet = SpookyHasher::new(seed.0, seed.1);
                        for chunk in bytes[offset..offset + length].chunks(width) {
                            original.write(chunk);
                            quiet.write(chunk);
                            assert_eq!(
                                quiet.finish128(),
                                original.finish128(),
                                "length={length},offset={offset},width={width}"
                            );
                        }
                        assert_eq!(quiet.finish128(), original.finish128());
                        assert_eq!(quiet.finish(), original.finish());
                    }
                }
            }
        }
        let mut original = Original::default();
        let mut quiet = SpookyHasher::default();
        let row = (
            17u64,
            "query/source",
            vec!["long-property-name".repeat(40), "second".into()],
        );
        row.hash(&mut original);
        row.hash(&mut quiet);
        assert_eq!(quiet.finish128(), original.finish128());
    }

    #[test]
    fn quiet_hashing_worker() {
        let mut hasher = SpookyHasher::default();
        for index in 0..256u64 {
            index.hash(&mut hasher);
            "a repeated property name long enough to fill the hash buffer".hash(&mut hasher);
        }
        assert_ne!(hasher.finish(), 0);
    }

    #[test]
    fn hashing_does_not_write_diagnostics_to_stdout() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "hashing::tests::quiet_hashing_worker",
                "--nocapture",
            ])
            .output()
            .expect("run stdout probe");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(!stdout.contains("m_data"), "{stdout}");
        assert!(
            !stdout
                .lines()
                .any(|line| line.trim().parse::<u64>().is_ok()),
            "{stdout}"
        );
    }
}
