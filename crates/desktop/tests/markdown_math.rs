use markdown::{Constructs, Options, ParseOptions, mdast::Node, to_html_with_options, to_mdast};
use std::time::{Duration, Instant};

const PROOF: &str = include_str!("fixtures/holder-proof.md");

fn options() -> ParseOptions {
    ParseOptions {
        constructs: Constructs {
            math_text: true,
            math_flow: true,
            ..Constructs::gfm()
        },
        ..ParseOptions::gfm()
    }
}
fn parse(source: &str) -> Node {
    to_mdast(source, &options()).unwrap()
}
fn visit<'a>(node: &'a Node, found: &mut Vec<&'a Node>) {
    if matches!(node, Node::Math(_) | Node::InlineMath(_)) {
        found.push(node);
    }
    if let Some(children) = node.children() {
        for child in children {
            visit(child, found);
        }
    }
}
fn formulas(node: &Node) -> Vec<&Node> {
    let mut found = Vec::new();
    visit(node, &mut found);
    found
}
fn counts(source: &str) -> (usize, usize) {
    let ast = parse(source);
    let nodes = formulas(&ast);
    (
        nodes
            .iter()
            .filter(|n| matches!(n, Node::InlineMath(_)))
            .count(),
        nodes.iter().filter(|n| matches!(n, Node::Math(_))).count(),
    )
}
type MathSpan = (bool, usize, usize, String);
type Block = (char, usize, usize, Vec<MathSpan>);
fn blocks(ast: &Node, offset: usize) -> Vec<Block> {
    ast.children()
        .unwrap()
        .iter()
        .map(|n| {
            let p = n.position().unwrap();
            let kind = match n {
                Node::Heading(_) => 'h',
                Node::Paragraph(_) => 'p',
                Node::Math(_) => 'M',
                Node::Code(_) => 'c',
                Node::List(_) => 'l',
                Node::Blockquote(_) => 'q',
                _ => '?',
            };
            let math = formulas(n)
                .into_iter()
                .map(|math| {
                    let p = math.position().unwrap();
                    let (display, value) = match math {
                        Node::Math(m) => (true, m.value.clone()),
                        Node::InlineMath(m) => (false, m.value.clone()),
                        _ => unreachable!(),
                    };
                    (
                        display,
                        p.start.offset + offset,
                        p.end.offset + offset,
                        value,
                    )
                })
                .collect();
            (kind, p.start.offset + offset, p.end.offset + offset, math)
        })
        .collect()
}

#[test]
fn fixture_has_exact_nodes_values_and_delimited_spans() {
    let ast = parse(PROOF);
    assert_eq!(counts(PROOF), (15, 11));
    for node in formulas(&ast) {
        let p = node.position().unwrap();
        let source = &PROOF[p.start.offset..p.end.offset];
        match node {
            Node::InlineMath(_) => assert!(source.starts_with(r"\(") && source.ends_with(r"\)")),
            Node::Math(_) => assert!(source.starts_with(r"\[") && source.ends_with(r"\]")),
            _ => unreachable!(),
        }
    }
    assert!(formulas(&ast).iter().any(|n| match n {
        Node::Math(m) => m.value.contains(r"\\[2mm]"),
        _ => false,
    }));
    let sequence = blocks(&ast, 0)
        .iter()
        .map(|b| b.0.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        sequence,
        "h p M p M p h p M p p M p M p M p M p h p M p p M p M p h p M p"
    );
}

#[test]
fn inline_contract() {
    for (source, expected) in [
        (r"unclosed \(x", 0),
        (r"\\(x\\)", 0),
        (r"\(\\)\)", 1),
        ("before \\(a\nb\\) after", 1),
        (r"`\(x\)`", 0),
        (r"[\(x\)](https://example.test)", 1),
        (r"[x](https://example.test/\(x\))", 0),
        (r"\(\)", 1),
        (r"mid \[x\] line", 0),
        (r"# heading \(x\)", 1),
        ("| a | b |\n| - | - |\n| \\(x\\) | y |", 1),
        ("a \\(x\r\ny\\) b", 1),
        ("a \\(x\ty\\)", 1),
    ] {
        assert_eq!(counts(source), (expected, 0), "{source:?}");
    }
    let ast = parse(r"\(\\)\)");
    assert!(matches!(formulas(&ast)[0], Node::InlineMath(m) if m.value == r"\\)"));
}

#[test]
fn display_contract() {
    for (source, expected) in [
        ("\\[\nx\n\\]", (0, 1)),
        (r"\[x\]", (0, 1)),
        ("a\n\\[x\\]\nb", (0, 1)),
        ("\\[\nx\\]\na", (0, 1)),
        (r"\[x\] y", (0, 0)),
        (r"\[1\] Smith et al.", (0, 0)),
        ("\\[\nx", (0, 1)),
        ("> \\[\n> x\n> \\]", (0, 1)),
        ("- \\[\n  x\n  \\]", (0, 1)),
        ("> \\[\nx\n\\]", (0, 1)),
        ("    \\[x\\]", (0, 0)),
        ("\t\\[x\\]", (0, 0)),
        ("```\n\\[x\\]\n```", (0, 0)),
        ("```math\nx\n```", (0, 0)),
        ("$$x$$", (0, 1)),
        ("a $$x$$ b", (1, 0)),
        ("$$x$$ text", (1, 0)),
        ("$$\nx\n$$", (0, 1)),
        ("$$$\nx\n$$$", (0, 1)),
        ("   \\[\n   x\n   \\]", (0, 1)),
        ("\\[\r\nx\\]\r\ny", (0, 1)),
        ("\\[\nx\\] extra\ny\n\\]", (0, 1)),
        ("\\[\n\\\\[2mm]\nx\n\\]", (0, 1)),
    ] {
        assert_eq!(counts(source), expected, "{source:?}");
    }
    let ast = parse("> \\[\nx\n\\]");
    assert!(matches!(&ast.children().unwrap()[0], Node::Blockquote(q) if q.children.len() == 1));
    let ast = parse("   \\[\n   x\n   \\]");
    assert!(matches!(formulas(&ast)[0], Node::Math(m) if m.value == "x"));
}

