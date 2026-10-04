//! Random VM names: small, quick things.

const ADJECTIVES: &[&str] = &[
    "brave", "brisk", "bold", "bouncy", "busy", "clever", "cozy", "dapper", "eager", "fleet",
    "frisky", "fuzzy", "gentle", "happy", "jolly", "jumpy", "keen", "lively", "lucky", "merry",
    "nimble", "peppy", "perky", "plucky", "quick", "quiet", "rapid", "scrappy", "snappy", "spry",
    "sunny", "swift", "tidy", "tiny", "wee", "zippy",
];

const CRITTERS: &[&str] = &[
    "ant", "bat", "bee", "chick", "cricket", "finch", "gecko", "gnat", "hamster", "hare", "kit",
    "lark", "midge", "minnow", "mite", "mole", "mouse", "newt", "otter", "pika", "piglet", "pup",
    "shrew", "sparrow", "squirrel", "stoat", "tadpole", "vole", "weasel", "wren",
];

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: buffer is valid for 8 bytes.
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n != 8 {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        return t.as_nanos() as u64 ^ u64::from(std::process::id());
    }
    u64::from_le_bytes(b)
}

pub fn random() -> String {
    let r = random_u64();
    let a = ADJECTIVES[(r % ADJECTIVES.len() as u64) as usize];
    let c = CRITTERS[((r >> 32) % CRITTERS.len() as u64) as usize];
    format!("{a}-{c}")
}
