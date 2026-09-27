#![cfg_attr(feature = "allocation-probe", allow(dead_code, unused_imports))]

use std::{hint::black_box, io::Write, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion};
use rig_core::message::Message;
use zevria_core::benchmark::AdmissionFixture;
use zevria_core::transcript_bench::Workload;
use zevria_core::transcript_bench::fixture;
use zevria_core::transcript_bench::layout;
use zevria_core::transcript_bench::owned_snapshot;
use zevria_foundation::SessionMode;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript;
use zevria_transcript::transcript::Conversation;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

#[cfg(feature = "allocation-probe")]
#[path = "support/allocations.rs"]
mod allocations;

fn in_memory(c: &mut Criterion) {
    for workload in [
        Workload::Plain,
        Workload::Replay,
        Workload::Mixed,
        Workload::Images,
    ] {
        for count in [10, 100, 1000] {
            if matches!(workload, Workload::Images) && count > 100 {
                continue;
            }
            for bytes in [128, 8192] {
                let f = fixture(workload, count, bytes);
                let name = format!("{}/{count}/{bytes}", workload.name());
                println!(
                    "fixture {name}: live={}, durable={}, jsonl_bytes={}",
                    f.items.len(),
                    f.lines.len(),
                    f.jsonl.len()
                );
                let mut group = c.benchmark_group(&name);
                group.bench_function("encode", |b| {
                    b.iter(|| {
                        for item in &f.items {
                            black_box(serde_json::to_vec(black_box(item)).unwrap());
                        }
                    })
                });
                group.bench_function("decode", |b| {
                    b.iter(|| {
                        for line in &f.lines {
                            black_box(
                                serde_json::from_slice::<TranscriptItem>(black_box(line)).unwrap(),
                            );
                        }
                    })
                });
                group.bench_function("validate", |b| {
                    b.iter(|| SessionReplayError::validate(black_box(&f.items)).unwrap())
                });
                group.bench_function("project", |b| {
                    b.iter(|| black_box(transcript::model_input(black_box(&f.items))))
                });
                group.bench_function("snapshot", |b| {
                    b.iter(|| black_box(owned_snapshot(black_box(&f.items))))
                });
                let snapshot = owned_snapshot(&f.items);
                group.bench_function("owned_encode", |b| {
                    b.iter(|| black_box(serde_json::to_vec(black_box(&snapshot)).unwrap()))
                });
                group.bench_function("restore", |b| {
                    b.iter(|| {
                        black_box(zevria_transcript::reconstruct_transcript(black_box(
                            &f.items,
                        )))
                    })
                });
                // File setup is outside the timed load; includes bounded reading,
                // per-record decode and complete replay/lifecycle validation.
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("load.jsonl");
                std::fs::write(&path, &f.jsonl).unwrap();
                group.bench_function("load", |b| {
                    b.iter(|| black_box(transcript::load(black_box(&path)).unwrap()))
                });
                group.finish();
            }
        }
    }
}

fn transactions(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    for count in [10, 100, 1000] {
        let f = fixture(Workload::Replay, count, 128);
        let mut group = c.benchmark_group(format!("transactions/{count}"));
        for operation in [
            "required",
            "completed",
            "required_batch",
            "completed_batch",
            "tail",
            "mode",
            "models",
        ] {
            group.bench_function(operation, |b| {
                b.iter_batched_ref(
                    || {
                        let directory = tempfile::tempdir().unwrap();
                        let mut writer =
                            TranscriptWriter::create_with_id(directory.path(), "benchmark")
                                .unwrap();
                        writer.rewrite(&f.items).unwrap();
                        let mut conversation = Conversation::new(writer);
                        conversation.adopt_persisted(f.items.clone());
                        (directory, conversation)
                    },
                    |(_, conversation)| {
                        let next = || TranscriptItem::Message(Message::user("next prompt"));
                        match operation {
                            "required" => conversation.push_required(next()).unwrap(),
                            "completed" => conversation.push_completed(next()).unwrap(),
                            "required_batch" => {
                                conversation.push_required_batch(vec![next()]).unwrap()
                            }
                            "completed_batch" => {
                                conversation.push_completed_batch(vec![next()]).unwrap()
                            }
                            "tail" => conversation
                                .replace_from_items(count - 2, vec![next()])
                                .unwrap(),
                            "mode" => {
                                conversation
                                    .replace_session_mode(SessionMode::Plan)
                                    .unwrap();
                            }
                            "models" => conversation
                                .replace_session_models(
                                    zevria_model::models::SessionModels::new(
                                        zevria_model::models::ModelSelection::new(
                                            zevria_foundation::ModelProfileRef::new(
                                                "transcript-bench",
                                                "other",
                                            ),
                                            zevria_foundation::ReasoningLevel::Medium,
                                        ),
                                        zevria_model::models::ModelSelection::new(
                                            zevria_core::transcript_bench::profile(),
                                            zevria_foundation::ReasoningLevel::Medium,
                                        ),
                                    )
                                    .unwrap(),
                                )
                                .unwrap(),
                            _ => unreachable!(),
                        }
                    },
                    BatchSize::PerIteration,
                )
            });
        }
        for operation in ["admit_required", "admit_completed"] {
            group.bench_function(operation, |b| {
                b.iter_batched_ref(
                    || AdmissionFixture::new(f.items.clone()),
                    |session| {
                        if operation == "admit_required" {
                            session.required("next")
                        } else {
                            session.completed("next")
                        }
                    },
                    BatchSize::PerIteration,
                )
            });
        }
        let mut session = AdmissionFixture::new(f.items.clone());
        group.bench_function("prepare_prompt", |b| {
            b.iter(|| session.prepare(black_box("next")))
        });
        let completed = [TranscriptItem::Message(Message::assistant("completed"))];
        group.bench_function("account_completed", |b| {
            b.iter(|| session.account_completed(black_box(&completed)))
        });
        group.finish();
    }
}

