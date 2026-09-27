use super::*;
use crate::{
    command::{ClassifiedInput, CommandRegistry},
    composer::ComposerState,
};
use zevria_content::{PromptBlock, PromptImage, UserPrompt};

#[test]
fn references_activate_at_word_boundaries_not_in_emails_or_escaped_literals() {
    for text in [
        "@",
        "Explain @app",
        "first\n@src",
        "$review @code",
        "/ensemble-plan check @x",
        "@one and @two",
        "x \u{0301}@src",
    ] {
        let query = file_query(text, text.len(), []).unwrap();
        assert_eq!(query.kind, CompletionKind::File);
        assert_eq!(query.replacement.end, text.len());
        assert!(text[query.replacement.clone()].starts_with('@'));
    }
    for text in [
        "a@example.com",
        "hello\\@src",
        "\\@src",
        "hello (@src",
        "@one ",
        "no references",
    ] {
        assert_eq!(file_query(text, text.len(), []), None, "{text}");
    }
    assert!(file_query("@src", 0, []).is_none());
    assert_eq!(file_query("@one @two", 3, []).unwrap().replacement, 0..4);
    assert_eq!(file_query("@one @two", 7, []).unwrap().prefix, "t");
}

#[test]
fn query_prefix_is_caret_local_but_replacement_covers_the_whole_token() {
    for (text, caret, prefix, token) in [
        ("Explain @app.rs now", 12, "app", "@app.rs"),
        ("@src/main.rs suffix", 5, "src/", "@src/main.rs"),
        (
            "@\"docs/my file.md\" later",
            10,
            "docs/my ",
            "@\"docs/my file.md\"",
        ),
        ("@\"unterminated file", 7, "unter", "@\"unterminated file"),
        (
            "@docs/my\\ file.md suffix",
            14,
            "docs/my file",
            "@docs/my\\ file.md",
        ),
    ] {
        let query = file_query(text, caret, []).unwrap();
        assert_eq!(query.prefix, prefix, "{text}");
        assert_eq!(&text[query.replacement], token);
    }
}

#[test]
fn canonical_references_round_trip_quotes_backslashes_whitespace_and_unicode() {
    for path in [
        "src/main.rs",
        "docs/my file.md",
        "a\"b\\c.md",
        "目录/👩🏽‍💻 e\u{301}.rs",
        "one@two",
    ] {
        let reference = file_reference(path);
        let query = file_query(&reference, reference.len(), []).unwrap();
        assert_eq!(query.prefix, path);
        assert_eq!(query.replacement, 0..reference.len());
        if path.contains(' ') || path.contains('"') || path.contains('\\') {
            assert!(reference.starts_with("@\""));
            assert!(reference.ends_with('"'));
        }
    }
    let input = "@e\u{301}界.rs";
    for byte in 2..=3 {
        assert_eq!(file_query(input, byte, []).unwrap().prefix, "");
    }
    assert_eq!(file_query(input, 4, []).unwrap().prefix, "e\u{301}");
}

fn install(state: &mut ComposerState, paths: &[&str]) -> FileCompletionRequest {
    let request = state.reconcile_file_completion(true).unwrap();
    assert!(state.install_file_results(
        &request,
        paths.iter().map(|s| (*s).into()).collect(),
        FileSearchStatus::default()
    ));
    request
}

#[test]
fn file_acceptance_is_one_atomic_text_edit_with_exact_caret_and_suffix_restore() {
    for (text, cursor, expected, caret) in [
        (
            "Explain @app.rs suffix",
            12,
            "Explain @crates/app.rs suffix",
            23,
        ),
        ("@app", 4, "@crates/app.rs ", 15),
        ("@app\nnext", 4, "@crates/app.rs\nnext", 15),
        ("@app\r\nnext", 4, "@crates/app.rs\r\nnext", 16),
        ("@app \u{0301}next", 4, "@crates/app.rs \u{0301}next", 17),
    ] {
        let mut state = ComposerState::default();
        state.replace(text.into(), cursor);
        let draft = state.snapshot();
        let generation = state.generation();
        install(&mut state, &["crates/app.rs"]);
        assert_eq!(state.generation(), generation);
        assert!(state.matches_draft(&draft));
        assert_eq!(
            state.accept_highlighted_completion(),
            Some(CompletionAcceptance::Text)
        );
        assert_eq!(state.text(), expected);
        assert_eq!(state.cursor(), caret);
        assert_eq!(state.prompt(), UserPrompt::from_text(expected));
        assert!(!state.completion_filter_active());
        assert!(state.undo());
        assert_eq!((state.text(), state.cursor()), (text, cursor));
        assert!(state.redo());
        assert_eq!((state.text(), state.cursor()), (expected, caret));
    }
}

