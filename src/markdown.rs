/// Splits assistant markdown into plain-text and fenced code segments.
pub enum Segment {
    Text(String),
    Code {
        lang: String,
        code: String,
        /// False while the closing fence has not arrived (live streaming).
        closed: bool,
    },
}

pub fn split_fenced(input: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut code = String::new();
    let mut lang = String::new();
    let mut in_code = false;

    let flush_text = |text: &mut String, segments: &mut Vec<Segment>| {
        if !text.is_empty() {
            segments.push(Segment::Text(std::mem::take(text)));
        }
    };

    for line in input.lines() {
        let trimmed = line.trim_start();
        if !in_code && trimmed.starts_with("```") {
            flush_text(&mut text, &mut segments);
            in_code = true;
            lang = trimmed.trim_start_matches('`').trim().to_owned();
            code.clear();
            continue;
        }
        if in_code && trimmed.starts_with("```") && trimmed.trim_start_matches('`').trim().is_empty()
        {
            segments.push(Segment::Code {
                lang: std::mem::take(&mut lang),
                code: std::mem::take(&mut code),
                closed: true,
            });
            in_code = false;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }

    if in_code {
        segments.push(Segment::Code {
            lang,
            code,
            closed: false,
        });
    } else {
        flush_text(&mut text, &mut segments);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .map(|s| match s {
                Segment::Text(_) => "text",
                Segment::Code { .. } => "code",
            })
            .collect()
    }

    #[test]
    fn plain_text_stays_single_segment() {
        let segs = split_fenced("just a line\nanother line");
        assert_eq!(kinds(&segs), vec!["text"]);
        match &segs[0] {
            Segment::Text(t) => assert_eq!(t, "just a line\nanother line\n"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn fenced_block_with_language() {
        let input = "before\n```python\nprint(1)\nprint(2)\n```\nafter";
        let segs = split_fenced(input);
        assert_eq!(kinds(&segs), vec!["text", "code", "text"]);
        match &segs[1] {
            Segment::Code { lang, code, closed } => {
                assert_eq!(lang, "python");
                assert_eq!(code, "print(1)\nprint(2)\n");
                assert!(closed);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn unterminated_fence_is_streaming_code() {
        let segs = split_fenced("text\n```bash\nrustup update");
        assert_eq!(kinds(&segs), vec!["text", "code"]);
        match &segs[1] {
            Segment::Code { lang, code, closed } => {
                assert_eq!(lang, "bash");
                assert_eq!(code, "rustup update\n");
                assert!(!closed);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn fence_without_language() {
        let segs = split_fenced("```\ncode\n```");
        assert_eq!(kinds(&segs), vec!["code"]);
        match &segs[0] {
            Segment::Code { lang, code, .. } => {
                assert_eq!(lang, "");
                assert_eq!(code, "code\n");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn empty_text_segments_are_skipped() {
        let segs = split_fenced("```js\nlet x = 1;\n```");
        assert_eq!(kinds(&segs), vec!["code"]);
    }

    #[test]
    fn code_fences_inside_code_are_literal() {
        let input = "```text\n```not a fence\n```\ndone";
        let segs = split_fenced(input);
        assert_eq!(kinds(&segs), vec!["code", "text"]);
        match &segs[0] {
            Segment::Code { code, .. } => assert_eq!(code, "```not a fence\n"),
            _ => unreachable!(),
        }
    }
}
