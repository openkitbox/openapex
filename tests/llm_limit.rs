//! Endpoint concurrency gate: `LimitedChat` over a scripted fake `Chat`
//! (cases a-e, h) and over real TCP bytes through the real
//! `EndpointRouter::from_config` (case f). The gate must queue, never reject;
//! release a permit at the stream's terminal item (not at drop); and never
//! leak one, whatever happens to the caller.

use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use ox::cardinal::CardinalError;
use ox::cardinal::chat::{BoxChatStream, Chat, ChatFuture};
use ox::cardinal::client::CardinalClient;
use ox::cardinal::limit::{LimitedChat, QUEUE_WARN_AFTER};
use ox::cardinal::router::EndpointRouter;
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::ChatRequest;
use ox::config::{CandidateConfig, Config, EndpointLimits, ModelRouteConfig, ProviderConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

const BOUND: Duration = Duration::from_secs(5);

type Item = Result<StreamEvent, CardinalError>;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("non-zero")
}

fn text() -> Item {
    Ok(StreamEvent::TextDelta("x".to_owned()))
}

fn err() -> Item {
    Err(CardinalError::Upstream("boom".to_owned()))
}

// ---------------------------------------------------------------------------
// Scripted fake transport
// ---------------------------------------------------------------------------

/// What the fake does for the request whose `model` names this plan.
enum Plan {
    /// `stream_chat` itself fails.
    OpenErr,
    /// The stream yields whatever the test sends; closing the sender ends it.
    Gated(mpsc::UnboundedReceiver<Item>),
    /// The stream yields these items, pending once before each.
    Auto(Vec<Item>),
}

#[derive(Default)]
struct Counters {
    current: AtomicUsize,
    peak: AtomicUsize,
}

impl Counters {
    fn enter(&self) {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }
    fn leave(&self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct FakeChat {
    plans: Mutex<HashMap<String, Plan>>,
    reached: Mutex<Vec<String>>,
    counters: Arc<Counters>,
}

impl FakeChat {
    fn plan(&self, model: &str, plan: Plan) {
        self.plans.lock().unwrap().insert(model.to_owned(), plan);
    }
    /// Register a gated stream for `model`; the returned sender drives it.
    fn gated(&self, model: &str) -> mpsc::UnboundedSender<Item> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.plan(model, Plan::Gated(rx));
        tx
    }
    fn reached(&self, model: &str) -> bool {
        self.reached.lock().unwrap().iter().any(|m| m == model)
    }
    fn calls(&self) -> usize {
        self.reached.lock().unwrap().len()
    }
}

impl Chat for FakeChat {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            self.reached.lock().unwrap().push(request.model.clone());
            let plan = self
                .plans
                .lock()
                .unwrap()
                .remove(&request.model)
                .expect("a plan for this model");
            match plan {
                Plan::OpenErr => Err(CardinalError::Upstream("open failed".to_owned())),
                Plan::Gated(rx) => {
                    self.counters.enter();
                    Ok(Box::pin(FakeStream {
                        source: Source::Gated(rx),
                        counters: Arc::clone(&self.counters),
                        live: true,
                        yielded: false,
                    }) as BoxChatStream)
                }
                Plan::Auto(items) => {
                    self.counters.enter();
                    Ok(Box::pin(FakeStream {
                        source: Source::Auto(items.into()),
                        counters: Arc::clone(&self.counters),
                        live: true,
                        yielded: false,
                    }) as BoxChatStream)
                }
            }
        })
    }
}

enum Source {
    Gated(mpsc::UnboundedReceiver<Item>),
    Auto(VecDeque<Item>),
}

/// Counts itself as in flight until it yields a terminal item or is dropped.
struct FakeStream {
    source: Source,
    counters: Arc<Counters>,
    live: bool,
    yielded: bool,
}

impl FakeStream {
    fn finish(&mut self) {
        if std::mem::take(&mut self.live) {
            self.counters.leave();
        }
    }
}