#[test]
fn dollar_policy() {
    for (source, expected) in [
        ("$5 and $10", 0),
        ("costs $5-$10", 0),
        ("$x$, $y$", 2),
        ("$5$", 1),
        ("$ x$", 0),
        ("$x $", 0),
        ("$x$2", 0),
        ("$\u{00a0}x$", 0),
        ("$x\u{2003}$", 0),
        ("$α+β$", 1),
        ("$x $ still $y$", 1),
        ("$5 and $x$", 1),
        ("$$ x $$", 0), // A complete display is claimed by flow.
        ("a $$ x $$", 1),
        ("`$x$`", 0),
        ("\\$x$", 0),
    ] {
        assert_eq!(counts(source).0, expected, "{source:?}");
    }
    let mut opts = options();
    opts.math_text_single_dollar = false;
    assert!(formulas(&to_mdast("$x$", &opts).unwrap()).is_empty());
}

#[test]
fn disabled_constructs_keep_markdown_escapes_and_code() {
    let opts = ParseOptions::gfm();
    for source in [
        PROOF,
        r"\(x\) and \[x\]",
        "\\[\nx\n\\]",
        "$x$\n$$x$$",
        "`\\(x\\)`",
    ] {
        let ast = to_mdast(source, &opts).unwrap();
        assert!(formulas(&ast).is_empty());
    }
    let ast = to_mdast(r"\(x\) and \[x\]", &opts).unwrap();
    assert_eq!(
        ast.children().unwrap()[0].children().unwrap()[0].to_string(),
        "(x) and [x]"
    );
    assert!(matches!(
        to_mdast("    \\[x\\]", &opts).unwrap().children().unwrap()[0],
        Node::Code(_)
    ));
}

#[test]
fn disabled_constructs_match_published_1_0_0_mdast() {
    // FNV-1a of the complete Debug AST (including values and positions), generated
    // against the unmodified published crate at the VCS revision in docs/markdown-parser-patch.md.
    // No upstream crate dependency or network access is needed to run this test.
    for (source, expected) in [
        (r"\(x\) and \[x\]", 0xa6fe239694baf92a),
        ("\\[\nx\n\\]", 0xafc525829b54e991),
        ("$x$\n$$x$$", 0xdc81d18ac852593f),
        (r"\\(x\\)", 0xeb4e26a48eb9b6a9),
        ("before \\(a\nb\\) after", 0x496ea0eb844a47c2),
        (r"[\(x\)](https://example.test)", 0x74390d254f05b6ee),
        ("| a | b |\n| - | - |\n| \\(x\\) | y |", 0x076ffafcf0482b07),
        ("$$\nx\n$$", 0x8e20bddf7749e443),
        ("costs $5-$10", 0xf253dfcfb8c9da1b),
        ("> \\[\n> x\n> \\]", 0x0d87de0264ce17a1),
        ("- \\[\n  x\n  \\]", 0x594566f3dfe50a70),
        ("    \\[x\\]", 0x6ad74a844aa6ebbd),
        ("```math\nx\n```", 0x1c57344b92f7d1d2),
        ("\\[\r\nx\\]\r\ny", 0x3dcd1f6d2cc60a7f),
    ] {
        let ast = format!("{:?}", to_mdast(source, &ParseOptions::gfm()).unwrap());
        let actual = ast.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        assert_eq!(actual, expected, "{source:?}");
    }
}

// GPUI reparses the last block plus the append, preserving earlier block spans.
fn replay(boundaries: &[usize]) {
    let mut previous = 0;
    let mut result = Vec::<Block>::new();
    for &end in boundaries {
        let start = result.pop().map_or(previous, |last| last.1);
        result.extend(blocks(&parse(&PROOF[start..end]), start));
        assert_eq!(
            result,
            blocks(&parse(&PROOF[..end]), 0),
            "append {previous}..{end}"
        );
        previous = end;
    }
    assert_eq!(result, blocks(&parse(PROOF), 0));
}

#[test]
fn streaming_tail_reparse_has_identical_blocks_and_spans() {
    let chars: Vec<_> = PROOF
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .collect();
    replay(&chars);
    for size in 1..=16 {
        let mut ends: Vec<_> = chars.chunks(size).map(|c| *c.last().unwrap()).collect();
        if ends.last() != Some(&PROOF.len()) {
            ends.push(PROOF.len());
        }
        replay(&ends);
    }
    // Every possible single append split, not just uniform chunks.
    for &boundary in &chars[..chars.len() - 1] {
        replay(&[boundary, PROOF.len()]);
    }
}

#[test]
fn html_compilers_consume_existing_math_events() {
    let opts = Options {
        parse: options(),
        ..Options::default()
    };
    for (source, class) in [
        (r"a \(x\) b", "math-inline"),
        (r"\[x\]", "math-display"),
        ("\\[\nx\n\\]", "math-display"),
        ("$$x$$", "math-display"),
    ] {
        let html = to_html_with_options(source, &opts).unwrap();
        assert!(html.contains(class) && html.contains('x'), "{}", html);
    }
}

#[test]
fn unmatched_inline_openers_are_linear() {
    for input in [r"\(".repeat(16_000), "$x ".repeat(16_000)] {
        let start = Instant::now();
        assert_eq!(counts(&input), (0, 0));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
