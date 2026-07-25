use std::ops::Range;

use gpui::SharedString;
use markdown::mdast::{self, Node};

use crate::{
    highlighter::HighlightTheme,
    text::{
        document::ParsedDocument,
        markdown_ext::MarkdownParseContext,
        node::{
            self, BlockNode, CodeBlock, ImageNode, InlineNode, LinkMark, NodeContext, Paragraph,
            Span, Table, TableRow, TextMark,
        },
    },
};

/// Parse Markdown into a tree of nodes.
///
/// TODO: Remove `highlight_theme` option, this should in render stage.
pub(crate) fn parse(
    source: &str,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> Result<ParsedDocument, SharedString> {
    let options = cx.markdown_extensions.parse_options();
    markdown::to_mdast(&source, &options)
        .map(|n| ast_to_document(source, n, cx, highlight_theme))
        .map_err(|e| e.to_string().into())
}

fn parse_table_row(table: &mut Table, node: &mdast::TableRow, cx: &mut NodeContext) {
    let mut row = TableRow::default();
    node.children.iter().for_each(|c| {
        match c {
            Node::TableCell(cell) => {
                parse_table_cell(&mut row, cell, cx);
            }
            _ => {}
        };
    });
    table.children.push(row);
}

fn parse_table_cell(row: &mut node::TableRow, node: &mdast::TableCell, cx: &mut NodeContext) {
    let mut paragraph = Paragraph::default();
    node.children.iter().for_each(|c| {
        parse_paragraph(&mut paragraph, c, cx);
    });
    let table_cell = node::TableCell {
        children: paragraph,
        ..Default::default()
    };
    row.children.push(table_cell);
}

/// Push a text run with its existing `marks` plus `new_mark` across the full
/// run.
///
/// If the last mark already covers the full run, merge into it. Otherwise add a
/// new full-run mark. Empty runs are skipped so callers can flush freely.
fn push_merged(
    paragraph: &mut Paragraph,
    text: String,
    marks: Vec<(Range<usize>, TextMark)>,
    new_mark: TextMark,
) {
    if text.is_empty() {
        return;
    }

    let mut node = InlineNode::new(text).marks(marks);
    let len = node.text.len();
    if let Some(last) = node.marks.last_mut()
        && last.0.start == 0
        && last.0.end == len
    {
        last.1.merge(new_mark);
    } else {
        node.marks.push((0..len, new_mark));
    }
    paragraph.push(node);
}

/// The colors used for native markdown inline mark highlighting:
/// - Quark handles (`@Agy`, `@Sonnet`): `pink-400`
/// - File mentions (`@README.md`, `@src/main.rs`): `purple-400`
/// - Slash commands (`/learn`, `/plan`, `/goal`): `amber-400`
const MENTION_COLOR_NAME: &str = "pink-400";
const FILE_MENTION_COLOR_NAME: &str = "purple-400";
const COMMAND_COLOR_NAME: &str = "amber-400";

/// Scan a plain markdown text run for `@mention` (quarks & files) and `/command` tokens
/// and mark each one bold + colored, in a single `InlineNode` with sparse marks over the
/// unmatched text in between.
///
/// Quark handles (`@Agy`) render in `pink-400`, file paths (`@README.md`, `@src/app.rs`)
/// render in `purple-400`, and slash commands (`/learn`) render in `amber-400`.
/// Boundary rules: an `@mention` cannot be preceded by an alphanumeric (so `user@host` is
/// left alone), a `/command` must start the run or follow whitespace (so a file
/// path like `src/app.rs` is left alone).
fn push_mention_and_command_marks(paragraph: &mut Paragraph, text: &str) {
    if text.is_empty() {
        return;
    }

    let mut marks: Vec<(Range<usize>, TextMark)> = Vec::new();
    let mut prev_char: Option<char> = None;
    let mut chars = text.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        let is_mention_start = ch == '@' && !prev_char.is_some_and(|c| c.is_alphanumeric());
        let is_command_start = ch == '/' && prev_char.is_none_or(|c| c.is_whitespace());

        if is_mention_start || is_command_start {
            let mut end = start + ch.len_utf8();
            if is_mention_start {
                while let Some(&(idx, nc)) = chars.peek() {
                    if nc.is_alphanumeric() || nc == '-' || nc == '_' || nc == '.' || nc == '/' || nc == '\\' {
                        end = idx + nc.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                while end > start + ch.len_utf8() {
                    let last_char = text[start..end].chars().next_back().unwrap();
                    if matches!(last_char, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '>') {
                        if last_char == '.' {
                            let token_body = &text[start + ch.len_utf8()..end];
                            if token_body.trim_end_matches('.').contains('.') {
                                end -= last_char.len_utf8();
                                continue;
                            } else {
                                break;
                            }
                        }
                        end -= last_char.len_utf8();
                    } else {
                        break;
                    }
                }
            } else {
                while let Some(&(idx, nc)) = chars.peek() {
                    if nc.is_alphanumeric() || nc == '-' || nc == '_' {
                        end = idx + nc.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
            }

            // A bare `@` or `/` with no identifier following is not a token.
            if end > start + ch.len_utf8() {
                let color_name = if is_command_start {
                    COMMAND_COLOR_NAME
                } else {
                    let token_body = &text[start + ch.len_utf8()..end];
                    if token_body.contains('.') || token_body.contains('/') || token_body.contains('\\') {
                        FILE_MENTION_COLOR_NAME
                    } else {
                        MENTION_COLOR_NAME
                    }
                };

                if let Ok(color) = crate::try_parse_color(color_name) {
                    marks.push((start..end, TextMark::default().bold().color(color)));
                }
                prev_char = text[start..end].chars().next_back();
                continue;
            }
        }

        prev_char = Some(ch);
    }

    paragraph.push(InlineNode::new(text).marks(marks));
}

/// Parse `children` and apply `mark` across each emitted text run.
///
/// Nested child marks are kept and shifted to match the combined text for the
/// current run, which lets nested emphasis like `**_x_**` render as both bold
/// and italic. Inline images split the run and are emitted as sibling image
/// nodes. The return value is the plain text from all children, for callers that
/// need to pass text back to their parent node.
fn merge_children_with_mark(
    paragraph: &mut Paragraph,
    children: &[mdast::Node],
    mark: TextMark,
    cx: &mut NodeContext,
) -> String {
    let mut text = String::new();
    let mut merged_text = String::new();
    let mut merged_marks = Vec::new();

    for child in children {
        let mut child_paragraph = Paragraph::default();
        let child_text = parse_paragraph(&mut child_paragraph, child, cx);
        text.push_str(&child_text);

        for node in child_paragraph.children {
            let merged_offset = merged_text.len();
            merged_text.push_str(&node.text);

            for (range, child_mark) in node.marks {
                merged_marks.push((
                    range.start + merged_offset..range.end + merged_offset,
                    child_mark,
                ));
            }

            if let Some(mut image) = node.image {
                if let Some(link_mark) = mark.link.clone() {
                    image.link = Some(link_mark);
                }

                // GPUI InteractiveText does not support inline images, so
                // flush the accumulated text run and emit the image as its
                // own sibling InlineNode.
                push_merged(
                    paragraph,
                    std::mem::take(&mut merged_text),
                    std::mem::take(&mut merged_marks),
                    mark.clone(),
                );
                paragraph.push(InlineNode::image(image));
            }
        }
    }

    push_merged(paragraph, merged_text, merged_marks, mark);
    text
}

fn append_inline_html_blocks(paragraph: &mut Paragraph, blocks: Vec<BlockNode>) -> Option<String> {
    let mut text = String::new();

    for block in blocks {
        match block {
            BlockNode::Root { children, .. } => {
                text.push_str(&append_inline_html_blocks(paragraph, children)?);
            }
            BlockNode::Paragraph(html_paragraph) => {
                text.push_str(&html_paragraph.text());
                for child in html_paragraph.children {
                    paragraph.push(child);
                }
            }
            BlockNode::Break { .. } => {
                text.push('\n');
                paragraph.push(InlineNode::new("\n"));
            }
            _ => return None,
        }
    }

    Some(text)
}

fn parse_paragraph(paragraph: &mut Paragraph, node: &mdast::Node, cx: &mut NodeContext) -> String {
    let span = node.position().map(|pos| Span {
        start: cx.offset + pos.start.offset,
        end: cx.offset + pos.end.offset,
    });
    if let Some(span) = span {
        paragraph.set_span(span);
    }

    let mut text = String::new();

    match node {
        Node::Paragraph(val) => {
            val.children.iter().for_each(|c| {
                text.push_str(&parse_paragraph(paragraph, c, cx));
            });
        }
        Node::Text(val) => {
            text = val.value.clone();
            push_mention_and_command_marks(paragraph, &val.value);
        }
        Node::Emphasis(val) => {
            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark::default().italic(),
                cx,
            );
        }
        Node::Strong(val) => {
            text =
                merge_children_with_mark(paragraph, &val.children, TextMark::default().bold(), cx);
        }
        Node::Delete(val) => {
            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark::default().strikethrough(),
                cx,
            );
        }
        Node::InlineCode(val) => {
            text = val.value.clone();
            paragraph.push(
                InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default().code())]),
            );
        }
        Node::Link(val) => {
            let link_mark = Some(LinkMark {
                url: val.url.clone().into(),
                title: val.title.clone().map(|s| s.into()),
                ..Default::default()
            });

            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark {
                    link: link_mark,
                    ..Default::default()
                },
                cx,
            );
        }
        Node::Image(raw) => {
            paragraph.push_image(ImageNode {
                url: raw.url.clone().into(),
                title: raw.title.clone().map(|t| t.into()),
                alt: Some(raw.alt.clone().into()),
                ..Default::default()
            });
        }
        Node::InlineMath(raw) => {
            text = raw.value.clone();
            paragraph.push(
                InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default().code())]),
            );
        }
        Node::MdxTextExpression(raw) => {
            text = raw.value.clone();
            paragraph
                .push(InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default())]));
        }
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => {
                if let Some(inline_text) = append_inline_html_blocks(paragraph, el.blocks) {
                    text = inline_text;
                } else {
                    if cfg!(debug_assertions) {
                        tracing::warn!("unsupported inline html tag: {:#?}", val.value);
                    }
                }
            }
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("failed parsing html: {:#?}", err);
                }

                text.push_str(&val.value);
            }
        },
        Node::FootnoteReference(foot) => {
            let prefix = format!("[{}]", foot.identifier);
            paragraph.push(InlineNode::new(&prefix).marks(vec![(
                0..prefix.len(),
                TextMark {
                    italic: true,
                    ..Default::default()
                },
            )]));
        }
        Node::LinkReference(link) => {
            let link_mark = LinkMark {
                url: "".into(),
                title: link.label.clone().map(Into::into),
                identifier: Some(link.identifier.clone().into()),
            };

            text = merge_children_with_mark(
                paragraph,
                &link.children,
                TextMark {
                    link: Some(link_mark),
                    ..Default::default()
                },
                cx,
            );
        }
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported inline node: {:#?}", node);
            }
        }
    }

    text
}

