use super::*;

pub(super) fn render_plan_surface(
    frame: &mut Frame,
    layout: &FrameLayout,
    parts: &mut crate::app::RenderParts<'_>,
    dialog: crate::app::PlanDialogView<'_>,
) {
    let recovering = dialog.state.recovering();
    let choice = dialog.state.choice();
    let revise_label = if recovering {
        "3. Continue revising the plan"
    } else {
        "3. Revise the plan"
    };
    let title = format!(
        " {}{} ",
        dialog.artifact.version,
        if parts.chrome == ComposerChrome::ModePending {
            " · selecting mode · input locked"
        } else if dialog.decision_pending {
            " · decision pending"
        } else {
            ""
        }
    );
    let choices = [
        (PlanChoice::Implement, "1. Implement the plan"),
        (
            PlanChoice::ImplementFresh,
            "2. Clear context, then implement the plan",
        ),
        (PlanChoice::Revise, revise_label),
    ];
    let selected_index = match choice {
        PlanChoice::Implement => 0,
        PlanChoice::ImplementFresh => 1,
        PlanChoice::Revise => 2,
    };
    let mut eligibility = crate::hints::Eligibility::default();
    if dialog.decision_pending {
        eligibility.disabled.push(crate::input::Action::Confirm);
        eligibility.labels.extend([
            (crate::input::Action::Close, "hide"),
            (crate::input::Action::Cancel, "hide"),
        ]);
    }
    let hints = crate::hints::hint_line(
        crate::input::KeyContext::PlanDecision,
        &eligibility,
        usize::from(layout.prompt.width.saturating_sub(2)),
    );
    let inner_area = zevria_tui_widgets::overlay::modal(frame, layout.prompt, title, hints);
    let viewport = {
        let viewport = parts.view.plan_viewport_mut();
        viewport.reconcile(choices.len(), usize::from(inner_area.height));
        viewport.reveal(RowRange::from_start_len(selected_index, 1));
        viewport.clone()
    };
    let visible = viewport.visible_range();
    let rows = choices[visible.start()..visible.end()]
        .iter()
        .map(|(row_choice, label)| {
            let mut row = Line::raw(truncate_display_width(label, usize::from(inner_area.width)));
            if *row_choice == choice {
                style_selected_line(&mut row);
            }
            row
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows), inner_area);
    if selected_index >= visible.start() && selected_index < visible.end() {
        let y = inner_area
            .y
            .saturating_add(rows_to_u16(selected_index - visible.start()));
        paint_selection(
            frame.buffer_mut(),
            inner_area,
            ScreenRows::new(y, y.saturating_add(1)),
            selection_style(),
        );
    }
    render_scrollbar(frame, layout.prompt, &viewport);
}
