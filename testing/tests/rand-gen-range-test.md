# Minimal rand gen_range test (Cookbook reproduction)

This test reproduces the cookbook issue where `rand = "0.9"` in Cargo.toml should work with `gen_range(0..10)` API.

```rust,edition2018
extern crate rand;
use rand::Rng;

fn main() {
    let mut rng = rand::thread_rng();
    let n: i32 = rng.gen_range(0..10);
    println!("{}", n);
} 