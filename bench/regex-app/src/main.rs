fn main() {
    let expression = regex::Regex::new(r"[[:alpha:]]+").unwrap();
    println!("{}", expression.find_iter("one two three").count());
}