impl Stream for FakeStream {
    type Item = Item;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Item>> {
        let this = self.get_mut();
        let polled = match &mut this.source {
            Source::Gated(rx) => rx.poll_recv(cx),
            Source::Auto(items) => {
                if !this.yielded {
                    this.yielded = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                this.yielded = false;
                Poll::Ready(items.pop_front())
            }
        };
        if let Poll::Ready(item) = &polled
            && matches!(item, None | Some(Err(_)) | Some(Ok(StreamEvent::Done)))
        {
            this.finish();
        }
        polled
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn limited(fake: &Arc<FakeChat>, max: usize) -> Arc<LimitedChat> {
    Arc::new(LimitedChat::new(
        Arc::clone(fake) as Arc<dyn Chat>,
        nz(max),
        "fake",
    ))
}

type Opened = Result<BoxChatStream, CardinalError>;

fn call(limiter: &Arc<LimitedChat>, model: &str) -> JoinHandle<Opened> {
    let limiter = Arc::clone(limiter);
    let request = ChatRequest::new(model, Vec::new());
    tokio::spawn(async move { limiter.stream_chat(&request).await })
}

async fn next(stream: &mut BoxChatStream) -> Option<Item> {
    poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

/// Let every runnable task make progress (current-thread runtimes only: with
/// no other source of progress, a bounded run of yields is deterministic).
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn opened(handle: JoinHandle<Opened>) -> BoxChatStream {
    tokio::time::timeout(BOUND, handle)
        .await
        .expect("the call must be admitted")
        .expect("task")
        .unwrap_or_else(|e| panic!("stream_chat failed: {e}"))
}

// ---------------------------------------------------------------------------
// a. queueing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn limit_one_queues_the_second_request_until_the_first_reaches_done() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx_a = fake.gated("a");
    let _tx_b = fake.gated("b");

    let mut stream_a = opened(call(&limiter, "a")).await;
    let b = call(&limiter, "b");
    settle().await;
    assert!(!fake.reached("b"), "b must queue behind a");

    tx_a.send(text()).unwrap();
    assert!(matches!(
        next(&mut stream_a).await,
        Some(Ok(StreamEvent::TextDelta(_)))
    ));
    settle().await;
    assert!(!fake.reached("b"), "a is still streaming");

    tx_a.send(Ok(StreamEvent::Done)).unwrap();
    assert!(matches!(
        next(&mut stream_a).await,
        Some(Ok(StreamEvent::Done))
    ));
    wait_until("b to be admitted", || fake.reached("b")).await;
    let _stream_b = opened(b).await;
}

#[tokio::test]
async fn limit_two_admits_two_and_queues_the_third_until_either_finishes() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 2);
    let _tx_a = fake.gated("a");
    let tx_b = fake.gated("b");
    let _tx_c = fake.gated("c");

    let _stream_a = opened(call(&limiter, "a")).await;
    let mut stream_b = opened(call(&limiter, "b")).await;
    let c = call(&limiter, "c");
    settle().await;
    assert!(!fake.reached("c"), "the third request must queue");
    assert_eq!(limiter.available(), 0);

    tx_b.send(Ok(StreamEvent::Done)).unwrap();
    assert!(matches!(
        next(&mut stream_b).await,
        Some(Ok(StreamEvent::Done))
    ));
    let _stream_c = opened(c).await;
    assert!(fake.reached("c"));
}

// ---------------------------------------------------------------------------
// b. deadlock guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn done_releases_the_permit_even_while_the_stream_is_kept_alive() {
    // The runner keeps its stream variable alive after Done while it joins
    // tool tasks, and a foreground subagent's child needs a permit then.
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx_a = fake.gated("a");
    let _tx_b = fake.gated("b");

    let mut stream_a = opened(call(&limiter, "a")).await;
    let b = call(&limiter, "b");
    settle().await;
    assert!(!fake.reached("b"));

    tx_a.send(Ok(StreamEvent::Done)).unwrap();
    assert!(matches!(
        next(&mut stream_a).await,
        Some(Ok(StreamEvent::Done))
    ));
    // `stream_a` is NOT dropped here.
    let _stream_b = opened(b).await;
    assert!(fake.reached("b"));
    drop(stream_a);
}

// ---------------------------------------------------------------------------
// c. release timing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permit_is_released_on_a_mid_stream_error_while_the_stream_lives() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx = fake.gated("a");
    let mut stream = opened(call(&limiter, "a")).await;
    assert_eq!(limiter.available(), 0);
    tx.send(text()).unwrap();
    tx.send(err()).unwrap();
    assert!(matches!(next(&mut stream).await, Some(Ok(_))));
    assert_eq!(limiter.available(), 0, "still streaming");
    assert!(matches!(next(&mut stream).await, Some(Err(_))));
    assert_eq!(limiter.available(), 1, "released by the Err item itself");
    drop(stream);
    assert_eq!(limiter.available(), 1);
}

