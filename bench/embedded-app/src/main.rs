use jato_embedded_bench::DATA;

fn main() {
    let index: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    println!("{} {}", DATA.len(), DATA[index % DATA.len()]);
}
