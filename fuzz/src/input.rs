pub struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Input { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn byte(&mut self) -> u8 {
        match self.data.get(self.pos) {
            Some(&byte) => {
                self.pos += 1;
                byte
            }
            None => 0,
        }
    }

    pub fn bytes(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(n).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }

    fn wide(&mut self, n: usize) -> u64 {
        let mut word = [0u8; 8];
        let taken = self.bytes(n);
        word[..taken.len()].copy_from_slice(taken);
        u64::from_le_bytes(word)
    }

    pub fn num(&mut self) -> u64 {
        match self.byte() {
            byte @ 0..=239 => u64::from(byte),
            240..=245 => self.wide(2),
            246..=249 => self.wide(4),
            250..=252 => self.wide(8),
            253 => u64::MAX - u64::from(self.byte()),
            254 => 1u64 << (self.byte() % 64),
            _ => (1u64 << (self.byte() % 64)).wrapping_sub(1),
        }
    }

    pub fn chance(&mut self, sixteenths: u8) -> bool {
        self.byte() % 16 < sixteenths
    }

    pub fn pick(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            usize::from(self.byte()) % n
        }
    }

    pub fn text(&mut self) -> String {
        let len = usize::from(self.byte() % 12);
        String::from_utf8_lossy(self.bytes(len)).into_owned()
    }
}