fn ast_to_document(
    source: &str,
    root: mdast::Node,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> ParsedDocument {
    let root = match root {
        Node::Root(r) => r,
        _ => panic!("expected root node"),
    };

    let blocks = root
        .children
        .into_iter()
        .map(|c| ast_to_node(source, c, cx, highlight_theme))
        .collect();
    ParsedDocument {
        source: source.to_string().into(),
        blocks,
    }
}

fn new_span(pos: Option<markdown::unist::Position>, cx: &NodeContext) -> Option<Span> {
    let pos = pos?;

    Some(Span {
        start: cx.offset + pos.start.offset,
        end: cx.offset + pos.end.offset,
    })
}

fn ast_to_node(
    source: &str,
    value: mdast::Node,
    cx: &mut NodeContext,
    highlight_theme: &HighlightTheme,
) -> BlockNode {
    let span = new_span(value.position().cloned(), cx);
    let parse_cx = MarkdownParseContext::new(source, cx.offset);
    if let Some(mut node) = cx.markdown_extensions.parse_block(&value, &parse_cx) {
        node.set_span(span);
        return BlockNode::Custom(node);
    }

    match value {
        Node::Root(_) => unreachable!("node::Root should be handled separately"),
        Node::Paragraph(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Blockquote(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(source, c, cx, highlight_theme))
                .collect();
            BlockNode::Blockquote {
                children,
                span: new_span(val.position, cx),
            }
        }
        Node::List(list) => {
            let children = list
                .children
                .into_iter()
                .map(|c| ast_to_node(source, c, cx, highlight_theme))
                .collect();
            BlockNode::List {
                ordered: list.ordered,
                children,
                span: new_span(list.position, cx),
            }
        }
        Node::ListItem(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(source, c, cx, highlight_theme))
                .collect();
            BlockNode::ListItem {
                children,
                spread: val.spread,
                checked: val.checked,
                span: new_span(val.position, cx),
            }
        }
        Node::Break(val) => BlockNode::Break {
            html: false,
            span: new_span(val.position, cx),
        },
        Node::Code(raw) => BlockNode::CodeBlock(CodeBlock::new(
            raw.value.into(),
            raw.lang.map(|s| s.into()),
            highlight_theme,
            new_span(raw.position, cx),
        )),
        Node::Heading(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });

            BlockNode::Heading {
                level: val.depth,
                children: paragraph,
                span: new_span(val.position, cx),
            }
        }
        Node::Math(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            None,
            highlight_theme,
            new_span(val.position, cx),
        )),
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => BlockNode::Root {
                children: el.blocks,
                span: new_span(val.position, cx),
            },
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("error parsing html: {:#?}", err);
                }

                BlockNode::Paragraph(Paragraph::new(val.value))
            }
        },
        Node::MdxFlowExpression(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("mdx".into()),
            highlight_theme,
            new_span(val.position, cx),
        )),
        Node::Yaml(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("yml".into()),
            highlight_theme,
            new_span(val.position, cx),
        )),
        Node::Toml(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("toml".into()),
            highlight_theme,
            new_span(val.position, cx),
        )),
        Node::MdxJsxTextElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::MdxJsxFlowElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::ThematicBreak(val) => BlockNode::HorizontalRule {
            span: new_span(val.position, cx),
        },
        Node::Table(val) => {
            let mut table = Table::default();
            table.column_aligns = val
                .align
                .clone()
                .into_iter()
                .map(|align| align.into())
                .collect();
            val.children.iter().for_each(|c| {
                if let Node::TableRow(row) = c {
                    parse_table_row(&mut table, row, cx);
                }
            });
            table.span = new_span(val.position, cx);

            BlockNode::Table(table)
        }
        Node::FootnoteDefinition(def) => {
            let mut paragraph = Paragraph::default();
            let prefix = format!("[{}]: ", def.identifier);
            paragraph.push(InlineNode::new(&prefix).marks(vec![(
                0..prefix.len(),
                TextMark {
                    italic: true,
                    ..Default::default()
                },
            )]));

            def.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, cx);
            });
            paragraph.span = new_span(def.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Definition(def) => {
            cx.add_ref(
                def.identifier.clone().into(),
                LinkMark {
                    url: def.url.clone().into(),
                    identifier: Some(def.identifier.clone().into()),
                    title: def.title.clone().map(Into::into),
                },
            );

            BlockNode::Definition {
                identifier: def.identifier.clone().into(),
                url: def.url.clone().into(),
                title: def.title.clone().map(|s| s.into()),
                span: new_span(def.position, cx),
            }
        }
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported node: {:#?}", value);
            }
            BlockNode::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ParentElement;

    use crate::text::{MarkdownExtensions, MarkdownNode, MarkdownPlugin};

    #[test]
    fn test_nested_emphasis_merges_text_marks() {
        let mut cx = NodeContext::default();
        let document = parse(
            "This has **_bold and italic_** text.",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        let bold_italic = paragraph
            .children
            .iter()
            .find(|child| child.text.as_ref() == "bold and italic")
            .expect("expected emphasized text");

        assert!(
            bold_italic
                .marks
                .iter()
                .any(|(_, mark)| mark.bold && mark.italic),
            "nested emphasis should produce a bold and italic mark"
        );
    }

    #[test]
    fn parses_mentions_and_slash_commands_in_markdown() {
        let mut cx = NodeContext::default();
        let document = parse(
            "Hello @Sonnet and /team-brainstorm!",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.children.len(), 1, "one run with sparse marks");
        let node = &paragraph.children[0];
        assert_eq!(node.text.as_ref(), "Hello @Sonnet and /team-brainstorm!");

        let mention_range = "Hello ".len().."Hello @Sonnet".len();
        let (_, mention_mark) = node
            .marks
            .iter()
            .find(|(range, _)| *range == mention_range)
            .expect("expected a mark over @Sonnet");
        assert!(mention_mark.bold);
        assert_eq!(
            mention_mark.color,
            crate::try_parse_color(MENTION_COLOR_NAME).ok()
        );

        let command_start = "Hello @Sonnet and ".len();
        let command_range = command_start.."Hello @Sonnet and /team-brainstorm".len();
        let (_, command_mark) = node
            .marks
            .iter()
            .find(|(range, _)| *range == command_range)
            .expect("expected a mark over /team-brainstorm");
        assert!(command_mark.bold);
        assert_eq!(
            command_mark.color,
            crate::try_parse_color(COMMAND_COLOR_NAME).ok()
        );
    }

    #[test]
    fn parses_file_mentions_in_markdown() {
        let mut cx = NodeContext::default();
        let document = parse(
            "Check @README.md and @src/main.rs for info.",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let node = &paragraph.children[0];

        let readme_range = "Check ".len().."Check @README.md".len();
        let (_, readme_mark) = node
            .marks
            .iter()
            .find(|(range, _)| *range == readme_range)
            .expect("expected a mark over @README.md");
        assert!(readme_mark.bold);
        assert_eq!(
            readme_mark.color,
            crate::try_parse_color(FILE_MENTION_COLOR_NAME).ok()
        );

        let src_start = "Check @README.md and ".len();
        let src_range = src_start.."Check @README.md and @src/main.rs".len();
        let (_, src_mark) = node
            .marks
            .iter()
            .find(|(range, _)| *range == src_range)
            .expect("expected a mark over @src/main.rs");
        assert!(src_mark.bold);
        assert_eq!(
            src_mark.color,
            crate::try_parse_color(FILE_MENTION_COLOR_NAME).ok()
        );
    }

    #[test]
    fn a_slash_inside_a_file_path_is_not_a_command() {
        let mut cx = NodeContext::default();
        let document = parse(
            "See src/app.rs for details.",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            paragraph.children.iter().all(|n| n.marks.is_empty()),
            "a mid-word slash in a file path must not be marked as a command"
        );
    }

    #[test]
    fn test_bold_text_with_inline_code_and_mentions() {
        let mut cx = NodeContext::default();
        let document = parse(
            "This is **bold text with `code` and @Agy** here.",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        assert_eq!(paragraph.children.len(), 3);
        let bold_node = &paragraph.children[1];
        assert_eq!(bold_node.text, "bold text with code and @Agy");
        // Must contain bold mark covering the entire range
        assert!(bold_node.marks.iter().any(|(range, mark)| mark.bold && *range == (0..28)));
    }

    #[test]
    fn an_at_sign_inside_an_email_is_not_a_mention() {
        let mut cx = NodeContext::default();
        let document = parse(
            "Reach me at jake@example.com anytime.",
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let mention_color = crate::try_parse_color(MENTION_COLOR_NAME).ok();
        assert!(
            paragraph
                .children
                .iter()
                .flat_map(|n| &n.marks)
                .all(|(_, mark)| mark.color != mention_color),
            "an `@` preceded by an alphanumeric must not be marked as a mention: {:#?}",
            paragraph.children
        );
    }

    #[test]
    fn test_inline_html_image_stays_in_markdown_paragraph() {
        let mut cx = NodeContext::default();
        let document = parse(
            r#"Before <img src="https://example.com/avatar.png" alt="Avatar" width="32" height="32" /> after."#,
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        assert_eq!(paragraph.children.len(), 3);
        assert_eq!(paragraph.children[0].text.as_ref(), "Before ");
        assert_eq!(paragraph.children[2].text.as_ref(), " after.");

        let image = paragraph.children[1]
            .image
            .as_ref()
            .expect("expected inline html image");
        assert_eq!(image.url.as_ref(), "https://example.com/avatar.png");
        assert_eq!(image.width, Some(gpui::px(32.).into()));
        assert_eq!(image.height, Some(gpui::px(32.).into()));
    }

    #[test]
    fn test_inline_html_image_without_size_stays_in_markdown_paragraph() {
        let mut cx = NodeContext::default();
        let document = parse(
            r#"Before <img src="https://avatars.githubusercontent.com/u/5518"> after."#,
            &mut cx,
            &HighlightTheme::default_light(),
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        assert_eq!(paragraph.children.len(), 3);
        assert_eq!(paragraph.children[0].text.as_ref(), "Before ");
        assert_eq!(paragraph.children[2].text.as_ref(), " after.");

        let image = paragraph.children[1]
            .image
            .as_ref()
            .expect("expected inline html image");
        assert_eq!(
            image.url.as_ref(),
            "https://avatars.githubusercontent.com/u/5518"
        );
        assert_eq!(image.width, None);
        assert_eq!(image.height, None);
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Ticker {
        symbol: String,
    }

    fn parse_ticker_block(node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Paragraph(paragraph) = node else {
            return None;
        };
        let [Node::Text(text)] = paragraph.children.as_slice() else {
            return None;
        };
        let symbol = text.value.strip_prefix('$')?.to_string();
        let node_text = format!("${symbol}");

        Some(
            MarkdownNode::new("ticker", Ticker { symbol })
                .text(node_text)
                .markdown(cx.node_source(node).unwrap_or_default()),
        )
    }

    #[test]
    fn custom_block_parser_converts_ticker_syntax_to_custom_node() {
        let extensions = MarkdownExtensions::default().block_parser(parse_ticker_block);

        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let document = parse("$TSLA.US", &mut cx, &HighlightTheme::default_light()).unwrap();

        let BlockNode::Custom(node) = &document.blocks[0] else {
            panic!("expected custom markdown node");
        };
        assert_eq!(node.name(), "ticker");
        assert_eq!(node.as_text(), "$TSLA.US");
        assert_eq!(node.as_markdown(), "$TSLA.US");
        assert_eq!(
            node.data::<Ticker>(),
            Some(&Ticker {
                symbol: "TSLA.US".to_string()
            })
        );
        assert_eq!(document.text(), "$TSLA.US\n");
        assert_eq!(document.to_markdown(), "$TSLA.US");
    }

    struct TickerPlugin {
        name: &'static str,
    }

    impl TickerPlugin {
        fn new(name: &'static str) -> Self {
            Self { name }
        }
    }

    impl MarkdownPlugin for TickerPlugin {
        fn is_block(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            self.name
        }

        fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
            parse_ticker_block(node, cx)
        }

        fn render(
            &self,
            node: &MarkdownNode,
            _window: &mut gpui::Window,
            _cx: &mut gpui::App,
        ) -> impl gpui::IntoElement {
            gpui::div().child(node.as_text().to_string())
        }
    }

    #[test]
    fn custom_block_plugin_registers_parser_and_renderer() {
        let extensions = MarkdownExtensions::default().plugin(TickerPlugin::new("ticker"));

        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let document = parse("$TSLA.US", &mut cx, &HighlightTheme::default_light()).unwrap();

        let BlockNode::Custom(node) = &document.blocks[0] else {
            panic!("expected custom markdown node");
        };
        assert_eq!(node.name(), "ticker");
        assert_eq!(
            node.data::<Ticker>(),
            Some(&Ticker {
                symbol: "TSLA.US".to_string()
            })
        );
    }
}
