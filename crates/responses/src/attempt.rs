//! Adapter-owned hosted activity and provisional display text. Runs on the raw
//! event before Rig's deliberately narrower streaming projection.

use rig_core::message::Text;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use zevria_content::HostedActivityStatus;
use zevria_content::ResponseAttempt;
use zevria_content::citations;
use zevria_content::{CitedSource, HostedActivity};

#[cfg(test)]
#[path = "attempt_tests.rs"]
mod tests;

#[derive(Default)]
struct LiveText {
    raw: String,
    // Maintained at writes so a plain delta never scans the existing prefix.
    needs_render: bool,
    refusal: bool,
    annotations: BTreeMap<u64, Value>,
    annotation_sequences: HashMap<u64, u64>,
    // Text authority: added/delta (0), text-done (1), part-done (2),
    // item-done (3), terminal output (4). Annotation finality is independent.
    text_authority: u8,
    annotation_authority: u8,
    sequence: Option<u64>,
}

pub struct AttemptReducer {
    attempt: ResponseAttempt,
    // Only display-visible mutations advance the attempt revision. Authority
    // fences and raw text/annotation bookkeeping are not themselves changes.
    changed: bool,
    live_text: BTreeMap<(u64, u64), LiveText>,
    display_text: BTreeMap<(u64, u64), Text>,
    display_sources: BTreeMap<(u64, u64), Vec<CitedSource>>,
    dirty_text: std::collections::BTreeSet<(u64, u64)>,
    answers_dirty: bool,
    // Zero is a growing message; completed items also fence unknown late parts.
    text_outputs: BTreeMap<u64, u8>,
    authority: HashMap<u64, u8>,
    finalized_parts: HashMap<(u64, zevria_content::AssistantPartIdentity), u8>,
    sequences: std::collections::HashSet<u64>,
    item_ids: HashMap<u64, (String, u8)>,
}
impl AttemptReducer {
    pub fn attempt(&self) -> &ResponseAttempt {
        &self.attempt
    }
    pub fn attempt_mut(&mut self) -> &mut ResponseAttempt {
        self.materialize();
        &mut self.attempt
    }

    pub fn new(profile: zevria_foundation::ModelProfileRef) -> Self {
        let attempt = ResponseAttempt::new(profile);
        Self {
            attempt,
            changed: false,
            text_outputs: BTreeMap::new(),
            authority: HashMap::new(),
            finalized_parts: Default::default(),
            sequences: Default::default(),
            item_ids: Default::default(),
            live_text: Default::default(),
            display_text: Default::default(),
            display_sources: Default::default(),
            dirty_text: Default::default(),
            answers_dirty: false,
        }
    }

    fn activity(
        &mut self,
        index: u64,
        id: Option<&str>,
        status: HostedActivityStatus,
        action: Option<&Value>,
        authority: u8,
    ) {
        let position = self.attempt.hosted_activity.iter().position(|activity| {
            activity.output_index == index || (id.is_some() && activity.item_id.as_deref() == id)
        });
        let activity = match position {
            Some(position) => &mut self.attempt.hosted_activity[position],
            None => {
                self.changed = true;
                self.attempt.hosted_activity.push(HostedActivity {
                    kind: zevria_content::HostedActivityKind::WebSearch,
                    item_id: id.map(str::to_string),
                    output_index: index,
                    status,
                    action: action.cloned(),
                });
                self.attempt
                    .hosted_activity
                    .last_mut()
                    .expect("just inserted")
            }
        };
        let prior = self.authority.entry(activity.output_index).or_default();
        if authority < *prior {
            return;
        }
        if activity.item_id.is_none() && id.is_some() {
            activity.item_id = id.map(str::to_string);
            self.changed = true;
        }
        if status.is_terminal() || !activity.status.is_terminal() {
            // Out-of-order in_progress must not regress searching/completed.
            if (authority > *prior
                || status != HostedActivityStatus::InProgress
                || activity.status == HostedActivityStatus::InProgress)
                && activity.status != status
            {
                activity.status = status;
                self.changed = true;
            }
        }
        if let Some(action) = action {
            if authority > *prior || activity.action.is_none() {
                if activity.action.as_ref() != Some(action) {
                    activity.action = Some(action.clone());
                    self.changed = true;
                }
            } else if let (Some(Value::Object(existing)), Value::Object(incoming)) =
                (&mut activity.action, action)
            {
                for (key, value) in incoming {
                    if existing.get(key) != Some(value) {
                        existing.insert(key.clone(), value.clone());
                        self.changed = true;
                    }
                }
            }
        }
        *prior = authority;
    }

