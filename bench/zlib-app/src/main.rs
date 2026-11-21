use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use std::io::{Read, Write};

fn main() {
    let text = b"verified native library round trip";
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut output = Vec::new();
    ZlibDecoder::new(&compressed[..]).read_to_end(&mut output).unwrap();
    assert_eq!(output, text);
    println!("{}", String::from_utf8(output).unwrap());
}
