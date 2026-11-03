use ferro::{Bytes, Data, Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, Default, Data)]
#[ferro(transparent)]
pub struct Chunk(pub Bytes);

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at).ok_or_else(corrupt)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(corrupt());
        }
    }
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn corrupt() -> Error {
    Error::Corrupt("bad metrics chunk".to_string())
}

impl Chunk {
    pub fn encode_points(points: &[(u64, f64)]) -> Chunk {
        let mut out = Vec::with_capacity(8 + points.len() * 3);
        push_varint(&mut out, points.len() as u64);
        let Some(first) = points.first() else {
            return Chunk(out.into());
        };
        push_varint(&mut out, first.0);
        let mut prev_ts = first.0;
        let mut prev_delta = 0i64;
        for (index, (ts, _)) in points.iter().enumerate().skip(1) {
            let delta = (*ts - prev_ts) as i64;
            if index == 1 {
                push_varint(&mut out, delta as u64);
            } else {
                push_varint(&mut out, zigzag(delta - prev_delta));
            }
            prev_delta = delta;
            prev_ts = *ts;
        }
        let mut prev_bits = 0u64;
        for (_, value) in points {
            let bits = value.to_bits();
            let x = bits ^ prev_bits;
            prev_bits = bits;
            if x == 0 {
                out.push(0);
                continue;
            }
            let lead = (x.leading_zeros() / 8) as u8;
            let trail = (x.trailing_zeros() / 8) as u8;
            let len = 8 - lead - trail;
            out.push(0x80 | (lead << 3) | (len - 1));
            let shifted = x >> (u32::from(trail) * 8);
            out.extend_from_slice(&shifted.to_le_bytes()[..len as usize]);
        }
        Chunk(out.into())
    }

    pub fn len(&self) -> usize {
        let mut at = 0;
        read_varint(&self.0, &mut at).unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn decode_points(&self) -> Result<Vec<(u64, f64)>> {
        let bytes = &self.0;
        let mut at = 0;
        let count = read_varint(bytes, &mut at)? as usize;
        let mut out = Vec::with_capacity(count.min(bytes.len()));
        if count == 0 {
            return Ok(out);
        }
        let mut ts = read_varint(bytes, &mut at)?;
        out.push((ts, 0.0));
        let mut delta = 0i64;
        for index in 1..count {
            let raw = read_varint(bytes, &mut at)?;
            delta = if index == 1 {
                raw as i64
            } else {
                delta + unzigzag(raw)
            };
            ts = ts.wrapping_add(delta as u64);
            out.push((ts, 0.0));
        }
        let mut prev_bits = 0u64;
        for point in out.iter_mut() {
            let header = *bytes.get(at).ok_or_else(corrupt)?;
            at += 1;
            let bits = if header == 0 {
                prev_bits
            } else {
                let lead = u32::from((header >> 3) & 7);
                let len = u32::from(header & 7) + 1;
                let end = at + len as usize;
                let slice = bytes.get(at..end).ok_or_else(corrupt)?;
                at = end;
                let mut buf = [0u8; 8];
                buf[..len as usize].copy_from_slice(slice);
                let trail = 8u32.checked_sub(lead + len).ok_or_else(corrupt)?;
                prev_bits ^ (u64::from_le_bytes(buf) << (trail * 8))
            };
            prev_bits = bits;
            point.1 = f64::from_bits(bits);
        }
        Ok(out)
    }

    pub fn merged(&self, extra: &[(u64, f64)]) -> Result<Chunk> {
        let mut points = self.decode_points()?;
        merge_points(&mut points, extra);
        Ok(Chunk::encode_points(&points))
    }
}

pub fn merge_points(points: &mut Vec<(u64, f64)>, extra: &[(u64, f64)]) {
    if extra.is_empty() {
        return;
    }
    let tail_ok = match (points.last(), extra.first()) {
        (Some(last), Some(first)) => first.0 > last.0,
        _ => true,
    };
    if tail_ok && extra.windows(2).all(|pair| pair[0].0 < pair[1].0) {
        points.extend_from_slice(extra);
        return;
    }
    let mut all = std::mem::take(points);
    all.extend_from_slice(extra);
    all.sort_by_key(|point| point.0);
    let mut out: Vec<(u64, f64)> = Vec::with_capacity(all.len());
    let mut index = 0;
    while index < all.len() {
        let mut last = index;
        while last + 1 < all.len() && all[last + 1].0 == all[index].0 {
            last += 1;
        }
        out.push(all[last]);
        index = last + 1;
    }
    *points = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let points: Vec<(u64, f64)> = (0..500)
            .map(|i| (1_000 + i * 10_000 + (i % 7), (i as f64) * 1.5 - 3.0))
            .collect();
        let chunk = Chunk::encode_points(&points);
        assert_eq!(chunk.decode_points().unwrap(), points);
        assert_eq!(chunk.len(), 500);
    }

    #[test]
    fn special_values() {
        let points = vec![
            (1, f64::NAN.copysign(1.0)),
            (2, f64::INFINITY),
            (3, -0.0),
            (4, f64::MIN_POSITIVE),
            (4_000_000_000_000, 1e300),
        ];
        let chunk = Chunk::encode_points(&points);
        let back = chunk.decode_points().unwrap();
        for (a, b) in points.iter().zip(back.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.to_bits(), b.1.to_bits());
        }
    }

    #[test]
    fn merge_overwrites_and_sorts() {
        let chunk = Chunk::encode_points(&[(10, 1.0), (20, 2.0)]);
        let merged = chunk.merged(&[(15, 9.0), (20, 3.0), (5, 0.5)]).unwrap();
        assert_eq!(
            merged.decode_points().unwrap(),
            vec![(5, 0.5), (10, 1.0), (15, 9.0), (20, 3.0)]
        );
    }

    #[test]
    fn empty_chunk() {
        let chunk = Chunk::encode_points(&[]);
        assert!(chunk.is_empty());
        assert!(chunk.decode_points().unwrap().is_empty());
    }
}
