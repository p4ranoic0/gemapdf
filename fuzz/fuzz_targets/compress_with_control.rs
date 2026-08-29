#![no_main]

use std::cell::Cell;

use gema_core::{compress_with_control, CancelSignal, CompressOptions};
use libfuzzer_sys::fuzz_target;

struct PseudoRandomCancel {
    state: Cell<u64>,
}

impl CancelSignal for PseudoRandomCancel {
    fn is_cancelled(&self) -> bool {
        let mut state = self.state.get();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.state.set(state);
        state % 7 == 0
    }
}

fn input_byte(data: &[u8], index: usize) -> u8 {
    data.get(index).copied().unwrap_or(0)
}

fuzz_target!(|data: &[u8]| {
    let word = |offset| {
        u16::from_le_bytes([input_byte(data, offset), input_byte(data, offset + 1)])
    };
    let seed = (0..8).fold(0u64, |value, index| {
        value | u64::from(input_byte(data, index)) << (index * 8)
    });
    let cancel = PseudoRandomCancel {
        state: Cell::new(seed.max(1)),
    };
    let opts = CompressOptions {
        max_pages: Some(usize::from(input_byte(data, 8))),
        max_objects: Some(usize::from(word(9))),
        max_stream_bytes: Some(u64::from(word(11)) * 1_024),
        max_total_work_bytes: Some(u64::from(word(13)) * 1_024),
        ..Default::default()
    };

    let _ = compress_with_control(data, &opts, &mut |_| {}, &cancel);
});
