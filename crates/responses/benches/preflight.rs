//! Same-input comparison of the full capacity path and its append cache.
//! History/identity construction and cache priming are outside the timed work.
use criterion::{BatchSize, Criterion};
use rig_core::message::Message;
use serde_json::json;
use std::{hint::black_box, time::Duration};
use zevria_foundation::ModelProfileRef;
use zevria_model::{ModelInputIdentity, OwnedModelRequestItem, ProviderReplay};
use zevria_responses::{
    replay::{PreflightCache, preflight},
    tools::ToolRendererOptions,
};

fn history(count: usize, target: &ModelProfileRef) -> Vec<OwnedModelRequestItem> {
    let mut items = Vec::with_capacity(count);
    for index in 0..count / 2 {
        items.push(OwnedModelRequestItem::message(Message::user(
            "question ".repeat(16),
        )));
        items.push(OwnedModelRequestItem::replay_backed(ProviderReplay::openai_responses(
            target.clone(), vec![json!({"type":"message","id":format!("message_{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer ".repeat(16),"annotations":[]}]})],
        )).unwrap());
    }
    items
}

fn main() {
    let mut criterion = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(300))
        .configure_from_args();
    let target = ModelProfileRef::new("benchmark", "model");
    for count in [100, 1000] {
        let history = history(count, &target);
        let input = history
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        let identity = ModelInputIdentity::from_items(&input).unwrap();
        let prefix = &input[..input.len() - 2];
        let prefix_identity = ModelInputIdentity::from_items(prefix).unwrap();
        let mut group = criterion.benchmark_group(format!("responses_preflight/{count}"));
        group.bench_function("full", |bench| {
            bench.iter(|| black_box(preflight(black_box(&input), &target).unwrap()))
        });
        let mut cache = PreflightCache::default();
        cache
            .assess(
                &input,
                &identity,
                &target,
                true,
                ToolRendererOptions::default(),
                true,
            )
            .unwrap();
        group.bench_function("cached_unchanged", |bench| {
            bench.iter(|| {
                black_box(
                    cache
                        .assess(
                            black_box(&input),
                            &identity,
                            &target,
                            true,
                            ToolRendererOptions::default(),
                            true,
                        )
                        .unwrap(),
                )
            })
        });
        group.bench_function("append_pair", |bench| {
            bench.iter_batched_ref(
                || {
                    let mut cache = PreflightCache::default();
                    cache
                        .assess(
                            prefix,
                            &prefix_identity,
                            &target,
                            true,
                            ToolRendererOptions::default(),
                            true,
                        )
                        .unwrap();
                    cache
                },
                |cache| {
                    black_box(
                        cache
                            .assess(
                                &input,
                                &identity,
                                &target,
                                true,
                                ToolRendererOptions::default(),
                                true,
                            )
                            .unwrap(),
                    )
                },
                BatchSize::PerIteration,
            )
        });
        group.finish();
    }
    // Multi-MiB real raster occurrences: a text append must not revisit their
    // encoded payloads. Fixture construction and lineage hashing are not timed.
    let mut seed = 0xa18f23d9u32;
    let mut pixels = vec![0u8; 1024 * 512 * 4];
    for pixel in pixels.chunks_exact_mut(4) {
        for channel in &mut pixel[..3] {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *channel = seed as u8;
        }
        pixel[3] = 255;
    }
    let image = zevria_content::PromptImage::from_rgba(1024, 512, &pixels).unwrap();
    let prompt = zevria_content::UserPrompt::new(
        (0..3)
            .map(|_| zevria_content::PromptBlock::Image(image.clone()))
            .collect(),
    )
    .unwrap()
    .to_message();
    let history = [
        prompt,
        Message::assistant("screenshots inspected"),
        Message::user("text-only followup"),
    ];
    let input = history
        .iter()
        .map(zevria_model::ModelRequestItem::Message)
        .collect::<Vec<_>>();
    let identity = ModelInputIdentity::from_items(&input).unwrap();
    let prefix = &input[..2];
    let prefix_identity = ModelInputIdentity::from_items(prefix).unwrap();
    let mut group = criterion.benchmark_group("responses_preflight/three_large_images");
    group.bench_function("full", |bench| {
        bench.iter(|| black_box(preflight(black_box(&input), &target).unwrap()))
    });
    let mut cache = PreflightCache::default();
    let expected = cache
        .assess(&input, &identity, &target, true, Default::default(), true)
        .unwrap();
    assert_eq!(expected, preflight(&input, &target).unwrap());
    group.bench_function("cached_unchanged", |bench| {
        bench.iter(|| {
            black_box(
                cache
                    .assess(
                        black_box(&input),
                        &identity,
                        &target,
                        true,
                        Default::default(),
                        true,
                    )
                    .unwrap(),
            )
        })
    });
    group.bench_function("append_text", |bench| {
        bench.iter_batched_ref(
            || {
                let mut cache = PreflightCache::default();
                cache
                    .assess(
                        prefix,
                        &prefix_identity,
                        &target,
                        true,
                        Default::default(),
                        true,
                    )
                    .unwrap();
                cache
            },
            |cache| {
                black_box(
                    cache
                        .assess(&input, &identity, &target, true, Default::default(), true)
                        .unwrap(),
                );
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
    criterion.final_summary();
}
