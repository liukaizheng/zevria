//! Desktop-only grouping and rendering of provider-hosted web work.

use super::{BlockData, BlockKind, BlockView, Gutter, style};
use crate::ui::theme::palette;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    link::Link,
    menu::DropdownMenu as _,
    tooltip::Tooltip,
};
use gpui_kit::{prelude::*, *};
use zevria_content::{CitedSource, citations};
use zevria_presentation::{
    blocks::{WebActionKind, WebActionPresentation, WebActivityPresentation, aggregate_web_status},
    status_icon::StatusIcon,
};

const INDENT: f32 = 28.;
const VISIBLE_DOMAINS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebRow {
    actions: Vec<WebActionPresentation>,
    copies: Vec<String>,
}

impl WebRow {
    pub(super) fn new(activity: &WebActivityPresentation, copy: String) -> Self {
        Self {
            actions: activity.actions.clone(),
            copies: vec![copy],
        }
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

/// Coalesce only consecutive visible blocks in this entry. Keep the first
/// block's address and revision; non-first changes are detected from the row's
/// joined body and the structured actions in BlockView::sync.
pub(super) fn coalesce(blocks: Vec<BlockData>) -> Vec<BlockData> {
    let mut rows: Vec<BlockData> = Vec::with_capacity(blocks.len());
    for mut data in blocks {
        if let Some(BlockData {
            kind: BlockKind::Web(prior),
            ..
        }) = rows.last_mut()
            && let BlockKind::Web(current) = &mut data.kind
        {
            prior.actions.append(&mut current.actions);
            prior.copies.append(&mut current.copies);
        } else {
            rows.push(data);
        }
    }
    for data in &mut rows {
        if let BlockKind::Web(row) = &data.kind
            && row.copies.len() > 1
        {
            data.body = row.copies.join("\n");
            data.primary.clone_from(&data.body);
            data.gutter = Gutter::Glyph(row.status());
        }
    }
    rows
}

fn label(kind: WebActionKind) -> &'static str {
    match kind {
        WebActionKind::Search => "Web search",
        WebActionKind::OpenPage => "Open page",
        WebActionKind::FindInPage => "Find in page",
        WebActionKind::Other => "Web action",
    }
}

pub(in crate::ui) fn live_phrase(actions: &[WebActionPresentation]) -> Option<&'static str> {
    let action = actions
        .last()
        .filter(|action| action.status == StatusIcon::Running)?;
    Some(match action.kind {
        WebActionKind::Search | WebActionKind::Other => "Searching the web",
        WebActionKind::OpenPage => "Opening a page",
        WebActionKind::FindInPage => "Finding in page",
    })
}

fn quoted(text: &str) -> String {
    format!("“{}”", text.replace(['\n', '\t'], " "))
}

