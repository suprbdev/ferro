use wiki::chunk::{self, DEFAULT};
use wiki::model::Body;
use wiki::synth::{Rng, Vocab, prose};

#[test]
fn updating_a_body_gives_the_same_list_as_building_it() {
    let mut rng = Rng::new(11);
    let vocab = Vocab::new(2_000);
    let mut text = prose(&mut rng, &vocab, 40 * 1024, &mut |_| None);
    let mut body: Body = chunk::build(&text, &DEFAULT);
    for round in 0..60 {
        let at = text.floor_char_boundary(rng.below(text.len() as u64) as usize);
        let end = text.floor_char_boundary((at + rng.below(600) as usize).min(text.len()));
        match round % 3 {
            0 => text.replace_range(at..end, ""),
            1 => text.insert_str(at, &"fresh words here ".repeat(30)),
            _ => text.replace_range(at..end, "replacement"),
        }
        chunk::update(&mut body, &text, &DEFAULT);
        assert_eq!(chunk::text(&body), text);
        assert!(body == chunk::build(&text, &DEFAULT));
    }
    chunk::update(&mut body, "short", &DEFAULT);
    assert_eq!(body.len(), 1);
    assert_eq!(chunk::text(&body), "short");
}