    /// Sanitized, citation-rendered display copies, never canonical model input.
    /// Output addresses establish order without waiting for item completion.
    pub fn visible_text(&self) -> Vec<((u64, u64), &Text)> {
        let mut visible = Vec::new();
        for &output in self.text_outputs.keys() {
            let mut content = 0;
            while let Some(text) = self.display_text.get(&(output, content)) {
                if !text.text.trim().is_empty() {
                    visible.push(((output, content), text));
                }
                content += 1;
            }
        }
        visible
    }

    fn part(&mut self, output: u64, content: u64, part: &Value, authority: u8) {
        let kind = part.get("type").and_then(Value::as_str);
        if kind == Some("summary_text") {
            if let Some(text) = zevria_content::readable::readable_reasoning_value(part) {
                self.reasoning(
                    output,
                    zevria_content::AssistantPartIdentity::Summary(content),
                    text,
                    false,
                    authority,
                );
            }
            return;
        }
        let field = match kind {
            Some("output_text") => "text",
            Some("refusal") => "refusal",
            _ => return,
        };
        let Some(raw) = part.get(field).and_then(Value::as_str) else {
            return;
        };
        let key = (output, content);
        if self
            .text_outputs
            .get(&output)
            .is_some_and(|prior| *prior > authority)
        {
            return;
        }
        self.text_outputs.entry(output).or_default();
        let live = self.live_text.entry(key).or_default();
        if live.text_authority > authority || (authority == 0 && !live.raw.is_empty()) {
            return;
        }
        live.raw = raw.into();
        live.needs_render = needs_display_render(raw);
        live.refusal = kind == Some("refusal");
        live.text_authority = authority;
        if let Some(annotations) = part.get("annotations").and_then(Value::as_array) {
            if authority > 0 {
                live.annotations.clear();
            }
            for (index, annotation) in annotations.iter().enumerate() {
                live.annotations
                    .entry(index as u64)
                    .or_insert_with(|| annotation.clone());
            }
            live.annotation_authority = authority;
        } else if kind == Some("refusal") || authority >= 3 {
            live.annotations.clear();
            live.annotation_authority = authority;
        }
        self.dirty_text.insert(key);
        self.answers_dirty = true;
    }

    fn text(
        &mut self,
        key: (u64, u64),
        text: &str,
        done: bool,
        refusal: bool,
        sequence: Option<u64>,
    ) {
        use zevria_content::{
            AssistantPartIdentity::Content, AssistantPresentationContent::Answer,
        };
        let (output, content) = key;
        if self
            .text_outputs
            .get(&output)
            .is_some_and(|authority| *authority > 0)
        {
            return;
        }
        self.text_outputs.entry(output).or_default();
        let live = self.live_text.entry(key).or_default();
        if live.text_authority > u8::from(done)
            || (!done && live.text_authority > 0)
            || (live.text_authority == u8::from(done)
                && sequence
                    .zip(live.sequence)
                    .is_some_and(|(incoming, prior)| incoming < prior))
        {
            return;
        }
        live.refusal = refusal;
        live.sequence = sequence.or(live.sequence);
        if done {
            live.raw = text.into();
            live.needs_render = needs_display_render(text);
            live.text_authority = 1;
        } else {
            live.raw.push_str(text);
            live.needs_render |= needs_display_render(text);
            if !live.needs_render
                && live.annotations.is_empty()
                && !self.dirty_text.contains(&key)
                && let Some(part) = self.attempt.presentation.iter_mut().find(|part| {
                    part.source.output_index == output && part.source.part == Content(content)
                })
                && let Answer { text: answer, .. } = &mut part.content
                && let Some(display) = self.display_text.get_mut(&key)
            {
                answer.push_str(text);
                display.text.push_str(text);
                self.changed |= !text.is_empty();
                return;
            }
        }
        self.dirty_text.insert(key);
        self.answers_dirty = true;
    }

    fn annotation(
        &mut self,
        output: u64,
        content: u64,
        index: u64,
        annotation: &Value,
        sequence: Option<u64>,
    ) {
        if self
            .text_outputs
            .get(&output)
            .is_some_and(|authority| *authority > 0)
        {
            return;
        }
        let key = (output, content);
        let live = self.live_text.entry(key).or_default();
        if live.annotation_authority > 0
            || sequence
                .zip(live.annotation_sequences.get(&index).copied())
                .is_some_and(|(incoming, prior)| incoming < prior)
        {
            return;
        }
        if let Some(sequence) = sequence {
            live.annotation_sequences.insert(index, sequence);
        }
        if live.annotations.get(&index) != Some(annotation) {
            live.annotations.insert(index, annotation.clone());
            self.dirty_text.insert(key);
            self.answers_dirty = true;
        }
    }

