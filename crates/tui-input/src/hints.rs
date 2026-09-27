//! Priority-based, whole-binding hints; the catalogue also powers full help.
use crate::{
    action::Action,
    keymap::{Binding, KeyContext, bindings},
};
use ratatui::text::{Line, Span};
use zevria_tui_widgets::{
    chrome::{hint_description_style, hint_key_style},
    text::display_width,
};

#[derive(Clone, Debug, Default)]
pub struct Eligibility {
    pub disabled: Vec<Action>,
    pub labels: Vec<(Action, &'static str)>,
}
impl Eligibility {
    pub fn allows(&self, action: Action) -> bool {
        !self.disabled.contains(&action)
    }
    pub fn label(&self, binding: &Binding) -> &'static str {
        self.labels
            .iter()
            .find(|(action, _)| *action == binding.action)
            .map_or(binding.label, |(_, label)| *label)
    }
}

pub fn hint_line(context: KeyContext, eligibility: &Eligibility, width: usize) -> Line<'static> {
    let mut candidates: Vec<_> = bindings(context)
        .filter(|b| b.primary > 0 && b.action != Action::Help && eligibility.allows(b.action))
        .collect();
    candidates.sort_by_key(|binding| binding.primary);
    let binding_width = |binding: &Binding| {
        display_width(&binding.hint_key_label()) + 1 + display_width(eligibility.label(binding))
    };
    let help = bindings(context).find(|binding| {
        binding.action == Action::Help
            && eligibility.allows(binding.action)
            && binding_width(binding) <= width
    });
    let mut used = help.map_or(0, binding_width);
    let mut chosen: Vec<&Binding> = Vec::new();
    for binding in candidates {
        if chosen.iter().any(|b| b.action == binding.action) {
            continue;
        }
        if chosen.len() >= if help.is_some() { 4 } else { 5 } {
            break;
        }
        let size = display_width(&binding.hint_key_label())
            + 1
            + display_width(eligibility.label(binding));
        let extra = size + if used > 0 { 3 } else { 0 };
        if used + extra <= width {
            chosen.push(binding);
            used += extra;
        }
    }
    let mut spans = Vec::new();
    for binding in chosen {
        append_hint(
            &mut spans,
            binding.hint_key_label(),
            eligibility.label(binding),
        );
    }
    if let Some(help) = help {
        append_hint(&mut spans, help.hint_key_label(), eligibility.label(help));
    }
    Line::from(spans)
}

fn append_hint(spans: &mut Vec<Span<'static>>, key: String, description: &str) {
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", hint_description_style()));
    }
    spans.push(Span::styled(key, hint_key_style()));
    spans.push(Span::styled(
        format!(" {description}"),
        hint_description_style(),
    ));
}

pub fn help_lines(context: KeyContext, eligibility: &Eligibility) -> Vec<Line<'static>> {
    let mut groups: Vec<(&str, Vec<&Binding>)> = Vec::new();
    for binding in bindings(context).filter(|b| eligibility.allows(b.action)) {
        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == binding.group) {
            entries.push(binding);
        } else {
            groups.push((binding.group, vec![binding]));
        }
    }
    let mut lines = Vec::new();
    for (group, entries) in groups {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(group.to_string(), hint_key_style()));
        for binding in entries {
            let mut spans = Vec::new();
            append_hint(&mut spans, binding.key_label(), eligibility.label(binding));
            lines.push(Line::from(spans));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_fit_without_partial_bindings_or_ellipsis() {
        for width in 0..150 {
            let line = hint_line(KeyContext::OverlayList, &Eligibility::default(), width);
            assert!(line.width() <= width);
            let text = line.to_string();
            assert!(!text.contains('…'));
            if width >= 6 {
                assert!(text.ends_with("? help"));
            }
        }
        assert_eq!(
            hint_line(KeyContext::OverlayList, &Eligibility::default(), 6).to_string(),
            "? help"
        );
        assert_eq!(
            hint_line(KeyContext::OverlayList, &Eligibility::default(), 22).to_string(),
            "Enter confirm · ? help"
        );
    }
    #[test]
    fn text_fields_do_not_advertise_help_and_disabled_actions_are_absent() {
        let eligibility = Eligibility {
            disabled: vec![Action::Submit],
            ..Eligibility::default()
        };
        let text = hint_line(KeyContext::Composer, &eligibility, 200).to_string();
        assert!(!text.contains("send"));
        assert!(!text.contains("? help"));
    }
}
