//! Normalize one moderation keyword per input line into the canonical tokenized
//! form used by the runtime matcher and the admin import path.
//!
//! Usage: `cargo run -p llm-access-core --example normalize_moderation -- IN
//! OUT` Reads `IN` (one keyword per line, UTF-8), writes `OUT` with the
//! normalized form of each line. Empty results are preserved as blank lines so
//! the caller can zip output lines back to input lines by index.

use std::{env, fs};

use llm_access_core::moderation::normalize_moderation_text;

fn main() {
    let args: Vec<String> = env::args().collect();
    let input_path = args.get(1).expect("usage: normalize_moderation IN OUT");
    let output_path = args.get(2).expect("usage: normalize_moderation IN OUT");
    let input = fs::read_to_string(input_path).expect("read input file");
    let mut output = String::with_capacity(input.len());
    for line in input.lines() {
        output.push_str(&normalize_moderation_text(line));
        output.push('\n');
    }
    fs::write(output_path, output).expect("write output file");
}
