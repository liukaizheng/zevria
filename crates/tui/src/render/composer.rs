use super::*;

pub(super) fn render_composer_surface(
    frame: &mut Frame,
    layout: &FrameLayout,
    parts: &mut crate::app::RenderParts<'_>,
    composer_layout: &ComposerLayout,
    cursor_owned: bool,
) {
    let (caption, mode_color) = match parts.mode {
        SessionMode::Build => ("Build", theme().workflow.build),
        SessionMode::Plan => ("Plan", theme().workflow.plan),
    };
    paint_surface(
        frame.buffer_mut(),
        layout.prompt,
        Style::new()
            .fg(theme().text.primary)
            .bg(theme().surfaces.panel),
    );
    let mut flags = composer_flags(parts.chrome, parts.retained_plan, parts.composer_locked);
    if parts.draft_recovery == crate::input::DraftRecoveryHint::Saved {
        if !flags.is_empty() {
            flags.push_str(" · ");
        }
        flags.push_str("Rejected draft saved");
    }
    if !parts.image_ranges.is_empty() {
        if !flags.is_empty() {
            flags.push_str(" · ");
        }
        flags.push_str(&format!("{} images", parts.image_ranges.len()));
    }
    if parts.paste_pending {
        if !flags.is_empty() {
            flags.push_str(" · ");
        }
        flags.push_str("pasting");
    }
    let multiline = if composer_layout.rows().len() > 1 {
        "multiline"
    } else {
        ""
    };
    paint_rounded_prompt(
        frame.buffer_mut(),
        layout.prompt,
        Style::new().fg(mode_color).bg(theme().surfaces.panel),
        caption,
        PromptInfo {
            left: &flags,
            right: multiline,
        },
    );

    let text_area = prompt_text_area(layout.prompt);
    let viewport_rows = usize::from(text_area.height);
    let total_rows = composer_layout.rows().len();
    let caret_row = composer_layout.caret().row;
    parts
        .view
        .reconcile_composer_viewport(total_rows, viewport_rows, caret_row);
    let composer_viewport = parts.view.composer_viewport().clone();
    let visible = composer_viewport.visible_range();
    let input_lines = composer_layout.rows()[visible.start()..visible.end()]
        .iter()
        .map(|row| {
            let mut spans = Vec::new();
            let mut offset = row.display_range().start;
            for range in &parts.image_ranges {
                let start = range.start.max(row.display_range().start);
                let end = range.end.min(row.display_range().end);
                if start >= end {
                    continue;
                }
                if offset < start {
                    spans.push(Span::raw(parts.input[offset..start].to_string()));
                }
                spans.push(Span::styled(
                    parts.input[start..end].to_string(),
                    Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
                ));
                offset = end;
            }
            if offset < row.display_range().end {
                spans.push(Span::raw(
                    parts.input[offset..row.display_range().end].to_string(),
                ));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    if text_area.height > 0 {
        let prefix_area = prompt_prefix_area(layout.prompt, text_area.y);
        frame.render_widget(
            Paragraph::new(Line::styled(PROMPT_PREFIX, Style::default().fg(mode_color))),
            prefix_area,
        );
        frame.render_widget(
            Paragraph::new(input_lines).style(Style::default().fg(if parts.composer_locked {
                theme().text.muted
            } else {
                theme().text.primary
            })),
            text_area,
        );
    }
    render_scrollbar(frame, layout.prompt, &composer_viewport);

    if cursor_owned
        && !parts.composer_locked
        && text_area.width > 0
        && viewport_rows > 0
        && caret_row >= visible.start()
        && caret_row < visible.end()
    {
        let caret_column = composer_layout
            .caret()
            .column
            .min(text_area.width.saturating_sub(1));
        let local_caret_row = rows_to_u16(caret_row.saturating_sub(composer_viewport.top()));
        frame.set_cursor_position((
            text_area.x.saturating_add(caret_column),
            text_area.y.saturating_add(local_caret_row),
        ));
    }
}

fn prompt_text_area(prompt: Rect) -> Rect {
    let left = CHROME_PAD_LEFT
        .saturating_add(PROMPT_PREFIX_WIDTH)
        .min(prompt.width);
    let right = crate::chrome::CHROME_PAD_RIGHT.min(prompt.width.saturating_sub(left));
    let top = u16::from(prompt.height > 0);
    let bottom = u16::from(prompt.height > 1);
    Rect {
        x: prompt.x.saturating_add(left),
        y: prompt.y.saturating_add(top),
        width: prompt.width.saturating_sub(left).saturating_sub(right),
        height: prompt.height.saturating_sub(top).saturating_sub(bottom),
    }
}

fn prompt_prefix_area(prompt: Rect, y: u16) -> Rect {
    let left = CHROME_PAD_LEFT.min(prompt.width);
    let right = crate::chrome::CHROME_PAD_RIGHT.min(prompt.width.saturating_sub(left));
    Rect {
        x: prompt.x.saturating_add(left),
        y,
        width: PROMPT_PREFIX_WIDTH.min(prompt.width.saturating_sub(left).saturating_sub(right)),
        height: u16::from(prompt.height > 2),
    }
}

fn composer_flags(chrome: ComposerChrome, retained_plan: bool, composer_locked: bool) -> String {
    let mut flags = Vec::new();
    if retained_plan {
        flags.push("plan retained");
    }
    match chrome {
        ComposerChrome::Recalling { .. } => flags.push("recalling"),
        ComposerChrome::Selecting { .. } => flags.push("selecting"),
        ComposerChrome::Busy | ComposerChrome::WorkerBusy => flags.push("busy"),
        ComposerChrome::ModePending => flags.push("selecting mode"),
        ComposerChrome::PersistenceDegraded => flags.push("persistence degraded"),
        ComposerChrome::FreshPlanRetry => flags.push("plan retry"),
        ComposerChrome::Command | ComposerChrome::Worker { command: true, .. } => {
            flags.push("command")
        }
        ComposerChrome::Inspect
        | ComposerChrome::Idle { .. }
        | ComposerChrome::Worker { command: false, .. } => {}
    }
    if composer_locked {
        flags.push("locked");
    }
    flags.join(" · ")
}
