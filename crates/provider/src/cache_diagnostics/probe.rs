//! TEMPORARY ignored probe, no live APIs. See docs/cache-diagnostics.md.
//! Allocator counting is inactive except for the explicit measured invocation.
use super::*;
use crate::turn::{cache_preparation_probe_support::derive, prepare_turn_request};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::AtomicBool,
    time::Instant,
};

struct Counting;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Counting = Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: unchanged layout forwarded to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every pointer originated in System.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ACTIVE.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        // SAFETY: unchanged pointer/layout/size forwarded to System.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

async fn prepared(
    request: &ModelRequest<'_>,
    provider: &mut OpenAiProvider,
) -> crate::turn::PreparedRequest {
    let prepared = prepare_turn_request(request, provider).await.unwrap();
    #[cfg(feature = "cache-diagnostics")]
    let prepared = crate::cache_diagnostics::dispatch(prepared, provider);
    prepared
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "temporary release preparation/allocation probe; run alone with --ignored --nocapture --test-threads=1"]
async fn cache_preparation_probe() {
    #[cfg(feature = "cache-diagnostics")]
    {
        use crate::cache_diagnostics::socket::SocketDiagnostics;
        CALLS.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        let sidecar = SocketDiagnostics::default();
        ACTIVE.store(false, Ordering::Relaxed);
        println!(
            "socket_sidecar: inline_bytes={}, heap_requested_bytes={}, allocation_calls={}",
            std::mem::size_of::<SocketDiagnostics>(),
            BYTES.load(Ordering::Relaxed),
            CALLS.load(Ordering::Relaxed)
        );
        drop(sidecar);
    }
    println!(
        "feature,items,json_bytes,operation,median_us,p95_us,allocation_calls,requested_bytes,snapshot_bytes,baseline_bytes"
    );
    for (base_count, text_bytes) in [(4, 128), (436, 1112)] {
        let mut provider = connect_http_test_provider(
            "http://127.0.0.1:9/responses".into(),
            ToolServer::new().run(),
        )
        .await;
        let text = "word".repeat(text_bytes / 4);
        let history: Vec<_> = (0..base_count)
            .map(|index| {
                if index % 2 == 0 {
                    Message::user(text.clone())
                } else {
                    Message::assistant(text.clone())
                }
            })
            .collect();
        let initial = ModelRequest {
            instructions: test_instructions(),
            input: history.iter().map(ModelRequestItem::message).collect(),
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        let seed = prepared(&initial, &mut provider).await;
        let output = vec![
            json!({"type":"reasoning", "id":"rs_probe", "summary":[], "encrypted_content":text}),
            json!({"type":"message", "id":"msg_probe", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":text}]}),
        ];
        let replay = zevria_model::OwnedModelRequestItem::replay_backed(
            ProviderReplay::openai_responses(provider.profile.clone(), output.clone()),
        )
        .unwrap();
        #[cfg(feature = "cache-diagnostics")]
        {
            let response = zevria_model::ModelResponse::from_replay(
                ProviderReplay::openai_responses(provider.profile.clone(), output.clone()),
            )
            .unwrap();
            crate::cache_diagnostics::finish(&seed, &mut provider, Some(&response));
        }
        provider.ws.continuation = Some(ContinuationState {
            socket_generation: provider.ws.socket_generation,
            response_id: "resp_probe".into(),
            request_properties: seed.request_properties.clone(),
            request_input: seed.full_input.clone(),
            response_output: output,
        });
        let prompt = Message::user(text.clone());
        let mut input = initial.input;
        input.push(replay.as_borrowed());
        input.push(ModelRequestItem::message(&prompt));
        let request = ModelRequest {
            instructions: test_instructions(),
            input,
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        let retry_snapshot = prepared(&request, &mut provider).await;
        let json_bytes = serde_json::to_vec(&retry_snapshot.full_input)
            .unwrap()
            .len();
        let (snapshot_bytes, baseline_bytes) = (0usize, 0usize);
        #[cfg(feature = "cache-diagnostics")]
        let (snapshot_bytes, baseline_bytes) = {
            let _ = (snapshot_bytes, baseline_bytes);
            crate::cache_diagnostics::memory(&retry_snapshot, &provider)
        };
        for operation in [
            "prepare_only",
            "prepare_incremental",
            "prepare_full_replay",
            "retry_full_reuse",
        ] {
            let mut times = Vec::new();
            let mut counts = (0, 0);
            for sample in 0..121 {
                // 20 warmups, 100 uninstrumented timing samples, then one counted
                // allocation sample. Fixture construction/destruction is excluded.
                let counted = sample == 120;
                CALLS.store(0, Ordering::Relaxed);
                BYTES.store(0, Ordering::Relaxed);
                ACTIVE.store(counted, Ordering::Relaxed);
                let start = Instant::now();
                if operation == "retry_full_reuse" {
                    assert_eq!(derive(&retry_snapshot, &mut provider, true), base_count + 3);
                } else {
                    let snapshot = prepared(&request, &mut provider).await;
                    if operation != "prepare_only" {
                        let full = operation == "prepare_full_replay";
                        assert_eq!(
                            derive(&snapshot, &mut provider, full),
                            if full { base_count + 3 } else { 1 }
                        );
                    }
                    std::hint::black_box(snapshot);
                }
                let elapsed = start.elapsed().as_nanos();
                ACTIVE.store(false, Ordering::Relaxed);
                if counted {
                    counts = (CALLS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
                } else if sample >= 20 {
                    times.push(elapsed);
                }
            }
            times.sort_unstable();
            println!(
                "{},{},{},{},{:.3},{:.3},{},{},{},{}",
                cfg!(feature = "cache-diagnostics"),
                base_count + 3,
                json_bytes,
                operation,
                times[50] as f64 / 1000.0,
                times[95] as f64 / 1000.0,
                counts.0,
                counts.1,
                snapshot_bytes,
                baseline_bytes
            );
        }
    }
}

#[tokio::test]
#[ignore = "temporary feature-on/off wire equivalence capture; requires CACHE_DIAGNOSTICS_WIRE_OUTPUT"]
async fn cache_wire_equivalence_capture() {
    let websocket = incident_prefix_flow(true, [1000, 1000, 0, 1000]).await;
    let resumed_websocket_text = resume_wire::flow().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_request(&mut stream).await;
            requests.push(request.split_once("\r\n\r\n").unwrap().1.to_owned());
            if index == 0 {
                send_http_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    "{}",
                )
                .await;
            } else {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(
                        completed_event("resp_equivalence", "msg_equivalence", "same answer")
                            .to_string(),
                    ),
                )
                .await;
            }
        }
        requests
    });
    let mut provider = connect_http_test_provider(
        format!("http://{address}/responses"),
        ToolServer::new().run(),
    )
    .await;
    let prompt = Message::user("HTTP equivalence prompt");
    provider
        .complete(
            ModelRequest {
                instructions: test_instructions(),
                input: vec![ModelRequestItem::message(&prompt)],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    let http = server.await.unwrap();
    assert_eq!(http[0], http[1]);
    let path =
        std::env::var("CACHE_DIAGNOSTICS_WIRE_OUTPUT").expect("explicit disposable output path");
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({"websocket":websocket,"resumed_websocket_text":resumed_websocket_text,"http_text":http})).unwrap(),
    )
    .unwrap();
}