fn extra_queries(action: &WebActionPresentation) -> Option<String> {
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

#[derive(Debug, PartialEq, Eq)]
struct SourceDomain {
    host: String,
    pages: Vec<CitedSource>,
}

fn domains(sources: &[CitedSource]) -> Vec<SourceDomain> {
    let mut domains: Vec<SourceDomain> = Vec::new();
    for source in sources {
        let Some(url) = citations::safe_http_url(&source.url) else {
            continue;
        };
        let Some(display) = citations::url_display(&url) else {
            continue;
        };
        let page = CitedSource {
            url,
            title: source.title.clone(),
        };
        if let Some(domain) = domains
            .iter_mut()
            .find(|domain| domain.host == display.host)
        {
            if !domain.pages.iter().any(|prior| prior.url == page.url) {
                domain.pages.push(page);
            }
        } else {
            domains.push(SourceDomain {
                host: display.host,
                pages: vec![page],
            });
        }
    }
    domains
}

fn summary_text(text: impl Into<SharedString>, ink: Hsla, expanded: bool) -> Div {
    style::SECONDARY
        .apply(div())
        .font_family(style::UI_FONT)
        .text_color(ink)
        .min_w_0()
        .when(!expanded, |text| text.truncate())
        .child(text.into())
}

fn address(action: &WebActionPresentation, expanded: bool, cx: &App) -> Option<Div> {
    let raw = action.url.as_ref()?;
    let muted = palette(cx).text.muted;
    let mut row = div()
        .flex()
        .items_start()
        .gap(px(4.))
        .min_w_0()
        .max_w_full();
    if let Some((url, display)) = action
        .safe_url()
        .and_then(|url| Some((url.to_owned(), citations::url_display(url)?)))
    {
        let host_bytes = display.host.len();
        let text = display.host + &display.rest;
        let len = text.len();
        let hover_url = url.clone();
        let open_tooltip = format!("Open page in browser\n{url}");
        row = row
            .child(
                div()
                    .id(format!("web-url-{}", action.output_index))
                    .test_support()
                    .min_w_0()
                    .tooltip(move |w, cx| Tooltip::new(hover_url.clone()).build(w, cx))
                    .child(
                        style::CODE
                            .apply(Link::new("page-address"))
                            .href(url.clone())
                            .font_family(style::MONO_FONT)
                            .min_w_0()
                            .when(!expanded, |link| link.truncate())
                            .child(StyledText::new(text).with_highlights([(
                                host_bytes..len,
                                HighlightStyle {
                                    color: Some(muted),
                                    ..Default::default()
                                },
                            )])),
                    ),
            )
            .child(
                div()
                    .id(format!("web-open-{}", action.output_index))
                    .test_support()
                    .flex_shrink_0()
                    .child(
                        style::icon_button("open", IconName::ExternalLink, "Open page in browser")
                            .text_color(muted)
                            .tooltip(open_tooltip)
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            );
    } else {
        row = row.child(
            style::mono()
                .min_w_0()
                .text_color(muted)
                .when(!expanded, |text| text.truncate())
                .child(raw.replace(['\n', '\t'], " ")),
        );
    }
    Some(row)
}

fn action_summary(action: &WebActionPresentation, expanded: bool, cx: &App) -> Div {
    let t = palette(cx);
    let mut summary = div().flex_1().min_w_0().flex().flex_col();
    let mut line = div().min_w_0().flex().items_start().gap(px(6.));
    if let Some(first) = action.queries.first() {
        line = line.child(summary_text(quoted(first), t.text.primary, expanded));
        if !expanded {
            line = line.when_some(extra_queries(action), |line, more| {
                line.child(
                    style::meta()
                        .flex_shrink_0()
                        .pt(px(3.))
                        .text_color(t.text.muted)
                        .child(more),
                )
            });
        }
    }
    if let Some(pattern) = &action.pattern {
        line = line.child(summary_text(quoted(pattern), t.text.primary, expanded));
    }
    let has_words = !action.queries.is_empty() || action.pattern.is_some();
    if let Some(address) = address(action, expanded, cx) {
        if expanded && has_words {
            summary = summary.child(line).child(address);
        } else {
            summary = summary.child(line.child(address));
        }
    } else if has_words {
        summary = summary.child(line);
    } else if let Some(phrase) = live_phrase(std::slice::from_ref(action)) {
        summary = summary.child(summary_text(
            format!("{phrase}…"),
            t.text.secondary,
            expanded,
        ));
    }
    if expanded {
        for query in action.queries.iter().skip(1) {
            summary = summary.child(summary_text(quoted(query), t.text.primary, true));
        }
    }
    summary
}

fn action_label(text: &'static str, cx: &App) -> Div {
    style::label()
        .text_color(palette(cx).text.secondary)
        .child(text)
}

fn status_words(statuses: impl IntoIterator<Item = StatusIcon>, cx: &App) -> Div {
    let (mut interrupted, mut unconfirmed) = (false, false);
    for status in statuses {
        interrupted |= status == StatusIcon::Interrupted;
        unconfirmed |= matches!(status, StatusIcon::Updated | StatusIcon::Unknown);
    }
    let t = palette(cx);
    div()
        .flex()
        .flex_wrap()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .when(interrupted, |row| {
            row.child(
                style::meta()
                    .id("web-interrupted")
                    .test_support()
                    .text_color(t.feedback.warning)
                    .child("interrupted"),
            )
        })
        .when(unconfirmed, |row| {
            row.child(
                style::meta()
                    .id("web-unconfirmed")
                    .test_support()
                    .text_color(t.text.muted)
                    .tooltip(|w, cx| {
                        Tooltip::new("The response ended before the provider confirmed this action")
                            .build(w, cx)
                    })
                    .child("unconfirmed"),
            )
        })
}

fn outcome_counts(actions: &[WebActionPresentation]) -> Vec<(StatusIcon, usize)> {
    use StatusIcon::*;
    [
        Done,
        Failed,
        Denied,
        Interrupted,
        Updated,
        Unknown,
        Dismissed,
    ]
    .into_iter()
    .map(|status| {
        (
            status,
            actions
                .iter()
                .filter(|action| action.status == status)
                .count(),
        )
    })
    .filter(|(_, count)| *count > 0)
    .collect()
}

fn count_meta(row: &WebRow, ink: Hsla, cx: &App) -> Option<impl IntoElement> {
    let counts = outcome_counts(&row.actions);
    if counts.len() < 2 {
        return None;
    }
    let mut meta = div()
        .id("web-count-meta")
        .test_support()
        .flex()
        .items_center()
        .gap(px(4.));
    for (i, (status, count)) in counts.into_iter().enumerate() {
        if i > 0 {
            meta = meta.child(style::meta().text_color(palette(cx).text.muted).child("·"));
        }
        meta = meta.child(
            style::meta()
                .whitespace_nowrap()
                .text_color(style::glyph_color(status, ink, cx))
                .child(format!("{count} {}", status.glyph(0))),
        );
    }
    Some(meta)
}

fn list_item(action: &WebActionPresentation, ink: Hsla, cx: &App) -> impl IntoElement {
    div()
        .id(format!("web-action-{}", action.output_index))
        .test_support()
        .flex()
        .items_start()
        .gap(px(6.))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .pt(px(2.))
                .child(style::status_glyph(action.status, ink, cx)),
        )
        .child(
            action_label(label(action.kind), cx)
                .w(px(76.))
                .flex_shrink_0()
                .pt(px(2.)),
        )
        .child(action_summary(action, true, cx))
        .child(status_words([action.status], cx).pt(px(3.)))
}

pub(super) fn render_row(
    row: &WebRow,
    view: &BlockView,
    ink: Hsla,
    window: &Window,
    cx: &mut Context<BlockView>,
) -> Div {
    let expanded = view.data.expanded;
    let header = div()
        .id("web-header")
        .test_support()
        .min_h(px(28.))
        .flex()
        .items_start()
        .children(view.block_disclosure(&format!("{} details", row.label()), window, cx));
    let label = action_label(row.label(), cx).flex_shrink_0().pt(px(2.));
    let contents = match row.single() {
        Some(action) => div()
            .flex()
            .items_start()
            .gap(px(8.))
            .child(label)
            .child(action_summary(action, expanded, cx))
            .child(status_words([action.status], cx).pt(px(3.))),
        None => div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(label.pt_0())
            .child(
                style::meta()
                    .id("web-counts")
                    .test_support()
                    .min_w_0()
                    .truncate()
                    .text_color(palette(cx).text.muted)
                    .child(row.counts()),
            )
            .child(div().flex_1())
            .children(count_meta(row, ink, cx))
            .child(status_words(
                row.actions.iter().map(|action| action.status),
                cx,
            )),
    };
    div()
        .group("log-block")
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            header
                .child(contents.flex_1().min_w_0().pt(px(4.)))
                .child(view.actions(window, cx).flex_shrink_0()),
        )
        .when(expanded && row.single().is_none(), |web| {
            web.child(
                div()
                    .id("web-action-list")
                    .test_support()
                    .pl(px(INDENT))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(row.actions.iter().map(|action| list_item(action, ink, cx))),
            )
        })
}