#[tokio::test]
async fn permit_is_released_when_a_partially_read_stream_is_dropped() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx = fake.gated("a");
    let mut stream = opened(call(&limiter, "a")).await;
    tx.send(text()).unwrap();
    assert!(matches!(next(&mut stream).await, Some(Ok(_))));
    assert_eq!(limiter.available(), 0);
    drop(stream);
    assert_eq!(limiter.available(), 1);
}

#[tokio::test]
async fn permit_is_released_when_the_inner_stream_chat_fails() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    fake.plan("a", Plan::OpenErr);
    let Err(error) = limiter
        .stream_chat(&ChatRequest::new("a", Vec::new()))
        .await
    else {
        panic!("the open error must surface");
    };
    assert!(matches!(error, CardinalError::Upstream(_)), "{error}");
    assert_eq!(limiter.available(), 1);
}

#[tokio::test]
async fn permit_is_released_when_the_stream_ends_with_none() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx = fake.gated("a");
    let mut stream = opened(call(&limiter, "a")).await;
    assert_eq!(limiter.available(), 0);
    drop(tx);
    assert!(next(&mut stream).await.is_none());
    assert_eq!(limiter.available(), 1, "released by the None itself");
    drop(stream);
}

// ---------------------------------------------------------------------------
// d. FIFO and cancel safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queued_requests_are_admitted_in_arrival_order() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx_a = fake.gated("a");
    let tx_b = fake.gated("b");
    let _tx_c = fake.gated("c");

    let mut stream_a = opened(call(&limiter, "a")).await;
    let b = call(&limiter, "b");
    settle().await;
    let c = call(&limiter, "c");
    settle().await;

    tx_a.send(Ok(StreamEvent::Done)).unwrap();
    next(&mut stream_a).await;
    wait_until("b", || fake.reached("b")).await;
    settle().await;
    assert!(!fake.reached("c"), "c must wait for b, its predecessor");

    let mut stream_b = opened(b).await;
    tx_b.send(Ok(StreamEvent::Done)).unwrap();
    next(&mut stream_b).await;
    let _stream_c = opened(c).await;
}

#[tokio::test]
async fn a_cancelled_waiter_leaves_the_queue_without_starving_the_next() {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, 1);
    let tx_a = fake.gated("a");
    let _tx_b = fake.gated("b");
    let _tx_c = fake.gated("c");

    let mut stream_a = opened(call(&limiter, "a")).await;
    let b = call(&limiter, "b");
    settle().await;
    let c = call(&limiter, "c");
    settle().await;

    b.abort();
    let Err(joined) = b.await else {
        panic!("b was aborted while queued and cannot have completed");
    };
    assert!(joined.is_cancelled());

    tx_a.send(Ok(StreamEvent::Done)).unwrap();
    next(&mut stream_a).await;
    let stream_c = opened(c).await;
    assert!(!fake.reached("b"), "the cancelled waiter never ran");
    drop(stream_c);
    drop(stream_a);
    assert_eq!(limiter.available(), 1, "no permit leaked");
}

// ---------------------------------------------------------------------------
// e. deterministic stress
// ---------------------------------------------------------------------------

