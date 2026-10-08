//! Manual check of the IPC pipe without running the WM:
//! `cargo run -p rivewm-platform --example ipc_echo`
//! Don't run it while rivewm itself is running; they share the pipe name.

use rivewm_platform::ipc;

fn main() {
    ipc::serve(|line, conn| {
        if line == "count" {
            // A streaming reply, like `subscribe`.
            for i in 0..3 {
                let _ = conn.send_line(&i.to_string());
            }
        } else {
            let _ = conn.send_line(&format!("echo: {line}"));
        }
    })
    .expect("first server starts");
    println!("pipe: {}", ipc::pipe_name());

    for request in ["hello", "query state", "a  b  c"] {
        println!("{request:?} -> {:?}", ipc::request(request));
    }

    // A long-lived client must not block others.
    let _held = ipc::stream("count").unwrap();
    println!(
        "while a stream is open: {:?}",
        ipc::request("still answered")
    );
    let lines: Vec<_> = ipc::stream("count").unwrap().map(Result::unwrap).collect();
    println!("streamed: {lines:?}");

    let ok = (0..50)
        .filter(|i| ipc::request(&i.to_string()).is_ok())
        .count();
    println!("{ok}/50 rapid requests answered");

    match ipc::serve(|_, _| {}) {
        Err(ipc::ServeError::AlreadyRunning) => println!("second server refused: ok"),
        other => println!("second server: unexpected {:?}", other.err()),
    }
}