    fn remember_item_id(&mut self, index: u64, id: &str, authority: u8) {
        if self
            .item_ids
            .get(&index)
            .is_none_or(|(_, prior)| authority > *prior)
        {
            self.answers_dirty |= self.text_outputs.contains_key(&index)
                && self
                    .item_ids
                    .get(&index)
                    .is_none_or(|(prior_id, _)| prior_id != id);
            self.item_ids.insert(index, (id.into(), authority));
        }
    }

    fn item(&mut self, index: u64, item: &Value, authority: u8) {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            self.remember_item_id(index, id, if authority > 0 { authority + 2 } else { 0 });
        }
        match item.get("type").and_then(Value::as_str) {
            Some("web_search_call") => {
                let status = HostedActivityStatus::from_native_item_status(
                    item.get("status").and_then(Value::as_str),
                );
                self.activity(
                    index,
                    item.get("id").and_then(Value::as_str),
                    status,
                    item.get("action"),
                    authority,
                );
            }
            Some("reasoning") => {
                if self
                    .finalized_parts
                    .iter()
                    .any(|((output, _), prior)| *output == index && *prior > authority + 1)
                {
                    return;
                }
                let readable = [("summary", true), ("content", false)]
                    .into_iter()
                    .flat_map(|(field, summary)| {
                        item.get(field)
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .enumerate()
                            .filter_map(move |(part_index, part)| {
                                let text =
                                    zevria_content::readable::readable_reasoning_value(part)?;
                                let identity = if summary {
                                    zevria_content::AssistantPartIdentity::Summary(
                                        part_index as u64,
                                    )
                                } else {
                                    zevria_content::AssistantPartIdentity::Content(
                                        part_index as u64,
                                    )
                                };
                                Some((identity, zevria_content::readable::sanitize_readable(text)))
                            })
                    });
                if authority > 0 {
                    // Reconcile the complete set using the same sanitized text
                    // that will be installed below.
                    let readable = readable.collect::<Vec<_>>();
                    let keep = readable
                        .iter()
                        .filter_map(|(identity, text)| {
                            (!text.trim().is_empty()).then_some(*identity)
                        })
                        .collect::<std::collections::HashSet<_>>();
                    let before = self.attempt.presentation.len();
                    self.attempt.presentation.retain(|part| {
                        part.source.output_index != index || keep.contains(&part.source.part)
                    });
                    self.changed |= before != self.attempt.presentation.len();
                    for (identity, text) in readable {
                        self.readable_reasoning(index, identity, text, false, authority + 1);
                    }
                } else {
                    for (identity, text) in readable {
                        self.readable_reasoning(index, identity, text, false, 0);
                    }
                }
            }
            Some("function_call") => {
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    self.set_part(
                        index,
                        zevria_content::AssistantPartIdentity::Tool,
                        zevria_content::AssistantPresentationContent::NativeTool {
                            call_id: call_id.into(),
                        },
                    );
                }
            }
            Some("message") => {
                let authority = if authority > 0 { authority + 2 } else { 0 };
                if self
                    .text_outputs
                    .get(&index)
                    .is_some_and(|prior| *prior > authority)
                {
                    return;
                }
                self.text_outputs.entry(index).or_default();
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    if authority > 0 {
                        // A completed item can remove provisional trailing parts.
                        let keep = |key: &(u64, u64)| key.0 != index || key.1 < parts.len() as u64;
                        self.live_text.retain(|key, _| keep(key));
                        self.display_text.retain(|key, _| keep(key));
                        self.display_sources.retain(|key, _| keep(key));
                        self.dirty_text.retain(keep);
                        self.answers_dirty = true;
                    }
                    for (content, part) in parts.iter().enumerate() {
                        self.part(index, content as u64, part, authority);
                    }
                }
                self.text_outputs.insert(index, authority);
            }
            _ => {}
        }
    }

    fn set_part(
        &mut self,
        output: u64,
        identity: zevria_content::AssistantPartIdentity,
        content: zevria_content::AssistantPresentationContent,
    ) {
        let part = zevria_content::AssistantPresentationPart {
            source: zevria_content::AssistantSourceAddress {
                output_index: output,
                part: identity,
                item_id: self.item_ids.get(&output).map(|(id, _)| id.clone()),
            },
            content,
        };
        if let Some(existing) = self
            .attempt
            .presentation
            .iter_mut()
            .find(|part| part.source.output_index == output && part.source.part == identity)
        {
            if *existing != part {
                *existing = part;
                self.changed = true;
            }
        } else {
            self.attempt.presentation.push(part);
            self.changed = true;
        }
    }

    fn reasoning(
        &mut self,
        output: u64,
        identity: zevria_content::AssistantPartIdentity,
        text: &str,
        append: bool,
        authority: u8,
    ) {
        self.readable_reasoning(
            output,
            identity,
            zevria_content::readable::sanitize_readable(text),
            append,
            authority,
        );
    }

    fn readable_reasoning(
        &mut self,
        output: u64,
        identity: zevria_content::AssistantPartIdentity,
        text: String,
        append: bool,
        authority: u8,
    ) {
        use zevria_content::AssistantPresentationContent::Reasoning;
        if self
            .finalized_parts
            .get(&(output, identity))
            .is_some_and(|prior| *prior > authority || (append && *prior > 0))
        {
            return;
        }
        if !append
            && authority == 0
            && self
                .attempt
                .presentation
                .iter()
                .any(|part| part.source.output_index == output && part.source.part == identity)
        {
            return;
        }
        if append
            && let Some(part) = self
                .attempt
                .presentation
                .iter_mut()
                .find(|part| part.source.output_index == output && part.source.part == identity)
            && let Reasoning { text: existing } = &mut part.content
        {
            // Existing reasoning is already readable, so whitespace deltas
            // remain meaningful separators. Append without copying the prefix.
            if !text.is_empty() {
                existing.push_str(&text);
                self.changed = true;
            }
            let item_id = self.item_ids.get(&output).map(|(id, _)| id);
            if part.source.item_id.as_ref() != item_id {
                part.source.item_id = item_id.cloned();
                self.changed = true;
            }
            if authority > 0 {
                self.finalized_parts.insert((output, identity), authority);
            }
            return;
        }
        // Whitespace in a live delta can separate words. Keep it only inside
        // an already readable part, never as an empty selectable block.
        if !text.trim().is_empty() {
            self.set_part(output, identity, Reasoning { text });
        }
        if authority > 0 {
            self.finalized_parts.insert((output, identity), authority);
        }
    }

    fn refresh_answers(&mut self) {
        use zevria_content::{
            AssistantPartIdentity::Content, AssistantPresentationContent::Answer,
        };
        // Render only changed parts, always from original offsets, never from
        // a previously linked/sanitized display string.
        for key in std::mem::take(&mut self.dirty_text) {
            let live = &self.live_text[&key];
            let annotations = Value::Array(live.annotations.values().cloned().collect());
            let text = if live.annotation_authority > 0 {
                citations::render(&live.raw, Some(&annotations))
            } else {
                citations::render_preview(&live.raw, Some(&annotations), live.text_authority > 0)
            };
            let sources = if live.refusal {
                Vec::new()
            } else {
                citations::sources(Some(&annotations))
            };
            self.display_sources.insert(key, sources);
            self.display_text.insert(
                key,
                Text::new(zevria_content::readable::sanitize_readable(&text)),
            );
        }
        let visible = self.visible_text();
        let keys = visible
            .iter()
            .filter(|(_, text)| !text.text.trim().is_empty())
            .map(|(key, _)| *key)
            .collect::<std::collections::HashSet<_>>();
        let answers = visible
            .into_iter()
            .filter(|(_, text)| !text.text.trim().is_empty())
            .filter_map(|((output, content), text)| {
                let sources = &self.display_sources[&(output, content)];
                let unchanged = self.attempt.presentation.iter().any(|part| {
                    part.source.output_index == output
                        && part.source.part == Content(content)
                        && part.source.item_id.as_ref()
                            == self.item_ids.get(&output).map(|(id, _)| id)
                        && matches!(&part.content, Answer { text: prior, sources: prior_sources }
                            if prior == &text.text && prior_sources == sources)
                });
                (!unchanged).then(|| (output, content, text.text.clone(), sources.clone()))
            })
            .collect::<Vec<_>>();
        let before = self.attempt.presentation.len();
        self.attempt.presentation.retain(|part| !matches!(&part.content, Answer { .. })
            || matches!(part.source.part, Content(content) if keys.contains(&(part.source.output_index, content))));
        self.changed |= before != self.attempt.presentation.len();
        for (output, content, text, sources) in answers {
            self.set_part(output, Content(content), Answer { text, sources });
        }
    }

    /// Reconcile display-only work at publication, close and cancellation.
    pub fn materialize(&mut self) {
        self.changed = false;
        if std::mem::take(&mut self.answers_dirty) {
            self.refresh_answers();
        }
        let key = |part: &zevria_content::AssistantPresentationPart| {
            (part.source.output_index, part.source.part)
        };
        if self
            .attempt
            .presentation
            .windows(2)
            .any(|parts| key(&parts[0]) > key(&parts[1]))
        {
            self.attempt.presentation.sort_by_key(key);
            self.changed = true;
        }
        if self.changed {
            self.attempt.touch();
        }
    }

    /// Ingest all readable evidence synchronously, before any cancellable await.
    pub fn ingest(&mut self, event: &Value) {
        if let Some(sequence) = event.get("sequence_number").and_then(Value::as_u64)
            && !self.sequences.insert(sequence)
        {
            return;
        }
        self.changed = false;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sequence = event.get("sequence_number").and_then(Value::as_u64);
        let output = event.get("output_index").and_then(Value::as_u64);
        let content = event.get("content_index").and_then(Value::as_u64);
        if let (Some(output), Some(id)) = (output, event.get("item_id").and_then(Value::as_str)) {
            let authority = if kind.ends_with("part.done") {
                2
            } else {
                u8::from(kind.ends_with(".done"))
            };
            self.remember_item_id(output, id, authority);
        }
        match kind {
            "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => {
                let summary = kind.contains("summary");
                let index = event
                    .get(if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    })
                    .and_then(Value::as_u64)
                    .or_else(|| (!summary).then_some(0));
                let done = kind.ends_with(".done");
                if let (Some(output), Some(index), Some(text)) = (
                    output,
                    index,
                    event
                        .get(if done { "text" } else { "delta" })
                        .and_then(Value::as_str),
                ) {
                    let identity = if summary {
                        zevria_content::AssistantPartIdentity::Summary(index)
                    } else {
                        zevria_content::AssistantPartIdentity::Content(index)
                    };
                    self.reasoning(output, identity, text, !done, u8::from(done));
                }
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                if let (Some(output), Some(index), Some(text)) = (
                    output,
                    event.get("summary_index").and_then(Value::as_u64),
                    event
                        .get("part")
                        .and_then(zevria_content::readable::readable_reasoning_value),
                ) {
                    self.reasoning(
                        output,
                        zevria_content::AssistantPartIdentity::Summary(index),
                        text,
                        false,
                        u8::from(kind.ends_with(".done")),
                    );
                }
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {
                if let Some(output) = output {
                    let status = match kind {
                        "response.web_search_call.completed" => HostedActivityStatus::Completed,
                        "response.web_search_call.searching" => HostedActivityStatus::Searching,
                        _ => HostedActivityStatus::InProgress,
                    };
                    self.activity(
                        output,
                        event.get("item_id").and_then(Value::as_str),
                        status,
                        event.get("action"),
                        0,
                    );
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let (Some(output), Some(item)) = (output, event.get("item")) {
                    self.item(output, item, u8::from(kind.ends_with(".done")));
                }
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => {
                let done = kind.ends_with(".done");
                let refusal = kind.starts_with("response.refusal.");
                let field = if !done {
                    "delta"
                } else if refusal {
                    "refusal"
                } else {
                    "text"
                };
                if let (Some(output), Some(content), Some(text)) =
                    (output, content, event.get(field).and_then(Value::as_str))
                {
                    self.text((output, content), text, done, refusal, sequence);
                }
            }
            "response.output_text.annotation.added" => {
                if let (Some(output), Some(content), Some(index), Some(annotation)) = (
                    output,
                    content,
                    event.get("annotation_index").and_then(Value::as_u64),
                    event.get("annotation"),
                ) {
                    self.annotation(output, content, index, annotation, sequence);
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                if let (Some(output), Some(content), Some(part)) =
                    (output, content, event.get("part"))
                {
                    self.part(
                        output,
                        content,
                        part,
                        if kind.ends_with(".done") { 2 } else { 0 },
                    );
                }
            }
            _ => {}
        }
        if let Some(response) = event.get("response") {
            if let Some(id) = response.get("id").and_then(Value::as_str)
                && self.attempt.response_id.as_deref() != Some(id)
            {
                self.attempt.response_id = Some(id.into());
                self.changed = true;
            }
            if matches!(
                kind,
                "response.completed" | "response.done" | "response.failed" | "response.incomplete"
            ) && let Some(items) = response.get("output").and_then(Value::as_array)
            {
                for (index, item) in items.iter().enumerate() {
                    self.item(index as u64, item, 2);
                }
                // Success is decided only after native validation and core commit.
                // A terminal event with an unusable ledger is still a failed attempt.
            }
        }
        if self.changed {
            self.attempt.touch();
        }
    }
}

fn needs_display_render(text: &str) -> bool {
    text.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\t'))
            || matches!(c, '\u{e200}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
    })
}
