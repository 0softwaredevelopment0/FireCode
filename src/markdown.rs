//! Minimal markdown: block structure (headings, lists, fenced code,
//! paragraphs) and inline styles (**bold**, *italic*, `code`).
//!
//! Default egui fonts have no bold/italic glyphs, so inline styles are
//! conveyed by size, color and monospace backgrounds at render time.

pub enum Block {
    Heading { level: u8, text: String },
    BulletItem(String),
    OrderedItem { num: u32, text: String },
    Code {
        lang: String,
        code: String,
        /// False while the closing fence has not arrived (live streaming).
        closed: bool,
    },
    Paragraph(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSpan {
    Plain(String),
    Bold(String),
    Italic(String),
    Code(String),
}

pub fn parse_blocks(input: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();

    let flush_paragraph = |paragraph: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(paragraph.join("\n")));
            paragraph.clear();
        }
    };

    let mut in_code = false;
    let mut code = String::new();
    let mut lang = String::new();

    for line in input.lines() {
        let trimmed = line.trim_start();
        if !in_code && trimmed.starts_with("```") {
            flush_paragraph(&mut paragraph, &mut blocks);
            in_code = true;
            lang = trimmed.trim_start_matches('`').trim().to_owned();
            code.clear();
            continue;
        }
        if in_code {
            if trimmed.starts_with("```") && trimmed.trim_start_matches('`').trim().is_empty() {
                blocks.push(Block::Code {
                    lang: std::mem::take(&mut lang),
                    code: std::mem::take(&mut code),
                    closed: true,
                });
                in_code = false;
            } else {
                code.push_str(line);
                code.push('\n');
            }
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
            continue;
        }

        let heading_level = heading_level(trimmed);
        if heading_level > 0 {
            flush_paragraph(&mut paragraph, &mut blocks);
            let text = trimmed[heading_level as usize..].trim_start().to_owned();
            blocks.push(Block::Heading {
                level: heading_level,
                text,
            });
            continue;
        }

        if let Some(text) = trimmed.strip_prefix("- ").or(trimmed.strip_prefix("* ")) {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(Block::BulletItem(text.to_owned()));
            continue;
        }

        if let Some((num, text)) = ordered_item(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(Block::OrderedItem { num, text });
            continue;
        }

        paragraph.push(line.to_owned());
    }

    if in_code {
        blocks.push(Block::Code {
            lang,
            code,
            closed: false,
        });
    } else {
        flush_paragraph(&mut paragraph, &mut blocks);
    }
    blocks
}

fn heading_level(line: &str) -> u8 {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && line[hashes..].starts_with(' ')
        && !line[..hashes].is_empty()
    {
        hashes as u8
    } else {
        0
    }
}