#[test]
fn images_are_hard_query_boundaries_and_survive_adjacent_completion_and_undo() {
    let image = PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let mut state = ComposerState::default();
    state.replace_prompt(
        &UserPrompt::new(vec![
            PromptBlock::Image(image.clone()),
            PromptBlock::Text(" see @a tail ".into()),
            PromptBlock::Image(image),
        ])
        .unwrap(),
    );
    while state.cursor() > "[image 1] see @a".len() {
        state.move_left();
    }
    let before = state.prompt();
    install(&mut state, &["my file.txt"]);
    assert_eq!(
        state.accept_highlighted_completion(),
        Some(CompletionAcceptance::Text)
    );
    assert_eq!(
        state.text(),
        "[image 1] see @\"my file.txt\" tail [image 2]"
    );
    assert_eq!(state.prompt().images().count(), 2);
    assert!(state.undo());
    assert_eq!(state.prompt(), before);
    assert!(file_query("@\"a[image 1]b\"", 3, std::iter::once(3..12)).is_none());
    assert!(file_query("@[image 1]", 2, std::iter::once(1..10)).is_none());
}

#[test]
fn results_and_dismissal_are_transient_and_cannot_change_acknowledgement_identity() {
    let mut state = ComposerState::default();
    state.insert_text("@a");
    let before = state.snapshot();
    let first = install(&mut state, &["a.rs", "b/a.rs"]);
    state.move_menu_down();
    assert!(state.install_file_results(
        &first,
        vec!["0/a.rs".into(), "b/a.rs".into(), "a.rs".into()],
        FileSearchStatus::default()
    ));
    assert_eq!(state.menu().selected(), 1);
    assert_eq!(state.snapshot(), before);
    assert!(state.cancel_completion());
    assert_eq!(state.snapshot(), before);
    assert!(!state.completion_filter_active());
    assert!(!state.install_file_results(
        &first,
        vec!["late.rs".into()],
        FileSearchStatus::default()
    ));
    assert!(state.reconcile_file_completion(true).is_none());
    state.move_left();
    let second = state.reconcile_file_completion(true).unwrap();
    assert_ne!(second.activation, first.activation);
    state.move_right();
    let third = state.reconcile_file_completion(true).unwrap();
    assert_eq!(second.activation, third.activation);
    assert_ne!(second.request, third.request);
    assert!(!state.install_file_results(
        &second,
        vec!["late.rs".into()],
        FileSearchStatus::default()
    ));
    state.restore(before);
    assert!(!state.install_file_results(
        &third,
        vec!["late.rs".into()],
        FileSearchStatus::default()
    ));
    assert!(state.completion_filter_active());
}

#[test]
fn query_changes_clear_results_but_do_not_reopen_the_index_activation() {
    let mut state = ComposerState::default();
    state.insert_text("@");
    let first = install(&mut state, &["a"]);
    state.insert_character('b');
    assert_eq!(state.matching_completion_count(), 0);
    assert!(state.accept_highlighted_completion().is_none());
    let next = state.reconcile_file_completion(true).unwrap();
    assert_eq!(first.activation, next.activation);
    assert_ne!(first.request, next.request);
    state.clear();
    assert!(!state.install_file_results(&next, vec!["bad".into()], FileSearchStatus::default()));
    assert!(!state.undo());
}

#[test]
fn file_references_are_not_a_command_namespace() {
    let registry = CommandRegistry::default();
    for text in ["@src/main.rs", "Explain @\"my file.md\"", "@unresolved"] {
        assert!(registry.matches(text, text.len()).is_empty());
        assert_eq!(
            registry.classify(text),
            Ok(ClassifiedInput::Message(text.into()))
        );
    }
    let mut state = ComposerState::default();
    state.insert_text("/ensemble-plan @app");
    install(&mut state, &["app.rs"]);
    assert_eq!(
        state.accept_highlighted_completion(),
        Some(CompletionAcceptance::Text)
    );
    assert!(
        matches!(state.classify().unwrap(), ClassifiedInput::Ensemble { prompt, .. } if prompt == UserPrompt::from_text("@app.rs"))
    );
}
