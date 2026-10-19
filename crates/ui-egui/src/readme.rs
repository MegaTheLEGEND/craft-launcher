//! A third-party app's README, shown as readable text without a Markdown engine.
//!
//! The text comes from someone ArtCraft doesn't vouch for, so it is only ever drawn as plain
//! text: no images, no HTML, no clickable links.

use egui::{RichText, Ui};

use crate::theme::Tokens;

/// Where a README is: asked for, here, or not to be had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Readme {
    Loading,
    Ready(String),
    Failed(String),
}

/// One block of a README, classified.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Block {
    Heading(u8, String),
    Paragraph(String),
    Bullet(String),
    Code(String),
}

/// The most blocks drawn: a huge README must not make the dialog crawl.
const MAX_BLOCKS: usize = 600;

/// The README's blocks: headings, paragraphs (soft line breaks joined), bullets and code.
/// Images, badges, HTML and table rules are dropped.
pub(crate) fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut para = String::new();
    let mut code: Option<String> = None;
    let flush = |para: &mut String, out: &mut Vec<Block>| {
        if !para.trim().is_empty() {
            out.push(Block::Paragraph(std::mem::take(para).trim().to_string()));
        }
        para.clear();
    };
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            match code.take() {
                Some(body) => out.push(Block::Code(body.trim_end().to_string())),
                None => {
                    flush(&mut para, &mut out);
                    code = Some(String::new());
                }
            }
            continue;
        }
        if let Some(body) = code.as_mut() {
            if body.len() < 20_000 {
                body.push_str(line);
                body.push('\n');
            }
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut para, &mut out);
            continue;
        }
        // Raw HTML (<img>, <p align=…>, comments) and image or badge lines say nothing as text.
        if trimmed.starts_with('<') || trimmed.starts_with("![") || trimmed.starts_with("[![") {
            continue;
        }
        if trimmed.starts_with('#') {
            flush(&mut para, &mut out);
            let level = trimmed.bytes().take_while(|b| *b == b'#').count().min(6) as u8;
            let title = inline(trimmed.trim_start_matches('#'));
            if !title.is_empty() {
                out.push(Block::Heading(level, title));
            }
            continue;
        }
        // A table row, a rule line or the `---|---` under a table head.
        if trimmed.starts_with('|') || trimmed.chars().all(|c| matches!(c, '-' | '=' | '*' | '_' | ' ' | ':' | '|')) {
            if trimmed.chars().all(|c| matches!(c, '-' | '=' | '*' | '_' | ' ' | ':' | '|')) {
                flush(&mut para, &mut out);
                continue;
            }
            flush(&mut para, &mut out);
            let cells: Vec<String> = trimmed.trim_matches('|').split('|').map(inline).filter(|c| !c.is_empty()).collect();
            out.push(Block::Paragraph(cells.join("  ·  ")));
            continue;
        }
        let bullet = ["- ", "* ", "+ "].iter().find_map(|p| trimmed.strip_prefix(p)).or_else(|| numbered(trimmed));
        if let Some(item) = bullet {
            flush(&mut para, &mut out);
            let item = inline(item.trim_start_matches("[ ] ").trim_start_matches("[x] "));
            if !item.is_empty() {
                out.push(Block::Bullet(item));
            }
            continue;
        }
        let text = inline(trimmed.trim_start_matches('>').trim_start());
        if !text.is_empty() {
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(&text);
        }
    }
    if let Some(body) = code {
        out.push(Block::Code(body.trim_end().to_string()));
    }
    flush(&mut para, &mut out);
    out.truncate(MAX_BLOCKS);
    out
}

/// `1. item` / `2) item`: the item.
fn numbered(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    line.get(digits..)?.strip_prefix(". ").or_else(|| line.get(digits..)?.strip_prefix(") "))
}

