//! Pure projection of a group of adjacent hosted web actions.
use zevria_content::citations;
use zevria_presentation::{
    blocks::{WebActionKind, WebActionPresentation, WebActivityPresentation, aggregate_web_status},
    status_icon::StatusIcon,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebRow {
    pub actions: Vec<WebActionPresentation>,
    copies: Vec<String>,
}

impl WebRow {
    pub fn new(activity: &WebActivityPresentation, copy: String) -> Self {
        Self {
            actions: activity.actions.clone(),
            copies: vec![copy],
        }
    }

    pub fn merge(&mut self, next: &Self) {
        self.actions.extend_from_slice(&next.actions);
        self.copies.extend_from_slice(&next.copies);
    }

    pub fn copy_text(&self) -> String {
        self.copies.join("\n")
    }

    pub fn target(&self) -> String {
        self.single().map_or_else(
            || self.counts(),
            |action| summary_preview(action, self.live_phrase()),
        )
    }

    pub fn status(&self) -> StatusIcon {
        aggregate_web_status(self.actions.iter().map(|action| action.status))
    }

    pub fn single(&self) -> Option<&WebActionPresentation> {
        match self.actions.as_slice() {
            [action] => Some(action),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        self.single()
            .map_or("Web activity", |action| label(action.kind))
    }

    pub fn counts(&self) -> String {
        let mut counts = [0; 4];
        for action in &self.actions {
            counts[match action.kind {
                WebActionKind::Search => 0,
                WebActionKind::OpenPage => 1,
                WebActionKind::FindInPage => 2,
                WebActionKind::Other => 3,
            }] += 1;
        }
        counts
            .into_iter()
            .zip(["search", "page", "find", "action"])
            .filter(|(count, _)| *count > 0)
            .map(|(count, noun)| {
                let suffix = match (count, noun) {
                    (1, _) => "",
                    (_, "search") => "es",
                    _ => "s",
                };
                format!("{count} {noun}{suffix}")
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn live_phrase(&self) -> Option<&'static str> {
        live_phrase(&self.actions)
    }

    pub fn preview(&self) -> String {
        let summary = self.single().map_or_else(
            || self.counts(),
            |action| summary_preview(action, self.live_phrase()),
        );
        if summary.is_empty() {
            self.label().into()
        } else {
            format!("{}  {summary}", self.label())
        }
    }
}

pub(crate) fn label(kind: WebActionKind) -> &'static str {
    match kind {
        WebActionKind::Search => "Web search",
        WebActionKind::OpenPage => "Open page",
        WebActionKind::FindInPage => "Find in page",
        WebActionKind::Other => "Web action",
    }
}

pub(crate) fn live_phrase(actions: &[WebActionPresentation]) -> Option<&'static str> {
    let action = actions
        .last()
        .filter(|action| action.status == StatusIcon::Running)?;
    Some(match action.kind {
        WebActionKind::Search | WebActionKind::Other => "Searching the web",
        WebActionKind::OpenPage => "Opening a page",
        WebActionKind::FindInPage => "Finding in page",
    })
}

pub(crate) fn quoted(text: &str) -> String {
    format!("“{}”", text.replace(['\n', '\t'], " "))
}

pub(crate) fn extra_queries(action: &WebActionPresentation) -> Option<String> {
    (action.queries.len() > 1).then(|| format!("+{} more", action.queries.len() - 1))
}

fn summary_preview(action: &WebActionPresentation, live: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(query) = action.queries.first() {
        parts.push(quoted(query));
        parts.extend(extra_queries(action));
    }
    if let Some(pattern) = &action.pattern {
        parts.push(quoted(pattern));
    }
    if let Some(url) = &action.url {
        parts.push(
            action
                .safe_url()
                .and_then(citations::url_display)
                .map_or_else(
                    || url.replace(['\n', '\t'], " "),
                    |display| display.host + &display.rest,
                ),
        );
    }
    if parts.is_empty() {
        return live.map_or_else(String::new, |phrase| format!("{phrase}…"));
    }
    parts.join(" ")
}
