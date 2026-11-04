use crate::model::Body;

#[derive(Clone, Copy, Debug)]
pub struct Chunker {
    pub min: usize,
    pub bits: u32,
    pub max: usize,
}

pub const DEFAULT: Chunker = Chunker {
    min: 512,
    bits: 10,
    max: 4096,
};

const fn gear() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

const GEAR: [u64; 256] = gear();

impl Chunker {
    pub fn split<'a>(&self, text: &'a str) -> Vec<&'a str> {
        let bytes = text.as_bytes();
        let mask = ((1u64 << self.bits) - 1) << (64 - self.bits);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < bytes.len() {
            let limit = (start + self.max).min(bytes.len());
            let mut end = limit;
            if limit - start > self.min {
                let mut hash = 0u64;
                for (offset, byte) in bytes[start..limit].iter().enumerate() {
                    hash = (hash << 1).wrapping_add(GEAR[*byte as usize]);
                    if offset + 1 >= self.min && hash & mask == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
            }
            while !text.is_char_boundary(end) {
                end += 1;
            }
            chunks.push(&text[start..end]);
            start = end;
        }
        chunks
    }
}

pub fn build(text: &str, chunker: &Chunker) -> Body {
    chunker
        .split(text)
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub fn text(body: &Body) -> String {
    let mut out = String::new();
    for chunk in body.iter() {
        out.push_str(&chunk);
    }
    out
}

pub fn bytes(body: &Body) -> usize {
    body.iter().map(|chunk| chunk.len()).sum()
}

pub fn update(body: &mut Body, text: &str, chunker: &Chunker) {
    let chunks = chunker.split(text);
    for (index, chunk) in chunks.iter().enumerate() {
        match body.get(index) {
            Some(existing) if existing.as_str() == *chunk => {}
            Some(_) => {
                body.set(index, chunk.to_string());
            }
            None => body.push(chunk.to_string()),
        }
    }
    body.truncate(chunks.len());
}
