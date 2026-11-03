use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let [target, input, out] = &args[..] else {
        panic!("usage: fixture <open|admin|log_entry> <fuzz input> <output .db>");
    };
    let data = fs::read(input).unwrap();
    let out = PathBuf::from(out);
    let _ = fs::remove_file(&out);
    match target.as_str() {
        "open" | "admin" => {
            let (mode, bytes) = ferro_fuzz::file_bytes(&data).expect("empty input");
            fs::write(&out, bytes).unwrap();
            println!("mode {mode:#04x}: kind {}, read_only {}", mode >> 2, mode & 2 != 0);
        }
        "log_entry" => {
            let mode = ferro_fuzz::log_entry::write(&data, &out).expect("input builds no file");
            println!("mode {mode:#04x}: read_only {}, admin {}", mode & 1 != 0, mode & 2 != 0);
        }
        other => panic!("no fixture form for target {other}"),
    }
}
