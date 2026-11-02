#![allow(dead_code)]

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

use ferro::codec::{Encoder, HashSink};
use ferro::{Data, Durability, Options};

pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

pub fn text(seed: u64, len: usize) -> String {
    let mut rng = Rng::new(seed.wrapping_add(1));
    (0..len)
        .map(|_| char::from(b'a' + rng.below(26) as u8))
        .collect()
}

pub fn cases(base: u32) -> u32 {
    let scale = std::env::var("FERRO_TEST_SCALE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    base.saturating_mul(scale.max(1))
}

pub fn level_of<K: Data>(key: &K) -> u32 {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    key.encode(&mut encoder).unwrap();
    let hash = blake3::hash(encoder.bytes());
    let word = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
    word.trailing_zeros() / 4
}

pub fn fast() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 20)
}

pub fn roomy() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1 << 28)
}

pub fn uncached() -> Options {
    Options::new()
        .durability(Durability::Buffered)
        .cache_bytes(1)
}

pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        text.to_string()
    } else {
        "<non-string panic>".to_string()
    }
}

pub fn quiet_ferro_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !panic_text(info.payload()).starts_with("ferro:") {
                default(info);
            }
        }));
    });
}

pub enum Outcome<T> {
    Value(T),
    FerroPanic(String),
}

pub fn guarded<T>(f: impl FnOnce() -> T) -> Outcome<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Outcome::Value(value),
        Err(payload) => {
            let message = panic_text(&*payload);
            assert!(
                message.starts_with("ferro:"),
                "panic without ferro prefix: {message}"
            );
            Outcome::FerroPanic(message)
        }
    }
}
