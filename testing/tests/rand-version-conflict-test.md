# Rand version conflict test

This test demonstrates the version conflict issue where:
- Project uses `rand = "0.8"` (newer API: `gen_range(0..10)`)
- `crossbeam = "0.5"` brings in `rand = "0.5"` (older API: `gen_range(0, 10)`)

This should cause a compilation error due to API mismatch.

```rust,edition2018
extern crate rand;
extern crate crossbeam;
use rand::Rng;

fn main() {
    let mut rng = rand::thread_rng();
    // This uses rand 0.8 API (range syntax)
    let n: i32 = rng.gen_range(0..10);
    println!("{}", n);
    
    // This should fail because crossbeam 0.5 expects rand 0.5 API
    // but we're linking against rand 0.8
}
``` 