/// A line's text without Markdown marks: emphasis and code ticks dropped, `[text](url)` as
/// `text`, images and HTML tags gone.
pub(crate) fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // An image `![alt](url)`: nothing.
            '!' if chars.get(i + 1) == Some(&'[') => match link_end(&chars, i + 1) {
                Some((_, end)) => i = end,
                None => {
                    out.push(c);
                    i += 1;
                    continue;
                }
            },
            // A link `[text](url)`: its text.
            '[' => match link_end(&chars, i) {
                Some((text, end)) => {
                    out.push_str(&inline(&text));
                    i = end;
                }
                None => {
                    out.push(c);
                    i += 1;
                    continue;
                }
            },
            // An HTML tag: dropped (its text stays).
            '<' => match chars[i..].iter().position(|&x| x == '>') {
                Some(n) if n < 200 && chars.get(i + 1).is_some_and(|x| x.is_alphabetic() || matches!(x, '/' | '!')) => i += n + 1,
                _ => {
                    out.push(c);
                    i += 1;
                    continue;
                }
            },
            '`' | '*' => i += 1,
            '_' if chars.get(i + 1) == Some(&'_') => i += 2,
            _ => {
                out.push(c);
                i += 1;
                continue;
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// For `[text](url)` starting at `start` (the `[`): the text and the index after the `)`.
fn link_end(chars: &[char], start: usize) -> Option<(String, usize)> {
    let close = start + chars.get(start..)?.iter().position(|&c| c == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let paren = close + 2 + chars.get(close + 2..)?.iter().position(|&c| c == ')')?;
    Some((chars.get(start + 1..close)?.iter().collect(), paren + 1))
}

/// Draws a README at `scale` times the usual text size.
pub(crate) fn show(ui: &mut Ui, text: &str, scale: f32) {
    let t = Tokens::get(ui.ctx());
    let size = 14.5 * scale;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 8.0 * scale;
        let all = blocks(text);
        if all.is_empty() {
            ui.label(RichText::new(tl!("This README is empty.")).color(t.muted));
            return;
        }
        for block in all {
            match block {
                Block::Heading(level, title) => {
                    let h = match level {
                        1 => 24.0,
                        2 => 20.0,
                        3 => 17.0,
                        _ => 15.5,
                    } * scale;
                    ui.add_space(6.0 * scale);
                    ui.add(egui::Label::new(RichText::new(title).size(h).strong().color(t.ink_strong)).wrap());
                }
                Block::Paragraph(p) => {
                    ui.add(egui::Label::new(RichText::new(p).size(size).color(t.ink_strong)).wrap());
                }
                Block::Bullet(item) => {
                    ui.horizontal_top(|ui| {
                        ui.add_space(8.0 * scale);
                        ui.label(RichText::new("•").size(size).color(t.muted));
                        ui.add(egui::Label::new(RichText::new(item).size(size).color(t.ink_strong)).wrap());
                    });
                }
                Block::Code(code) => {
                    egui::Frame::new().stroke(t.hairline()).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(code).monospace().size(size - 1.0).color(t.muted)).wrap());
                    });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_links_images_and_html_are_dropped() {
        assert_eq!(inline("**Fast** and `small`, see [the docs](https://x.y/z)."), "Fast and small, see the docs.");
        assert_eq!(inline("a ![logo](l.png) b"), "a  b".split_whitespace().collect::<Vec<_>>().join(" "));
        assert_eq!(inline("<b>bold</b> 1 < 2"), "bold 1 < 2");
        assert_eq!(inline("[broken](no close"), "[broken](no close");
    }

    #[test]
    fn a_readme_becomes_blocks() {
        let text = "# Tool\n\n[![ci](b.svg)](x)\n<p align=center><img src=a.png></p>\n\nA small\ntool.\n\n- one\n* two\n1. three\n\n```sh\ncargo run\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(
            blocks(text),
            vec![
                Block::Heading(1, "Tool".into()),
                Block::Paragraph("A small tool.".into()),
                Block::Bullet("one".into()),
                Block::Bullet("two".into()),
                Block::Bullet("three".into()),
                Block::Code("cargo run".into()),
                Block::Paragraph("a  ·  b".into()),
                Block::Paragraph("1  ·  2".into()),
            ]
        );
    }

    #[test]
    fn an_unclosed_code_fence_and_huge_input_are_handled() {
        assert_eq!(blocks("```\nlet x = 1;"), vec![Block::Code("let x = 1;".into())]);
        let many = "- x\n".repeat(5000);
        assert_eq!(blocks(&many).len(), MAX_BLOCKS);
        assert!(blocks("").is_empty());
    }
}
