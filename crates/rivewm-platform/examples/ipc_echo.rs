//! Manual check of the IPC pipe without running the WM:
//! `cargo run -p rivewm-platform --example ipc_echo`
//! Don't run it while rivewm itself is running; they share the pipe name.

use rivewm_platform::ipc;

fn main() {
    ipc::serve(|line| format!("echo: {line}")).expect("first server starts");
    println!("pipe: {}", ipc::pipe_name());

    for request in ["hello", "query state", "a  b  c"] {
        println!("{request:?} -> {:?}", ipc::request(request));
    }

    // Many clients back to back must all get through.
    let ok = (0..50).filter(|i| ipc::request(&i.to_string()).is_ok()).count();
    println!("{ok}/50 rapid requests answered");

    match ipc::serve(|_| String::new()) {
        Err(ipc::ServeError::AlreadyRunning) => println!("second server refused: ok"),
        other => println!("second server: unexpected {other:?}"),
    }
}
