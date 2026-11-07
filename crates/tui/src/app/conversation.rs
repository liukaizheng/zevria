pub(crate) use zevria_presentation::conversation::*;

#[cfg(test)]
mod rendering_tests {
    #[test]
    fn citation_entities_cannot_reintroduce_terminal_controls_through_markdown() {
        let text = zevria_content::citations::render(
            "Claim",
            Some(&serde_json::json!([
                {"type":"url_citation","start_index":0,"end_index":5,"title":"&Tab; and &#27;","url":"https://example.com/?q=&Tab;&x=1"}
            ])),
        );
        let displayed = crate::markdown::markdown_lines(&text, ratatui::style::Style::default(), 0)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(!displayed.chars().any(char::is_control));
        assert!(displayed.contains("https://example.com/?q=&Tab;&x=1"));
    }
}