async fn stress(max: usize, tasks: usize) {
    let fake = Arc::new(FakeChat::default());
    let limiter = limited(&fake, max);
    for i in 0..tasks {
        let items = match i % 4 {
            0 => vec![text(), text(), Ok(StreamEvent::Done)],
            1 => vec![text(), err()],
            _ => vec![text(), text(), text(), text()],
        };
        fake.plan(&format!("m{i}"), Plan::Auto(items));
    }
    let handles: Vec<JoinHandle<()>> = (0..tasks)
        .map(|i| {
            let limiter = Arc::clone(&limiter);
            let request = ChatRequest::new(format!("m{i}"), Vec::new());
            tokio::spawn(async move {
                let mut stream = limiter.stream_chat(&request).await.expect("opens");
                match i % 4 {
                    // Read to the end / until the mid-stream error.
                    0 | 1 => while next(&mut stream).await.is_some() {},
                    // Read one item, then drop the stream half-read.
                    2 => {
                        next(&mut stream).await;
                    }
                    // Aborted by the driver (while queued or mid-read).
                    _ => while next(&mut stream).await.is_some() {},
                }
            })
        })
        .collect();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    for (i, handle) in handles.iter().enumerate() {
        if i % 4 == 3 {
            handle.abort();
        }
    }
    for handle in handles {
        match tokio::time::timeout(BOUND, handle)
            .await
            .expect("no task may hang")
        {
            Ok(()) => {}
            Err(e) => assert!(e.is_cancelled(), "task panicked: {e}"),
        }
    }
    assert!(
        fake.counters.peak() <= max,
        "peak in-flight {} exceeded {max}",
        fake.counters.peak()
    );
    assert!(fake.counters.peak() >= 1);
    assert_eq!(limiter.available(), max, "permits leaked");
    assert_eq!(fake.counters.current.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stress_current_thread_never_exceeds_the_limit_and_leaks_nothing() {
    stress(4, 64).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_multi_thread_never_exceeds_the_limit_and_leaks_nothing() {
    stress(4, 64).await;
}

// ---------------------------------------------------------------------------
// f. real bytes through the real router
// ---------------------------------------------------------------------------

const HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
const FINISH: &str = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn event(text: &str) -> String {
    format!(
        "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n"
    )
}

/// Read one HTTP request (head + content-length body), discarding it.
async fn drain_request(socket: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if socket.read_exact(&mut byte).await.is_err() {
            return;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    socket.read_exact(&mut body).await.ok();
}

/// A mock endpoint that counts accepted/concurrent connections and holds each
/// response open (after one event) until the test hands out a release ticket.
struct HoldingMock {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    counters: Arc<Counters>,
    release: Arc<Semaphore>,
    task: JoinHandle<()>,
}

impl HoldingMock {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counters = Arc::new(Counters::default());
        let release = Arc::new(Semaphore::new(0));
        let task = {
            let (accepted, counters, release) = (
                Arc::clone(&accepted),
                Arc::clone(&counters),
                Arc::clone(&release),
            );
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    counters.enter();
                    let (counters, release) = (Arc::clone(&counters), Arc::clone(&release));
                    tokio::spawn(async move {
                        drain_request(&mut socket).await;
                        socket
                            .write_all(format!("{HEAD}{}", event("hi")).as_bytes())
                            .await
                            .ok();
                        release.acquire().await.expect("open").forget();
                        // Leave the in-flight count BEFORE the final bytes: the
                        // client may admit the next request the instant it reads
                        // them.
                        counters.leave();
                        socket.write_all(FINISH.as_bytes()).await.ok();
                    });
                }
            })
        };
        Self {
            addr,
            accepted,
            counters,
            release,
            task,
        }
    }
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn router_config(addr: SocketAddr, limit: Option<usize>) -> Config {
    let mut config = Config::default();
    config.llm.providers.insert(
        "x".to_owned(),
        ProviderConfig {
            base_url: format!("http://{addr}"),
            api_key: None,
            limits: EndpointLimits {
                max_concurrent: limit.map(nz),
                stall_timeout_secs: None,
            },
        },
    );
    config.llm.models.insert(
        "m".to_owned(),
        ModelRouteConfig {
            candidates: vec![CandidateConfig {
                provider: "x".to_owned(),
                model: "wire".to_owned(),
            }],
        },
    );
    config
}

fn router(config: &Config) -> Arc<EndpointRouter> {
    // The implicit endpoint is never contacted (explicit routes disable it).
    let cardinal = CardinalClient::new("http://127.0.0.1:9", None).unwrap();
    Arc::new(EndpointRouter::from_config(&config.llm, cardinal).unwrap())
}

/// Two concurrent requests through `router`, each drained to the end.
fn drain_two(router: &Arc<EndpointRouter>, model: &str) -> [JoinHandle<()>; 2] {
    [0, 1].map(|_| {
        let router = Arc::clone(router);
        let request = ChatRequest::new(model, Vec::new());
        tokio::spawn(async move {
            let mut stream = router.stream_chat(&request).await.expect("opens");
            let mut done = false;
            while let Some(item) = next(&mut stream).await {
                done |= matches!(item.expect("a clean stream"), StreamEvent::Done);
            }
            assert!(done, "the stream must end with Done");
        })
    })
}

async fn finish_all(handles: [JoinHandle<()>; 2]) {
    for handle in handles {
        tokio::time::timeout(BOUND, handle)
            .await
            .expect("request completes")
            .expect("no panic");
    }
}

#[tokio::test]
async fn configured_provider_limit_serializes_real_connections() {
    let mock = HoldingMock::start().await;
    let router = router(&router_config(mock.addr, Some(1)));
    let handles = drain_two(&router, "m");

    wait_until("the first connection", || mock.accepted() >= 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.accepted(), 1, "the second request must queue");

    mock.release.add_permits(1);
    wait_until("the second connection", || mock.accepted() == 2).await;
    mock.release.add_permits(1);
    finish_all(handles).await;
    assert_eq!(mock.counters.peak(), 1);
    mock.task.abort();
}

#[tokio::test]
async fn without_a_limit_both_connections_are_open_at_once() {
    let mock = HoldingMock::start().await;
    let router = router(&router_config(mock.addr, None));
    let handles = drain_two(&router, "m");

    wait_until("both connections", || mock.accepted() == 2).await;
    mock.release.add_permits(2);
    finish_all(handles).await;
    assert_eq!(mock.counters.peak(), 2);
    mock.task.abort();
}

#[tokio::test]
async fn cardinal_limits_gate_the_implicit_passthrough_endpoint() {
    let mock = HoldingMock::start().await;
    let mut config = Config::default();
    config.llm.cardinal_url = format!("http://{}", mock.addr);
    config.llm.cardinal_limits.max_concurrent = Some(nz(1));
    let cardinal = CardinalClient::from_config(&config.llm).unwrap();
    let router = Arc::new(EndpointRouter::from_config(&config.llm, cardinal).unwrap());
    let handles = drain_two(&router, "any-model");

    wait_until("the first connection", || mock.accepted() >= 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.accepted(), 1, "the second request must queue");
    mock.release.add_permits(1);
    wait_until("the second connection", || mock.accepted() == 2).await;
    mock.release.add_permits(1);
    finish_all(handles).await;
    assert_eq!(mock.counters.peak(), 1);
    mock.task.abort();
}

// ---------------------------------------------------------------------------
// h. queue warning
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl SharedBuf {
    fn warns(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains("WARN"))
            .map(str::to_owned)
            .collect()
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_queued_past_the_threshold_warns_once_while_still_queued() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let fake = Arc::new(FakeChat::default());
    let limiter = Arc::new(LimitedChat::new(
        Arc::clone(&fake) as Arc<dyn Chat>,
        nz(1),
        "p-test",
    ));
    let tx_a = fake.gated("a");
    let _tx_b = fake.gated("b");

    let mut stream_a = opened(call(&limiter, "a")).await;
    let b = call(&limiter, "b");
    settle().await;
    assert!(buf.warns().is_empty(), "no warn before the threshold");

    tokio::time::advance(QUEUE_WARN_AFTER - Duration::from_secs(1)).await;
    settle().await;
    assert!(buf.warns().is_empty(), "still under the threshold");

    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert!(!fake.reached("b"), "b is still queued");
    let warns = buf.warns();
    assert_eq!(warns.len(), 1, "exactly one warn while queued: {warns:?}");
    for needle in ["p-test", "max=1", "in_flight=1", "still queued"] {
        assert!(warns[0].contains(needle), "missing {needle}: {}", warns[0]);
    }

    tokio::time::advance(QUEUE_WARN_AFTER * 4).await;
    settle().await;
    assert_eq!(buf.warns().len(), 1, "waiting longer must not warn again");

    // The same pinned acquire future kept its place: finishing a admits b.
    tx_a.send(Ok(StreamEvent::Done)).unwrap();
    next(&mut stream_a).await;
    let _stream_b = opened(b).await;
    assert_eq!(buf.warns().len(), 1);
    assert_eq!(fake.calls(), 2);
}

// ---------------------------------------------------------------------------
// i. production shape: real HTTP server + real router + queued interrupt
// ---------------------------------------------------------------------------

/// Counts how many requests reached the chat seam and how many of those
/// futures were dropped before opening (i.e. cancelled while queued).
struct Spy {
    inner: Arc<EndpointRouter>,
    entered: AtomicUsize,
    cancelled_unopened: AtomicUsize,
}

struct UnopenedGuard<'a> {
    cancelled: &'a AtomicUsize,
    done: bool,
}

