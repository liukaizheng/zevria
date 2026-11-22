//! Summary labels for a session's subtask switcher.
use zevria_foundation::SubtaskStatus;

pub fn active_count(statuses: impl IntoIterator<Item = Option<SubtaskStatus>>) -> usize {
    statuses
        .into_iter()
        .filter(|status| status.is_none_or(|status| !status.is_terminal()))
        .count()
}

pub fn summary_title(statuses: impl IntoIterator<Item = Option<SubtaskStatus>>) -> String {
    let mut total = 0;
    let running = active_count(statuses.into_iter().inspect(|_| total += 1));
    format!("Subtasks · {running} running · {total} total")
}

pub fn chip_label(active: usize) -> String {
    match active {
        0 => "Subtasks".into(),
        1 => "1 subtask".into(),
        n => format!("{n} subtasks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubtaskStatus::*;

    #[test]
    fn missing_and_nonterminal_statuses_count_as_active() {
        let statuses = [
            None,
            Some(Starting),
            Some(Running),
            Some(Completed),
            Some(Failed),
            Some(Cancelled),
        ];
        assert_eq!(active_count(statuses), 3);
        assert_eq!(summary_title(statuses), "Subtasks · 3 running · 6 total");
        assert_eq!(summary_title([]), "Subtasks · 0 running · 0 total");
    }

    #[test]
    fn chip_uses_the_active_count() {
        assert_eq!(chip_label(0), "Subtasks");
        assert_eq!(chip_label(1), "1 subtask");
        assert_eq!(chip_label(2), "2 subtasks");
    }
}