fn page_tooltip(page: &CitedSource) -> String {
    page.title.as_ref().map_or_else(
        || page.url.clone(),
        |title| format!("{title}\n{}", page.url),
    )
}

fn chip(domain: SourceDomain, cx: &App) -> impl IntoElement {
    let SourceDomain { host, pages } = domain;
    let count = pages.len();
    let label = if count == 1 {
        host.clone()
    } else {
        format!("{host} {count}")
    };
    let tooltip = page_tooltip(&pages[0]);
    let ink = cx.theme().link;
    let chip = style::LABEL
        .apply(Button::new(host.clone()))
        .label(label)
        .font_weight(FontWeight::MEDIUM)
        .xsmall()
        .ghost()
        .px(px(6.))
        .h(px(22.))
        .rounded(px(4.))
        .bg(palette(cx).surfaces.panel)
        .text_color(ink)
        .max_w_full();
    let control = if count == 1 {
        let url = pages
            .into_iter()
            .next()
            .expect("a cited domain is nonempty")
            .url;
        chip.accessibility_label(format!("Open source from {host}"))
            .tooltip(tooltip)
            .on_click(move |_, _, cx| cx.open_url(&url))
            .into_any_element()
    } else {
        chip.accessibility_label(format!("Show {count} sources from {host}"))
            .tooltip(format!("{count} pages · First cited\n{tooltip}"))
            .dropdown_menu(move |menu, _, _| {
                pages.iter().fold(
                    menu.max_w(px(480.)).max_h(px(320.)).scrollable(true),
                    |menu, page| {
                        menu.link(
                            page.title.as_deref().unwrap_or(&page.url).to_owned(),
                            page.url.clone(),
                        )
                    },
                )
            })
            .into_any_element()
    };
    div()
        .id(format!("source-domain-{host}"))
        .test_support()
        .min_w_0()
        .max_w_full()
        .child(control)
}

