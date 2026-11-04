use ferro::codec::{Data, Encoder, Ptr};
use ferro::map::Summary;
use ferro::store::{Store, StoreOptions, Txn};
use ferro::{Bytes, Durability, Map};

use crate::guarded;
use crate::input::Input;
use crate::types::{Gen, Probe, Shop, Total};

fn value(txn: &mut Txn, value: &impl Data) -> ferro::Result<Ptr> {
    let mut encoder = Encoder::new(txn);
    value.encode(&mut encoder)?;
    encoder.finish()
}

fn build<K, V, S>(input: &mut Input<'_>, txn: &mut Txn) -> ferro::Result<Ptr>
where
    K: Gen + Data + Ord,
    V: Gen + Data,
    S: Gen + Summary<K, V>,
{
    let mut values = Vec::new();
    for _ in 0..input.byte() % 4 {
        let made = V::generate(input);
        values.push(value(txn, &made)?);
    }
    let mut nodes: Vec<(Ptr, u64)> = Vec::new();
    for _ in 0..1 + input.byte() % 8 {
        let control = input.byte();
        let mut w = Encoder::new(&mut *txn);
        let mut count = 0u64;
        if control.is_multiple_of(8) {
            let len = usize::from(input.byte());
            w.raw(input.bytes(len));
            for _ in 0..input.byte() % 4 {
                match nodes.len() {
                    0 => w.link(Ptr {
                        hash: [control; 20],
                        offset: input.num(),
                    }),
                    len => w.link(nodes[usize::from(input.byte()) % len].0),
                }
            }
        } else {
            let level = match control & 8 {
                0 => input.byte() % 4,
                _ => input.byte(),
            };
            let mut keys: Vec<K> = (0..input.byte() % 6).map(|_| K::generate(input)).collect();
            if control & 16 == 0 {
                keys.sort();
                keys.dedup();
            }
            w.u8(level);
            w.varint(keys.len() as u64);
            for key in &keys {
                key.encode(&mut w)?;
            }
            for _ in &keys {
                match input.byte() % 8 {
                    0 if !values.is_empty() => {
                        w.u8(1);
                        w.link(values[usize::from(input.byte()) % values.len()]);
                        S::generate(input).encode(&mut w)?;
                    }
                    1 => {
                        w.u8(input.byte() % 3);
                        let len = usize::from(input.byte() % 16);
                        w.raw(input.bytes(len));
                    }
                    _ => {
                        w.u8(0);
                        V::generate(input).encode(&mut w)?;
                    }
                }
            }
            count = keys.len() as u64;
            for _ in 0..=keys.len() {
                if nodes.is_empty() || !input.chance(5) {
                    w.varint(0);
                    continue;
                }
                let (child, below) = nodes[usize::from(input.byte()) % nodes.len()];
                let below = if control & 32 != 0 {
                    input.num()
                } else {
                    below
                };
                count = count.wrapping_add(below);
                w.varint(below);
                S::generate(input).encode(&mut w)?;
                w.link(child);
            }
            if control & 64 != 0 {
                w.raw(input.bytes(4));
            }
        }
        nodes.push((w.finish()?, count));
    }
    let (root, count) = *nodes.last().unwrap();
    let mut w = Encoder::new(txn);
    w.varint(if input.chance(2) {
        input.num()
    } else {
        count.max(1)
    });
    S::generate(input).encode(&mut w)?;
    w.link(root);
    w.finish()
}

fn exercise<K, V, S>(input: &mut Input<'_>, store: &Store) -> ferro::Result<()>
where
    K: Gen + Data + Ord,
    V: Gen + Probe,
    S: Gen + Summary<K, V>,
{
    let mut txn = store.begin()?;
    let ptr = build::<K, V, S>(input, &mut txn)?;
    txn.commit(ptr)?;
    let map = store.load::<Map<K, V, S>>(ptr)?;
    let mut budget = 2000;
    map.probe(&mut budget);
    let key = K::generate(input);
    guarded(|| map.try_get(&key));
    guarded(|| map.contains_key(&key));
    guarded(|| map.range(..&key).count());
    let mut edited = (*map).clone();
    guarded(|| edited.insert(K::generate(input), V::generate(input)));
    guarded(|| edited.remove(&key));
    guarded(|| edited.iter().take(64).count());
    Ok(())
}

pub fn run(data: &[u8]) {
    let mut input = Input::new(data);
    let kind = input.byte();
    let path = crate::fresh("map.ferro");
    let options = StoreOptions {
        durability: Durability::Buffered,
        cache_bytes: 1 << 20,
        ..StoreOptions::default()
    };
    let Ok((store, _)) = Store::open(&path, options) else {
        return;
    };
    let _ = match kind % 6 {
        0 => exercise::<u64, String, ()>(&mut input, &store),
        1 => exercise::<String, Bytes, ()>(&mut input, &store),
        2 => exercise::<u64, u64, Total>(&mut input, &store),
        3 => exercise::<u64, Shop, ()>(&mut input, &store),
        4 => exercise::<String, Map<u64, String>, ()>(&mut input, &store),
        _ => exercise::<u8, i64, ()>(&mut input, &store),
    };
    store.close();
}
