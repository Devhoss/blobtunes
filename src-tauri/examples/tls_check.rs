fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).expect("usage: tls_check <url>");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap();
    for (label, range) in [("closed 0-999", "bytes=0-999"), ("open 0-", "bytes=0-")] {
        let r = client
            .get(url)
            .header("Range", range)
            .header("User-Agent", "Mozilla/5.0 Chrome/126")
            .send();
        match r {
            Ok(resp) => println!("{label}: HTTP {}", resp.status()),
            Err(e) => println!("{label}: ERROR {e}"),
        }
    }
}
