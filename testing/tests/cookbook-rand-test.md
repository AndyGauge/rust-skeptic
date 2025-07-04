# Cookbook rand test reproduction

This test reproduces the exact cookbook scenario with `rand = "0.9"` in Cargo.toml.

```rust,edition2018
extern crate rand;
use rand::Rng;

fn main() {
    let mut rng = rand::thread_rng();
    let n: i32 = rng.gen_range(0..10);
    println!("{}", n);
}
``` 