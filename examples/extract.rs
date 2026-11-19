//! Prints the main text shlv extracts from a page, for tuning `shlv::extract`.
//!
//! cargo run --example extract -- FILE|URL

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).ok_or("usage: extract FILE|URL")?;
    let html = if arg.starts_with("http://") || arg.starts_with("https://") {
        shlv::fetch::Fetcher::default().get(&arg)?.html
    } else {
        String::from_utf8_lossy(&std::fs::read(&arg)?).into_owned()
    };
    let t = std::time::Instant::now();
    let text = shlv::extract::extract(&html);
    let took = t.elapsed();
    println!("{text}");
    eprintln!(
        "{} words, {} bytes of HTML, {took:?}",
        text.split_whitespace().count(),
        html.len()
    );
    Ok(())
}