fn ordered_item(line: &str) -> Option<(u32, String)> {
    let (digits, rest) = line.split_once('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: u32 = digits.parse().ok()?;
    let text = rest.trim_start();
    if text.is_empty() {
        return None;
    }
    Some((num, text.to_owned()))
}

/// Parses inline markdown into styled spans. Unsupported or unterminated
/// markers stay as literal text.
pub fn parse_inline(input: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    let flush_plain = |plain: &mut String, spans: &mut Vec<InlineSpan>| {
        if !plain.is_empty() {
            spans.push(InlineSpan::Plain(std::mem::take(plain)));
        }
    };

    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                if let Some(close) = input[i + 1..].find('`') {
                    let end = i + 1 + close;
                    flush_plain(&mut plain, &mut spans);
                    spans.push(InlineSpan::Code(input[i + 1..end].to_owned()));
                    i = end + 1;
                } else {
                    plain.push('`');
                    i += 1;
                }
            }
            b'*' if input[i..].starts_with("**") => {
                if let Some(close) = input[i + 2..].find("**") {
                    let end = i + 2 + close;
                    if !input[i + 2..end].is_empty() {
                        flush_plain(&mut plain, &mut spans);
                        spans.push(InlineSpan::Bold(input[i + 2..end].to_owned()));
                    } else {
                        // empty bold: keep everything consumed as literal
                        plain.push_str(&input[i..end + 2]);
                    }
                    i = end + 2;
                } else {
                    plain.push_str("**");
                    i += 2;
                }
            }
            b'*' => {
                // single asterisk: italic only if a closing one exists and the
                // char after the opening is not a space
                let inner_starts_normal = !input[i + 1..].starts_with([' ', '*']);
                if inner_starts_normal {
                    if let Some(close) = input[i + 1..].find('*') {
                        let end = i + 1 + close;
                        if !input[i + 1..end].is_empty() {
                            flush_plain(&mut plain, &mut spans);
                            spans.push(InlineSpan::Italic(input[i + 1..end].to_owned()));
                            i = end + 1;
                            continue;
                        }
                    }
                }
                plain.push('*');
                i += 1;
            }
            _ => {
                let run_end = input[i..]
                    .find(['`', '*'])
                    .map_or(input.len(), |pos| i + pos);
                plain.push_str(&input[i..run_end]);
                i = run_end;
            }
        }
    }
    flush_plain(&mut plain, &mut spans);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| match b {
                Block::Heading { level, .. } => format!("heading{level}"),
                Block::BulletItem(_) => "bullet".to_owned(),
                Block::OrderedItem { .. } => "ordered".to_owned(),
                Block::Code { .. } => "code".to_owned(),
                Block::Paragraph(_) => "paragraph".to_owned(),
            })
            .collect()
    }

    #[test]
    fn heading_bullets_and_paragraph() {
        let input = "## Что внутри\n- первый пункт\n- `код` — тире\n\nОбычный текст.";
        let blocks = parse_blocks(input);
        assert_eq!(
            kinds(&blocks),
            vec!["heading2", "bullet", "bullet", "paragraph"]
        );
        match &blocks[0] {
            Block::Heading { level, text } => {
                assert_eq!(*level, 2);
                assert_eq!(text, "Что внутри");
            }
            _ => unreachable!(),
        }
        match &blocks[2] {
            Block::BulletItem(text) => assert!(text.starts_with("`код`")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn bold_marker_is_not_a_bullet() {
        let blocks = parse_blocks("**жирный** текст");
        assert_eq!(kinds(&blocks), vec!["paragraph"]);
    }

    #[test]
    fn ordered_list() {
        let blocks = parse_blocks("1. раз\n2. два\n12. двенадцать");
        assert_eq!(kinds(&blocks), vec!["ordered", "ordered", "ordered"]);
        match &blocks[2] {
            Block::OrderedItem { num, text } => {
                assert_eq!(*num, 12);
                assert_eq!(text, "двенадцать");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn fenced_code_with_language() {
        let input = "before\n```python\nprint(1)\n```\nafter";
        let blocks = parse_blocks(input);
        assert_eq!(
            kinds(&blocks),
            vec!["paragraph", "code", "paragraph"]
        );
        match &blocks[1] {
            Block::Code { lang, code, closed } => {
                assert_eq!(lang, "python");
                assert_eq!(code, "print(1)\n");
                assert!(closed);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn unterminated_fence_is_streaming_code() {
        let blocks = parse_blocks("text\n```bash\nrustup update");
        assert_eq!(kinds(&blocks), vec!["paragraph", "code"]);
        match &blocks[1] {
            Block::Code { closed, .. } => assert!(!closed),
            _ => unreachable!(),
        }
    }

    #[test]
    fn inline_styles() {
        let spans = parse_inline("a **bold** and `code` and *it* end");
        assert_eq!(
            spans,
            vec![
                InlineSpan::Plain("a ".to_owned()),
                InlineSpan::Bold("bold".to_owned()),
                InlineSpan::Plain(" and ".to_owned()),
                InlineSpan::Code("code".to_owned()),
                InlineSpan::Plain(" and ".to_owned()),
                InlineSpan::Italic("it".to_owned()),
                InlineSpan::Plain(" end".to_owned()),
            ]
        );
    }

    #[test]
    fn unterminated_markers_stay_literal() {
        let spans = parse_inline("a **b and c *d");
        assert_eq!(
            spans,
            vec![InlineSpan::Plain("a **b and c *d".to_owned())]
        );
    }

    #[test]
    fn empty_bold_stays_literal() {
        let spans = parse_inline("x **** y");
        assert_eq!(spans, vec![InlineSpan::Plain("x **** y".to_owned())]);
    }
}
