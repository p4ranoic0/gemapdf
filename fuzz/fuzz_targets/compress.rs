#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = gema_compress::compress(data, &gema_compress::CompressOptions::default());
});