fn prompt_admission(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mut group = c.benchmark_group("admit_prompt");
    for count in [10, 1000] {
        for bytes in [128, 8192] {
            for (kind, activated, skill) in [
                ("message", false, false),
                ("activation", false, true),
                ("reapplication", true, true),
            ] {
                let session = AdmissionFixture::prompt(count, bytes, activated);
                for (anchor, edit) in [("append", false), ("edit", true)] {
                    group.bench_function(format!("{anchor}/{kind}/{count}/{bytes}"), |b| {
                        b.iter(|| session.admit(black_box(edit), black_box(skill)))
                    });
                }
            }
        }
    }
    group.finish();
}

fn prompt_pipeline(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    for operation in ["measure", "accept"] {
        let mut group = c.benchmark_group(format!("admit_prompt_pipeline/{operation}"));
        for count in [10, 1000] {
            for bytes in [128, 8192] {
                for (kind, activated, skill) in [
                    ("message", false, false),
                    ("activation", false, true),
                    ("reapplication", true, true),
                ] {
                    for (anchor, edit) in [("append", false), ("edit", true)] {
                        group.bench_function(format!("{anchor}/{kind}/{count}/{bytes}"), |b| {
                            if operation == "measure" {
                                let mut session = AdmissionFixture::prompt(count, bytes, activated);
                                b.iter(|| {
                                    runtime.block_on(
                                        session.measure(black_box(edit), black_box(skill)),
                                    )
                                });
                            } else {
                                b.iter_batched_ref(
                                    || AdmissionFixture::prompt(count, bytes, activated),
                                    |session| {
                                        runtime.block_on(
                                            session.accept(black_box(edit), black_box(skill)),
                                        )
                                    },
                                    BatchSize::PerIteration,
                                );
                            }
                        });
                    }
                }
            }
        }
        group.finish();
    }
}

fn filesystem(c: &mut Criterion) {
    let f = fixture(Workload::Replay, 100, 128);
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create_with_id(directory.path(), "rewrite").unwrap();
    c.bench_function("filesystem/full_rewrite", |b| {
        b.iter(|| writer.rewrite(black_box(&f.items)).unwrap())
    });
    let mut group = c.benchmark_group("filesystem");
    group.bench_function("write_only", |b| {
        b.iter_batched_ref(
            || tempfile::tempfile().unwrap(),
            |file| file.write_all(black_box(&f.jsonl)).unwrap(),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("staged_sync", |b| {
        b.iter_batched_ref(
            || {
                let mut staged = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
                staged.write_all(&f.jsonl).unwrap();
                staged
            },
            |staged| staged.as_file().sync_all().unwrap(),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("rename", |b| {
        b.iter_batched_ref(
            || {
                let mut staged = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
                staged.write_all(&f.jsonl).unwrap();
                staged.as_file().sync_all().unwrap();
                staged
            },
            |staged| std::fs::rename(staged.path(), directory.path().join("renamed")).unwrap(),
            BatchSize::PerIteration,
        )
    });
    #[cfg(unix)]
    group.bench_function("directory_sync", |b| {
        b.iter(|| {
            std::fs::File::open(directory.path())
                .unwrap()
                .sync_all()
                .unwrap()
        })
    });
    group.finish();
}

fn record_codecs(c: &mut Criterion) {
    let f = fixture(Workload::Mixed, 10, 128);
    let mut group = c.benchmark_group("records");
    for (index, item) in f.items.iter().enumerate() {
        let bytes = serde_json::to_vec(item).unwrap();
        group.bench_with_input(BenchmarkId::new("encode", index), item, |b, item| {
            b.iter(|| black_box(serde_json::to_vec(black_box(item)).unwrap()))
        });
        group.bench_with_input(BenchmarkId::new("decode", index), &bytes, |b, bytes| {
            b.iter(|| {
                black_box(serde_json::from_slice::<TranscriptItem>(black_box(bytes)).unwrap())
            })
        });
    }
    group.finish();
}

fn main() {
    for (name, bytes) in layout() {
        println!("layout {name}: {bytes}");
    }
    #[cfg(feature = "allocation-probe")]
    {
        allocations::run();
    }
    #[cfg(not(feature = "allocation-probe"))]
    {
        let mut c = Criterion::default()
            .sample_size(20)
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_millis(300))
            .configure_from_args();
        in_memory(&mut c);
        transactions(&mut c);
        prompt_admission(&mut c);
        prompt_pipeline(&mut c);
        filesystem(&mut c);
        record_codecs(&mut c);
        c.final_summary();
    }
}
