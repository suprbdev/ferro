#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ferro_fuzz::install_hook();
    ferro_fuzz::run_admin_input(data);
});