pub(super) fn sources_footer(
    view: &BlockView,
    cx: &mut Context<BlockView>,
) -> Option<impl IntoElement> {
    let domains = domains(&view.data.sources);
    let count = domains.len();
    if count == 0 {
        return None;
    }
    let shown = if view.show_all_sources {
        count
    } else {
        VISIBLE_DOMAINS
    };
    let footer = div()
        .id("answer-sources")
        .test_support()
        .pl(px(INDENT))
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(6.))
        .child(
            style::label()
                .text_color(palette(cx).text.muted)
                .child("Sources"),
        )
        .children(
            domains
                .into_iter()
                .take(shown)
                .map(|domain| chip(domain, cx)),
        )
        .when(count > VISIBLE_DOMAINS, |footer| {
            let label = if view.show_all_sources {
                "Show fewer sources".to_owned()
            } else {
                format!("Show all {count} sources")
            };
            footer.child(
                Button::new("show-all-sources")
                    .xsmall()
                    .ghost()
                    .label(label.clone())
                    .accessibility_label(label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_all_sources = !this.show_all_sources;
                        cx.notify();
                    })),
            )
        });
    Some(footer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use zevria_content::{
        HostedActivity, HostedActivityKind, HostedActivityStatus, ResponseAttempt,
    };
    use zevria_foundation::ModelProfileRef;

    fn row(actions: impl IntoIterator<Item = (Value, HostedActivityStatus)>) -> WebRow {
        let attempt = ResponseAttempt::new(ModelProfileRef::new("fixture", "fixture"));
        let actions = actions
            .into_iter()
            .enumerate()
            .map(|(index, (action, status))| HostedActivity {
                kind: HostedActivityKind::WebSearch,
                item_id: None,
                output_index: index as u64,
                status,
                action: Some(action),
            })
            .collect::<Vec<_>>();
        let refs = actions.iter().collect::<Vec<_>>();
        let activity = WebActivityPresentation::from_actions(&attempt, &refs);
        WebRow::new(&activity, activity.copy_text())
    }

    #[::core::prelude::v1::test]
    fn summaries_keep_query_pattern_and_address_distinct() {
        use HostedActivityStatus::*;
        let row_search = row([(
            json!({"type":"search","queries":["gpui text view links", "buttons", "hover"]}),
            Searching,
        )]);
        assert_eq!(row_search.label(), "Web search");
        assert_eq!(row_search.live_phrase(), Some("Searching the web"));
        assert_eq!(
            row_search.preview(),
            "Web search  “gpui text view links” +2 more"
        );
        let page = row([(
            json!({"type":"open_page","url":"https://WWW.Example.org/?page=1#topic"}),
            InProgress,
        )]);
        assert_eq!(page.preview(), "Open page  example.org?page=1#topic");
        assert_eq!(page.live_phrase(), Some("Opening a page"));
        let find = row([(
            json!({"type":"find_in_page", "pattern":"aria_label\nname", "url":"https://example.org/docs"}),
            InProgress,
        )]);
        assert_eq!(
            find.preview(),
            "Find in page  “aria_label name” example.org/docs"
        );
        assert_eq!(find.live_phrase(), Some("Finding in page"));
        let opening = row([(json!({"type":"open_page"}), InProgress)]);
        assert_eq!(opening.preview(), "Open page  Opening a page…");
        let unsafe_page = row([(
            json!({"type":"open_page","url":"https://example.org/\u{7}docs"}),
            Completed,
        )]);
        assert_eq!(unsafe_page.single().unwrap().safe_url(), None);
        assert_eq!(unsafe_page.preview(), "Open page  https://example.org/docs");
    }

    #[::core::prelude::v1::test]
    fn groups_pluralize_each_kind_and_status_uses_precedence() {
        use HostedActivityStatus::*;
        let row = row([
            (json!({"type":"search"}), InProgress),
            (json!({"type":"search"}), Completed),
            (json!({"type":"open_page"}), Failed),
            (json!({"type":"open_page"}), Interrupted),
            (json!({"type":"find_in_page"}), Completed),
            (json!({"type":"future"}), Completed),
        ]);
        assert!(row.single().is_none());
        assert_eq!(row.status(), StatusIcon::Running);
        assert_eq!(
            row.live_phrase(),
            None,
            "only the newest action speaks for the call"
        );
        assert_eq!(
            row.preview(),
            "Web activity  2 searches · 2 pages · 1 find · 1 action"
        );
        assert_eq!(
            outcome_counts(&row.actions),
            [
                (StatusIcon::Done, 3),
                (StatusIcon::Failed, 1),
                (StatusIcon::Interrupted, 1)
            ]
        );
        let statuses = [
            StatusIcon::Pending,
            StatusIcon::Denied,
            StatusIcon::Interrupted,
            StatusIcon::Unknown,
            StatusIcon::Done,
        ];
        for (index, expected) in statuses.iter().enumerate() {
            assert_eq!(
                aggregate_web_status(statuses[index..].iter().copied().rev()),
                *expected
            );
        }
    }

    #[::core::prelude::v1::test]
    fn cited_domains_and_pages_follow_first_citation_order() {
        let page = |url: &str, title: &str| CitedSource {
            url: url.into(),
            title: Some(title.into()),
        };
        let grouped = domains(&[
            page("https://WWW.Bücher.de/first", " First "),
            page("https://docs.rs/gpui", "GPUI"),
            page("https://xn--bcher-kva.de/second", "Second"),
            page("https://xn--bcher-kva.de/first", "Alternate URL"),
            page("https://xn--bcher-kva.de/first", "Ignored title"),
            page("https://name@example.org/", "Unsafe"),
        ]);
        assert_eq!(
            grouped
                .iter()
                .map(|domain| domain.host.as_str())
                .collect::<Vec<_>>(),
            ["xn--bcher-kva.de", "docs.rs"]
        );
        assert_eq!(
            grouped[0]
                .pages
                .iter()
                .map(|page| page.title.as_deref().unwrap())
                .collect::<Vec<_>>(),
            [" First ", "Second", "Alternate URL"]
        );
        assert_eq!(grouped[1].pages.len(), 1);
    }
}