impl Drop for UnopenedGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Chat for Spy {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let open = self.inner.stream_chat(request);
        Box::pin(async move {
            let mut guard = UnopenedGuard {
                cancelled: &self.cancelled_unopened,
                done: false,
            };
            let opened = open.await;
            guard.done = true;
            opened
        })
    }
}

struct App {
    base: String,
    client: reqwest::Client,
    events: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    _dir: tempfile::TempDir,
    _server: JoinHandle<()>,
}

impl App {
    async fn start(chat: Arc<dyn Chat>, config: Config) -> Self {
        use salvo::prelude::*;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut config = config;
        config.db_path = dir.path().join("ox.db");
        config.listen = format!("127.0.0.1:{port}");
        config.magic_context.enabled = false;
        let state = ox::build_state_with_home(config, chat, ox::runner::ToolRegistry::new(), home)
            .await
            .expect("build state");
        let mut rx = state.bus.subscribe();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        tokio::spawn(async move {
            while let Ok(envelope) = rx.recv().await {
                sink.lock().unwrap().push((envelope.kind, envelope.data));
            }
        });
        let router = ox::build_router(state);
        let listen = format!("127.0.0.1:{port}");
        let server = tokio::spawn(async move {
            let acceptor = salvo::conn::TcpListener::new(listen).bind().await;
            Server::new(acceptor).serve(router).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if client.get(format!("{base}/api/info")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            base,
            client,
            events,
            _dir: dir,
            _server: server,
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert!(
            response.status().is_success(),
            "POST {path}: {}",
            response.status()
        );
        response.json().await.unwrap_or(serde_json::Value::Null)
    }

    /// A session routed to model `m`; the title keeps title generation (an
    /// extra chat call) out of the picture.
    async fn session(&self) -> String {
        let body = self
            .post(
                "/api/session",
                serde_json::json!({
                    "title": "queue test",
                    "model": {"id": "m", "providerID": "x"},
                }),
            )
            .await;
        body["data"]["id"].as_str().expect("session id").to_owned()
    }

    async fn prompt(&self, session: &str) {
        self.post(
            &format!("/api/session/{session}/prompt"),
            serde_json::json!({ "text": "hello" }),
        )
        .await;
    }

    fn has_event(&self, session: &str, kind: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|(k, data)| k == kind && data["sessionID"] == session)
    }

    async fn wait_event(&self, session: &str, kind: &str) {
        let what = format!("{kind} for {session}");
        wait_until(&what, || self.has_event(session, kind)).await;
    }
}

#[tokio::test]
async fn interrupting_a_queued_session_leaves_the_endpoint_slot_intact() {
    let mock = HoldingMock::start().await;
    let config = router_config(mock.addr, Some(1));
    let spy = Arc::new(Spy {
        inner: router(&config),
        entered: AtomicUsize::new(0),
        cancelled_unopened: AtomicUsize::new(0),
    });
    let app = App::start(Arc::clone(&spy) as Arc<dyn Chat>, config).await;

    // Session 1 takes the only slot; the mock holds its response open.
    let first = app.session().await;
    app.prompt(&first).await;
    wait_until("session 1's connection", || mock.accepted() == 1).await;

    // Session 2's turn reaches the chat seam and queues behind it.
    let second = app.session().await;
    app.prompt(&second).await;
    wait_until("session 2's request at the chat seam", || {
        spy.entered.load(Ordering::SeqCst) == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.accepted(), 1, "session 2 must queue, not connect");

    // Interrupting the queued turn cancels it without ever connecting.
    let body = app
        .post(
            &format!("/api/session/{second}/interrupt"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(body["interrupted"], true);
    app.wait_event(&second, "session.execution.interrupted")
        .await;
    assert_eq!(spy.cancelled_unopened.load(Ordering::SeqCst), 1);
    assert_eq!(mock.accepted(), 1, "the cancelled request never connected");
    assert!(!app.has_event(&first, "session.execution.interrupted"));

    // Releasing session 1 completes it normally.
    mock.release.add_permits(1);
    app.wait_event(&first, "session.execution.succeeded").await;

    // The slot is free again: a third turn is served without queueing.
    mock.release.add_permits(1);
    let third = app.session().await;
    app.prompt(&third).await;
    app.wait_event(&third, "session.execution.succeeded").await;
    assert_eq!(mock.accepted(), 2);
    assert_eq!(mock.counters.peak(), 1);
    mock.task.abort();
}
