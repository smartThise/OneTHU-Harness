//! Real loopback sockets only. In particular, cancellation assertions observe
//! EOF/reset on the server, not just an early return from the client wrapper.
use super::*;
use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct LocalHost {
    cancelled: Arc<AtomicBool>,
    // Deliberately !Send / !Sync: the public Host contract permits local state.
    _local: Rc<()>,
}

impl Host for LocalHost {
    fn call(&mut self, _: &str, _: &str, _: Value) -> Result<Value, String> {
        panic!("transport tests must not call a real host")
    }
    fn interrupted(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

struct Server {
    url: String,
    requests: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

fn consume_request(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = stream.read(&mut buf).expect("read local request");
        assert_ne!(n, 0, "client closed before request");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length = head
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return;
            }
        }
    }
}

impl Server {
    fn start(handler: impl Fn(usize, &mut TcpStream) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://localhost:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, peak2, stop2) = (requests.clone(), peak.clone(), stop.clone());
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            let mut workers = Vec::new();
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (count, active, peak, handler) = (
                            count.clone(),
                            active.clone(),
                            peak2.clone(),
                            handler.clone(),
                        );
                        workers.push(thread::spawn(move || {
                            // macOS accepts can inherit the listener's nonblocking
                            // flag; EOF assertions must actually await peer close.
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            consume_request(&mut stream);
                            let index = count.fetch_add(1, Ordering::SeqCst);
                            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            handler(index, &mut stream);
                            drop(stream);
                            active.fetch_sub(1, Ordering::SeqCst);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("local accept: {e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            url,
            requests,
            peak,
            stop,
            thread: Some(thread),
        }
    }

    fn cfg(&self, stream: bool) -> crate::config::Config {
        crate::config::Config::from_settings(
            json!({
                "provider":"custom", "apiKey":"test-not-a-secret", "baseUrl":self.url,
                "model":"local-test-model", "stream":if stream {"on"} else {"off"}
            })
            .as_object()
            .unwrap(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn response(stream: &mut TcpStream, status: u16, headers: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
}

fn success(stream: &mut TcpStream) {
    response(stream, 200, "Content-Type: text/event-stream\r\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from local server\"}}]}\n\ndata: [DONE]\n\n");
}

fn run(cfg: &crate::config::Config, flag: Arc<AtomicBool>) -> (Result<Turn, LlmError>, String) {
    let output = RefCell::new(String::new());
    let mut host = LocalHost {
        cancelled: flag,
        _local: Rc::new(()),
    };
    let result = chat_turn(
        &mut host,
        cfg,
        &[crate::session::Msg::user("local test")],
        &[],
        &mut Hooks {
            on_delta: Box::new(|s| output.borrow_mut().push_str(s)),
            on_think: Box::new(|_| {}),
            on_log: Box::new(|_| {}),
        },
    );
    (result, output.into_inner())
}

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn slow_headers_are_one_request_without_hedging() {
    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(3300));
        success(stream);
    });
    let (result, output) = run(&server.cfg(true), flag());
    assert!(
        result.is_ok(),
        "{}",
        result.err().map(|e| e.message).unwrap_or_default()
    );
    assert_eq!(output, "Hello from local server");
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(server.peak.load(Ordering::SeqCst), 1);
}

#[test]
fn retry_after_is_obeyed_and_retries_are_serial() {
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let observed = arrivals.clone();
    let server = Server::start(move |index, stream| {
        observed.lock().unwrap().push(Instant::now());
        if index == 0 {
            response(
                stream,
                429,
                "Retry-After: 1\r\n",
                "{\"error\":{\"message\":\"busy\"}}",
            );
        } else if index == 1 {
            response(
                stream,
                503,
                "Retry-After: 1\r\n",
                "{\"error\":{\"message\":\"temporarily unavailable\"}}",
            );
        } else {
            success(stream);
        }
    });
    assert!(run(&server.cfg(true), flag()).0.is_ok());
    assert_eq!(server.requests.load(Ordering::SeqCst), 3);
    assert_eq!(server.peak.load(Ordering::SeqCst), 1);
    let times = arrivals.lock().unwrap();
    assert!(times[1].duration_since(times[0]) >= Duration::from_millis(990));
    assert!(times[2].duration_since(times[1]) >= Duration::from_millis(990));
}

#[test]
fn persistent_429_has_a_finite_retry_limit() {
    let server = Server::start(|_, stream| {
        response(
            stream,
            429,
            "Retry-After: 0\r\n",
            "{\"error\":{\"message\":\"busy\"}}",
        )
    });
    assert!(run(&server.cfg(true), flag()).0.is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 3);
    assert_eq!(server.peak.load(Ordering::SeqCst), 1);
}

#[test]
fn long_retry_after_returns_wait_instruction_without_early_retry() {
    let server = Server::start(|_, stream| {
        response(
            stream,
            429,
            "Retry-After: 60\r\n",
            "{\"error\":{\"message\":\"busy\"}}",
        )
    });
    let error = run(&server.cfg(true), flag()).0.err().unwrap();
    assert!(error.message.contains("等待 60 秒"));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn authentication_failure_is_not_retried() {
    let server = Server::start(|_, stream| {
        response(
            stream,
            401,
            "",
            "{\"error\":{\"message\":\"expired token\"}}",
        )
    });
    let error = run(&server.cfg(true), flag()).0.err().unwrap();
    assert!(error.message.contains("HTTP 401"));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

fn cancellation_case(initial_response: &'static str, streaming: bool) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = Server::start(move |_, stream| {
        stream.write_all(initial_response.as_bytes()).unwrap();
        stream.flush().unwrap();
        ready_tx.send(()).unwrap();
        let mut byte = [0];
        let closed = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            _ => false,
        };
        closed_tx.send(closed).unwrap();
    });
    let cancelled = flag();
    let cancel = cancelled.clone();
    let interrupter = thread::spawn(move || {
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(80));
        let at = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        at
    });
    assert!(run(&server.cfg(streaming), cancelled).0.is_err());
    let cancelled_at = interrupter.join().unwrap();
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(1),
        "client did not cancel promptly"
    );
    assert!(
        closed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "server connection remained alive"
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_closes_socket_while_waiting_for_headers() {
    cancellation_case("", true);
}

#[test]
fn cancellation_closes_socket_during_stalled_sse() {
    cancellation_case("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n", true);
}

#[test]
fn cancellation_closes_socket_during_nonstream_body() {
    cancellation_case("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{\"choices\":", false);
}

#[test]
fn cancellation_during_backoff_does_not_send_the_next_attempt() {
    let (sent_tx, sent_rx) = mpsc::channel();
    let server = Server::start(move |_, stream| {
        response(
            stream,
            429,
            "Retry-After: 5\r\n",
            "{\"error\":{\"message\":\"busy\"}}",
        );
        sent_tx.send(()).unwrap();
    });
    let cancelled = flag();
    let cancel = cancelled.clone();
    let interrupter = thread::spawn(move || {
        sent_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    assert!(run(&server.cfg(true), cancelled).0.is_err());
    interrupter.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn partial_text_reasoning_and_tool_calls_are_never_replayed() {
    for delta in [
        json!({"content":"part"}),
        json!({"reasoning_content":"thinking"}),
        json!({"tool_calls":[{"index":0,"id":"call_1","function":{"name":"local_tool","arguments":"{"}}]}),
    ] {
        let server = Server::start(move |_, stream| {
            let body = format!("data: {}\n\ndata: {{\"error\":{{\"code\":429,\"message\":\"busy after partial\"}}}}\n\n", json!({"choices":[{"delta":delta}]}));
            response(stream, 200, "Content-Type: text/event-stream\r\n", &body);
        });
        assert!(run(&server.cfg(true), flag()).0.is_err());
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn empty_sse_failure_can_retry_before_any_output() {
    let server = Server::start(|index, stream| {
        if index == 0 {
            response(
                stream,
                200,
                "Content-Type: text/event-stream\r\n",
                "data: {\"error\":{\"code\":429,\"message\":\"busy\"}}\n\n",
            );
        } else {
            success(stream);
        }
    });
    assert!(run(&server.cfg(true), flag()).0.is_ok());
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn truncated_tool_call_at_clean_eof_is_not_a_success_or_retried() {
    let server = Server::start(|_, stream| {
        response(stream, 200, "Content-Type: text/event-stream\r\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"local_tool\",\"arguments\":\"{\"}}]}}]}\n\n");
    });
    let error = run(&server.cfg(true), flag()).0.err().unwrap();
    assert!(error.message.contains("完成前断开"));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn finish_reason_allows_eof_and_retains_trailing_usage() {
    let server = Server::start(|_, stream| {
        response(stream, 200, "Content-Type: text/event-stream\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n");
    });
    let turn = run(&server.cfg(true), flag())
        .0
        .unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(turn.content, "done");
    assert!(turn.usage.is_some());
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn fragmented_utf8_thinking_tools_and_usage_survive_streaming() {
    let server = Server::start(|_, stream| {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"<think>推理</think>你好\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7,\"total_tokens\":19}}\n\n",
            "data: [DONE]\n\n"
        );
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        for chunk in body.as_bytes().chunks(2) {
            stream.write_all(chunk).unwrap();
        }
    });
    let (result, output) = run(&server.cfg(true), flag());
    let turn = result.unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(turn.content, "你好");
    assert_eq!(output, "你好");
    assert_eq!(turn.reasoning, "推理");
    assert_eq!(turn.tool_calls[0].id, "call_1");
    assert_eq!(turn.tool_calls[0].arguments, "{\"q\":1}");
    assert!(turn.usage.is_some());
}

#[test]
fn retry_after_http_dates_and_jitter_are_bounded() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let date = httpdate::fmt_http_date(now + Duration::from_secs(12));
    assert_eq!(
        transport::retry_after(&date, now),
        Some(Duration::from_secs(12))
    );
    assert_eq!(transport::retry_after("garbage", now), None);
    for attempt in 0..2 {
        let delay = transport::retry_delay(&mut Failure::new("test", true), attempt).unwrap();
        assert!(delay >= Duration::from_millis(450 * (1 << attempt)));
        assert!(delay <= Duration::from_millis(550 * (1 << attempt)));
    }
}
