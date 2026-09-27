//! Shared Syntect grammars, Zevria theme mapping, and stateful highlighting.

use std::{path::Path, str::FromStr as _, sync::OnceLock};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color as SyntectColor, FontStyle, ScopeSelectors, Style as SyntectStyle, StyleModifier,
        Theme, ThemeItem, ThemeSettings,
    },
    parsing::{SyntaxReference, SyntaxSet},
};

use crate::theme::{ZevriaTheme, theme};

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static CODE_THEME: OnceLock<Theme> = OnceLock::new();

/// Opaque handle to one bundled Syntect grammar.
#[derive(Clone, Copy)]
pub struct CodeGrammar(&'static SyntaxReference);

/// Stateful line highlighter for one source-file side.
pub struct CodeHighlighter {
    highlighter: HighlightLines<'static>,
}

impl CodeHighlighter {
    pub fn new(grammar: CodeGrammar) -> Self {
        Self {
            highlighter: HighlightLines::new(grammar.0, code_theme()),
        }
    }

    /// Highlight one raw source line, including any line ending used to advance
    /// parser state, while returning only its visible content as owned spans.
    pub fn highlight_line(&mut self, raw_line: &str, fallback: Style) -> Vec<Span<'static>> {
        let content = visible_code_line(raw_line);
        let highlighted = self.highlighter.highlight_line(raw_line, syntax_set()).ok();
        match highlighted {
            Some(ranges) => highlighted_spans(ranges, content.len(), fallback),
            None if content.is_empty() => Vec::new(),
            None => vec![Span::styled(content.to_string(), fallback)],
        }
    }
}

/// Resolve a Markdown info-string or command-language token.
pub fn grammar_for_token(token: &str) -> Option<CodeGrammar> {
    syntax_set().find_syntax_by_token(token).map(CodeGrammar)
}

/// Resolve a bundled grammar from a path without touching the filesystem.
/// Complete filenames are checked before ordinary extensions so special names
/// such as `Makefile` can select their bundled grammar.
pub fn grammar_for_path(path: &Path) -> Option<CodeGrammar> {
    let syntax_set = syntax_set();
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| syntax_set.find_syntax_by_extension(name))
        .or_else(|| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        })
        .map(CodeGrammar)
}

/// Highlight source code into owned lines. Unknown or omitted grammars retain
/// `fallback`, and grammar state is carried across source lines.
pub fn highlighted_code_lines(
    input: &str,
    language: Option<&str>,
    fallback: Style,
) -> Vec<Line<'static>> {
    let mut highlighter = language
        .and_then(grammar_for_token)
        .map(CodeHighlighter::new);

    input
        .split_inclusive('\n')
        .map(|raw_line| {
            let spans = highlighter.as_mut().map_or_else(
                || {
                    let content = visible_code_line(raw_line);
                    if content.is_empty() {
                        Vec::new()
                    } else {
                        vec![Span::styled(content.to_string(), fallback)]
                    }
                },
                |highlighter| highlighter.highlight_line(raw_line, fallback),
            );
            Line::from(spans)
        })
        .collect()
}

/// Strip the terminal line ending from a raw parser input line.
pub fn visible_code_line(raw_line: &str) -> &str {
    let Some(without_newline) = raw_line.strip_suffix('\n') else {
        return raw_line;
    };
    without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline)
}

fn highlighted_spans(
    ranges: Vec<(SyntectStyle, &str)>,
    visible_len: usize,
    fallback: Style,
) -> Vec<Span<'static>> {
    let mut remaining = visible_len;
    let mut spans = Vec::new();
    for (style, text) in ranges {
        if remaining == 0 {
            break;
        }
        let visible_len = text.len().min(remaining);
        spans.push(Span::styled(
            text[..visible_len].to_string(),
            syntect_style(fallback, style),
        ));
        remaining -= visible_len;
    }
    spans
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn code_theme() -> &'static Theme {
    CODE_THEME.get_or_init(|| zevria_code_theme(theme()))
}

