use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let [target, input, out] = &args[..] else {
        panic!("usage: fixture <open|admin|log_entry|sync> <fuzz input> <output .db>");
    };
    let data = fs::read(input).unwrap();
    let out = PathBuf::from(out);
    let _ = fs::remove_file(&out);
    match target.as_str() {
        "open" | "admin" => {
            let (mode, bytes) = ferro_fuzz::file_bytes(&data).expect("empty input");
            fs::write(&out, bytes).unwrap();
            println!(
                "mode {mode:#04x}: kind {}, {:?}",
                (mode >> 2) & 0x0f,
                ferro_fuzz::Plan::of(mode)
            );
        }
        "log_entry" => {
            let mode = ferro_fuzz::log_entry::write(&data, &out).expect("input builds no file");
            println!(
                "mode {mode:#04x}: read_only {}, admin {}",
                mode & 1 != 0,
                mode & 2 != 0
            );
        }
        "sync" => {
            let prepared = ferro_fuzz::sync::prepare(&data).expect("input builds no bundle");
            let scenario = prepared.scenario;
            fs::write(&out, &scenario.image).unwrap();
            let bundle = out.with_extension("bundle");
            fs::write(&bundle, &prepared.bundle).unwrap();
            println!(
                "scenario {}: receive on {} as {}, mode {:#04x}, interfere {}, chunk {}, fail at {}, interrupt every {}, bundle of {} bytes in {}",
                scenario.name,
                scenario.branch,
                scenario.remote,
                prepared.mode,
                prepared.interfere % 8,
                prepared.chunk,
                prepared.fail_at,
                prepared.interrupt,
                prepared.bundle.len(),
                bundle.display()
            );
        }
        other => panic!("no fixture form for target {other}"),
    }
}
