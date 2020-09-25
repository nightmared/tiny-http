use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::io::Write;
use tiny_http::Method;

#[test]
#[ignore]
// TODO: obtain time
fn curl_bench() {
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let port = server.server_addr().port();
    let num_requests = 10usize;

    match Command::new("curl")
        .arg("-s")
        .arg(format!("http://localhost:{}/?[1-{}]", port, num_requests))
        .output()
    {
        Ok(p) => p,
        Err(_) => return, // ignoring test
    };

    drop(server);
}

#[allow(unused)]
fn sequential_requests(bencher: &mut Criterion) {
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let port = server.server_addr().port();

    bencher.bench_function("sequential_requests", |b| {
        b.iter_batched(
            || std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
            |mut stream: std::net::TcpStream| {
                (write!(stream, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")).unwrap();

                let request = server.recv().unwrap();

                assert_eq!(request.method(), &Method::Get);

                let _ = request.respond(tiny_http::Response::new_empty(tiny_http::StatusCode(204)));
            },
            criterion::BatchSize::LargeInput,
        )
    });
}

#[allow(unused)]
fn parallel_requests(bencher: &mut Criterion) {
    fdlimit::raise_fd_limit();

    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let port = server.server_addr().port();

    bencher.bench_function("parallel_requests 100", |b| {
        b.iter(|| {
            let mut streams = Vec::new();

            for _ in 0..black_box(100usize) {
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                (write!(
                    stream,
                    "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                ))
                .unwrap();
                streams.push(stream);
            }

            loop {
                let request = match server.try_recv().unwrap() {
                    None => break,
                    Some(rq) => rq,
                };

                assert_eq!(request.method(), &Method::Get);

                request
                    .respond(tiny_http::Response::new_empty(tiny_http::StatusCode(204)))
                    .unwrap();
            }
        })
    });
}

criterion_group!(benches, sequential_requests, parallel_requests);
criterion_main!(benches);