fn zevria_code_theme(tokens: &ZevriaTheme) -> Theme {
    Theme {
        name: Some("Zevria".to_string()),
        author: Some("Zevria".to_string()),
        settings: ThemeSettings {
            foreground: Some(syntect_color(tokens.text.primary)),
            background: Some(syntect_color(tokens.surfaces.canvas)),
            caret: Some(syntect_color(tokens.roles.tools)),
            selection: Some(syntect_color(tokens.surfaces.selection_background)),
            selection_foreground: Some(syntect_color(tokens.surfaces.selection_foreground)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            theme_item(
                "keyword, storage",
                tokens.syntax.keyword,
                FontStyle::empty(),
            ),
            theme_item(
                "entity.name.function, support.function, variable.function",
                tokens.syntax.function,
                FontStyle::empty(),
            ),
            theme_item(
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.interface, support.type, support.class",
                tokens.syntax.r#type,
                FontStyle::empty(),
            ),
            theme_item("string", tokens.syntax.string, FontStyle::empty()),
            theme_item(
                "constant.numeric, constant.language, constant.character, constant.other",
                tokens.syntax.constant,
                FontStyle::empty(),
            ),
            theme_item("comment", tokens.syntax.comment, FontStyle::ITALIC),
            theme_item(
                "invalid, invalid.illegal, invalid.deprecated",
                tokens.syntax.invalid,
                FontStyle::BOLD,
            ),
        ],
    }
}

fn theme_item(scope: &str, color: Color, font_style: FontStyle) -> ThemeItem {
    ThemeItem {
        scope: ScopeSelectors::from_str(scope).expect("Zevria syntax scopes are valid"),
        style: StyleModifier {
            foreground: Some(syntect_color(color)),
            background: None,
            font_style: Some(font_style),
        },
    }
}

fn syntect_color(color: Color) -> SyntectColor {
    let (r, g, b) = crate::theme::rgb_channels(color);
    SyntectColor { r, g, b, a: 0xff }
}

fn syntect_style(base: Style, syntax: SyntectStyle) -> Style {
    let mut style = base.fg(crate::theme::rgb(
        syntax.foreground.r,
        syntax.foreground.g,
        syntax.foreground.b,
    ));
    if syntax.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if syntax.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if syntax.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn find_span<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains(needle))
            .unwrap_or_else(|| panic!("missing highlighted token {needle:?}"))
    }

    #[test]
    fn resolves_grammars_from_extensions_special_filenames_and_missing_paths() {
        let rust = grammar_for_path(Path::new("src/lib.rs")).expect("Rust grammar");
        assert_eq!(rust.0.name, "Rust");

        let makefile = grammar_for_path(Path::new("nested/Makefile")).expect("Makefile grammar");
        assert_eq!(makefile.0.name, "Makefile");

        assert!(grammar_for_path(Path::new("file.zevria-unknown")).is_none());

        let nonexistent = grammar_for_path(Path::new(
            "/path/that/does/not/exist/and/must/not/be/read/source.rs",
        ));
        assert_eq!(
            nonexistent.map(|grammar| grammar.0.name.as_str()),
            Some("Rust")
        );
    }

    #[test]
    fn line_highlighter_omits_crlf_and_preserves_blank_lines() {
        let grammar = grammar_for_path(Path::new("file.rs")).expect("Rust grammar");
        let mut highlighter = CodeHighlighter::new(grammar);
        let code = highlighter.highlight_line("let value = 42;\r\n", Style::default());
        assert_eq!(
            code.iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "let value = 42;"
        );
        assert!(code.iter().all(|span| !span.content.contains(['\r', '\n'])));
        assert!(
            highlighter
                .highlight_line("\r\n", Style::default())
                .is_empty()
        );
    }

    #[test]
    fn whole_input_highlighting_handles_trailing_and_missing_newlines() {
        let fallback = Style::default().fg(theme().content.code);
        let without_trailing =
            highlighted_code_lines("let first = 1;\nlet second = 2;", Some("rust"), fallback);
        let with_trailing =
            highlighted_code_lines("let first = 1;\nlet second = 2;\n", Some("rust"), fallback);
        let expected = vec!["let first = 1;", "let second = 2;"];
        assert_eq!(
            without_trailing.iter().map(line_text).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            with_trailing.iter().map(line_text).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn line_highlighter_carries_multiline_grammar_state() {
        let grammar = grammar_for_path(Path::new("file.rs")).expect("Rust grammar");
        let fallback = Style::default().bg(theme().surfaces.panel);
        let mut highlighter = CodeHighlighter::new(grammar);
        highlighter.highlight_line("/* opening comment\n", fallback);
        let middle = highlighter.highlight_line("still inside\n", fallback);
        let comment = middle
            .iter()
            .find(|span| span.content.contains("still inside"))
            .expect("comment body");
        assert_eq!(comment.style.fg, Some(theme().syntax.comment));
        assert_eq!(comment.style.bg, Some(theme().surfaces.panel));
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));

        let closing = highlighter.highlight_line("*/ let value = 42;\n", fallback);
        let keyword = closing
            .iter()
            .find(|span| span.content.contains("let"))
            .expect("keyword after comment");
        assert_eq!(keyword.style.fg, Some(theme().syntax.keyword));
    }

    #[test]
    fn zevria_syntax_theme_maps_rust_scopes_to_exact_semantic_colors() {
        let source = concat!(
            "struct Zevria;\n",
            "fn illuminate(value: Zevria) -> usize {\n",
            "    let message = \"hello\";\n",
            "    42 // zevria comment\n",
            "}\n",
        );
        let lines = highlighted_code_lines(source, Some("rust"), Style::default());

        assert_eq!(
            find_span(&lines, "struct").style.fg,
            Some(theme().syntax.keyword)
        );
        assert_eq!(
            find_span(&lines, "illuminate").style.fg,
            Some(theme().syntax.function)
        );
        assert_eq!(
            find_span(&lines, "Zevria").style.fg,
            Some(theme().syntax.r#type)
        );
        assert_eq!(
            find_span(&lines, "hello").style.fg,
            Some(theme().syntax.string)
        );
        assert_eq!(
            find_span(&lines, "42").style.fg,
            Some(theme().syntax.constant)
        );
        let comment = find_span(&lines, "zevria comment");
        assert_eq!(comment.style.fg, Some(theme().syntax.comment));
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));

        let tokens = theme();
        let theme = code_theme();
        assert_eq!(
            theme.settings.foreground,
            Some(syntect_color(tokens.text.primary))
        );
        assert_eq!(
            theme.settings.background,
            Some(syntect_color(tokens.surfaces.canvas))
        );
        assert!(theme.scopes.iter().any(|item| {
            item.style.foreground == Some(syntect_color(tokens.syntax.invalid))
                && item.style.font_style == Some(FontStyle::BOLD)
        }));
    }

    #[test]
    fn syntax_theme_constructor_is_pure_and_uses_explicit_tokens() {
        for color in [
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
            Color::Rgb(30, 30, 46),
        ] {
            let mut tokens = crate::theme::ZEVRIA_DARK;
            tokens.text.primary = color;
            tokens.syntax.keyword = color;
            tokens.surfaces.canvas = color;
            let constructed = zevria_code_theme(&tokens);
            assert_eq!(constructed.settings.foreground, Some(syntect_color(color)));
            assert_eq!(constructed.settings.background, Some(syntect_color(color)));
            assert_eq!(
                constructed.scopes[0].style.foreground,
                Some(syntect_color(color))
            );
        }
    }

    #[test]
    fn highlighted_code_uses_true_color_and_unknown_language_fallback() {
        let fallback = Style::default()
            .fg(theme().content.code)
            .bg(theme().surfaces.panel)
            .add_modifier(Modifier::BOLD);
        let highlighted = highlighted_code_lines("let value = 42;\n", Some("rust"), fallback);
        assert!(
            highlighted
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| span.style.fg)
                .all(|color| matches!(color, Color::Rgb(_, _, _)))
        );
        assert!(
            highlighted
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg == Some(theme().surfaces.panel))
        );

        let unknown =
            highlighted_code_lines("fn unknown() {}\n\n", Some("not-a-language"), fallback);
        assert_eq!(
            unknown.iter().map(line_text).collect::<Vec<_>>(),
            ["fn unknown() {}", ""]
        );
        for span in unknown.iter().flat_map(|line| &line.spans) {
            assert_eq!(span.style, fallback);
        }
    }
}
