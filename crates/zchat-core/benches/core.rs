use std::{hint::black_box, time::Instant};
use zchat_core::{EventJournal, Request, RequestPolicy, Value, obj, parse};

fn measure(name: &str, iterations: u64, mut run: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let elapsed = started.elapsed();
    println!(
        "{{\"workload\":\"{name}\",\"iterations\":{iterations},\"elapsed_ns\":{},\"ns_per_operation\":{:.2}}}",
        elapsed.as_nanos(),
        elapsed.as_nanos() as f64 / iterations as f64
    );
}

fn main() {
    let input = r#"{"account":"matrix","operation":"message.send","params":{"conversation":"!example:localhost","text":"Hello, 世界","idempotency_key":"example-1"},"actor":"human"}"#;
    measure("json_parse_small_request", 20_000, || {
        black_box(parse(black_box(input)).unwrap());
    });
    let request = Request::parse(input).unwrap();
    measure("request_policy_validation", 20_000, || {
        RequestPolicy::interactive()
            .authorize(black_box(&request))
            .unwrap();
    });
    let mut journal = EventJournal::new(1024).unwrap();
    let mut sequence = 0u64;
    measure("bounded_journal_append", 20_000, || {
        sequence += 1;
        black_box(
            journal
                .append(
                    "matrix",
                    "message.received",
                    None,
                    obj([("sequence", Value::from(sequence))]),
                )
                .unwrap(),
        );
    });
    measure("journal_page_50", 20_000, || {
        black_box(journal.poll(None, 50).unwrap());
    });
}
