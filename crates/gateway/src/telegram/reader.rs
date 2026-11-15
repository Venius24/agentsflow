//! Читалка: текст и Markdown превращаются в страницы для Telegram.
//! Каждая страница сама по себе правильный HTML: теги закрыты в её пределах,
//! а блок кода, разорванный страницей, закрывается и открывается заново.

/// Telegram режет на 4096 единицах UTF-16, остальное занимает шапка экрана.
const PAGE_LIMIT: usize = 3200;
/// Самая длинная строка: даже из одних `&quot;` она влезает в страницу.
const LINE_LIMIT: usize = 500;
/// Шире этого таблица на телефоне переносится и не читается: выводим карточками.
const TABLE_WIDTH: usize = 38;

enum Piece {
    Text { html: String, heading: bool },
    Code(String),
}

pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Страницы текста; файл без разметки показывается как есть, моноширинно.
pub(super) fn pages(text: &str, markdown: bool) -> Vec<String> {
    pages_for(crate::i18n::default_lang(), text, markdown)
}

pub(super) fn pages_for(lang: crate::i18n::Lang, text: &str, markdown: bool) -> Vec<String> {
    let pieces = if markdown {
        markdown_pieces(text)
    } else {
        text.lines()
            .flat_map(split_long)
            .map(|line| Piece::Code(escape(&line)))
            .collect()
    };
    paginate(lang, pieces)
}

/// Режет длинную строку, по возможности на пробеле.
fn split_long(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= LINE_LIMIT {
        return vec![line.to_owned()];
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while chars.len() - start > LINE_LIMIT {
        let window = &chars[start..start + LINE_LIMIT];
        let cut = window
            .iter()
            .rposition(|c| c.is_whitespace())
            .filter(|at| *at > LINE_LIMIT / 2)
            .map_or(LINE_LIMIT, |at| at + 1);
        parts.push(chars[start..start + cut].iter().collect());
        start += cut;
    }
    parts.push(chars[start..].iter().collect());
    parts
}

fn size(text: &str) -> usize {
    text.encode_utf16().count()
}

fn paginate(lang: crate::i18n::Lang, pieces: Vec<Piece>) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = String::new();
    let mut in_code = false;
    let close = |page: &mut String, in_code: &mut bool, pages: &mut Vec<String>| {
        if *in_code {
            page.push_str("</pre>");
            *in_code = false;
        }
        let done = page.trim();
        if !done.is_empty() {
            pages.push(done.to_owned());
        }
        page.clear();
    };
    for piece in pieces {
        let added = match &piece {
            Piece::Text { html, .. } | Piece::Code(html) => size(html) + 1,
        };
        let full = size(&page) + added > PAGE_LIMIT;
        // Раздел лучше начать с новой страницы, если эта уже заполнена.
        let heading =
            matches!(piece, Piece::Text { heading: true, .. }) && size(&page) > PAGE_LIMIT * 3 / 5;
        if full || heading {
            close(&mut page, &mut in_code, &mut pages);
        }
        match piece {
            Piece::Text { html, .. } => {
                if in_code {
                    page.push_str("</pre>\n");
                    in_code = false;
                }
                // Не больше одной пустой строки подряд.
                if !(html.is_empty() && (page.is_empty() || page.ends_with("\n\n"))) {
                    page.push_str(&html);
                    page.push('\n');
                }
            }
            Piece::Code(line) => {
                if !in_code {
                    page.push_str("<pre>");
                    in_code = true;
                }
                page.push_str(&line);
                page.push('\n');
            }
        }
    }
    close(&mut page, &mut in_code, &mut pages);
    if pages.is_empty() {
        pages.push(crate::i18n::tr(lang, "(пусто)").to_owned());
    }
    pages
}

fn text_piece(html: String) -> Piece {
    Piece::Text {
        html,
        heading: false,
    }
}

fn markdown_pieces(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut fence: Option<&str> = None;
    let mut table: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(mark) = fence {
            if trimmed.starts_with(mark) {
                fence = None;
            } else {
                pieces.extend(
                    split_long(line)
                        .iter()
                        .map(|part| Piece::Code(escape(part))),
                );
            }
            continue;
        }
        if trimmed.starts_with('|') {
            table.push(trimmed);
            continue;
        }
        flush_table(&mut table, &mut pieces);
        if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        } else {
            for part in split_long(line) {
                pieces.push(markdown_line(&part));
            }
        }
    }
    flush_table(&mut table, &mut pieces);
    pieces
}

fn markdown_line(line: &str) -> Piece {
    let trimmed = line.trim();
    let indent = "  ".repeat((line.len() - line.trim_start().len()) / 2);
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
        let title = trimmed[level..].trim().trim_end_matches('#').trim_end();
        return Piece::Text {
            html: format!("<b>{}</b>", inline(title)),
            heading: true,
        };
    }
    if trimmed.len() >= 3
        && ["-", "*", "_"].iter().any(|mark| {
            trimmed
                .replace(' ', "")
                .chars()
                .all(|c| c.to_string() == *mark)
        })
    {
        return text_piece("———".to_owned());
    }
    if let Some(quote) = trimmed.strip_prefix('>') {
        return text_piece(format!("▎ <i>{}</i>", inline(quote.trim())));
    }
    for mark in ["- ", "* ", "+ "] {
        if let Some(item) = trimmed.strip_prefix(mark) {
            let html = match item.get(..4) {
                Some("[ ] ") => format!("☐ {}", inline(&item[4..])),
                Some("[x] " | "[X] ") => format!("☑ {}", inline(&item[4..])),
                _ => format!("• {}", inline(item)),
            };
            return text_piece(format!("{indent}{html}"));
        }
    }
    text_piece(format!("{indent}{}", inline(trimmed)))
}

fn table_cells(row: &str) -> Vec<String> {
    let row = row.trim().trim_start_matches('|').trim_end_matches('|');
    row.split('|').map(|cell| cell.trim().to_owned()).collect()
}

fn flush_table(table: &mut Vec<&str>, pieces: &mut Vec<Piece>) {
    if table.is_empty() {
        return;
    }
    let rows: Vec<Vec<String>> = table
        .drain(..)
        .map(table_cells)
        .filter(|cells| {
            !cells
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
        })
        .collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let plain = |cell: &str| cell.replace("**", "").replace('`', "");
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| plain(cell).chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    if widths.iter().sum::<usize>() + 2 * columns.saturating_sub(1) <= TABLE_WIDTH {
        for row in &rows {
            let cells: Vec<String> = (0..columns)
                .map(|column| {
                    let cell = plain(row.get(column).map_or("", String::as_str));
                    let pad = widths[column] - cell.chars().count();
                    format!("{cell}{}", " ".repeat(pad))
                })
                .collect();
            pieces.push(Piece::Code(escape(cells.join("  ").trim_end())));
        }
    } else {
        // Строка становится карточкой: первая колонка заголовком, остальные
        // с названиями колонок из шапки.
        let Some((head, body)) = rows.split_first() else {
            return;
        };
        for row in body {
            let mut cells = row.iter().enumerate();
            let title = cells.next().map_or("", |(_, cell)| cell.as_str());
            pieces.push(text_piece(format!("<b>{}</b>", inline(title))));
            for (column, cell) in cells.filter(|(_, cell)| !cell.is_empty()) {
                let name = head.get(column).map_or("", String::as_str);
                pieces.push(text_piece(format!("{}: {}", inline(name), inline(cell))));
            }
            pieces.push(text_piece(String::new()));
        }
    }
    pieces.push(text_piece(String::new()));
}

fn find(chars: &[char], from: usize, mark: &[char]) -> Option<usize> {
    (from..=chars.len().checked_sub(mark.len())?).find(|at| chars[*at..].starts_with(mark))
}

/// Разметка внутри строки. Что не закрыто, остаётся обычным текстом, поэтому
/// теги всегда парные, а `snake_case` и `2*3*4` остаются как написаны.
fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    let wrap = |tag: &str, inner: &[char]| {
        let inner: String = inner.iter().collect();
        format!("<{tag}>{}</{tag}>", inline(&inner))
    };
    while at < chars.len() {
        let c = chars[at];
        if c == '`'
            && let Some(end) = find(&chars, at + 1, &['`']).filter(|end| *end > at + 1)
        {
            let code: String = chars[at + 1..end].iter().collect();
            out.push_str(&format!("<code>{}</code>", escape(&code)));
            at = end + 1;
            continue;
        }
        let double = match (c, chars.get(at + 1)) {
            ('*', Some('*')) | ('_', Some('_')) => Some("b"),
            ('~', Some('~')) => Some("s"),
            _ => None,
        };
        if let Some(tag) = double
            && let Some(end) = find(&chars, at + 2, &[c, c]).filter(|end| *end > at + 2)
        {
            out.push_str(&wrap(tag, &chars[at + 2..end]));
            at = end + 2;
            continue;
        }
        if matches!(c, '*' | '_')
            && !at
                .checked_sub(1)
                .is_some_and(|p| chars[p].is_alphanumeric())
            && chars.get(at + 1).is_some_and(|n| !n.is_whitespace())
        {
            let end = (at + 2..chars.len()).find(|end| {
                chars[*end] == c
                    && !chars[*end - 1].is_whitespace()
                    && !chars.get(*end + 1).is_some_and(|n| n.is_alphanumeric())
            });
            if let Some(end) = end {
                out.push_str(&wrap("i", &chars[at + 1..end]));
                at = end + 1;
                continue;
            }
        }
        if c == '['
            && let Some(close) = find(&chars, at + 1, &[']', '('])
            && let Some(end) = find(&chars, close + 2, &[')'])
        {
            let url: String = chars[close + 2..end].iter().collect();
            if url.starts_with("http://") || url.starts_with("https://") {
                let inner: String = chars[at + 1..close].iter().collect();
                out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    escape(&url),
                    inline(&inner)
                ));
                at = end + 1;
                continue;
            }
        }
        out.push_str(&escape(&c.to_string()));
        at += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Теги страницы парные и вложены правильно: иначе Telegram отвергнет её.
    fn balanced(html: &str) -> bool {
        let mut stack = Vec::new();
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            let Some(close) = rest[open..].find('>') else {
                return false;
            };
            let tag = &rest[open + 1..open + close];
            if let Some(name) = tag.strip_prefix('/') {
                if stack.pop() != Some(name.to_owned()) {
                    return false;
                }
            } else {
                stack.push(tag.split(' ').next().unwrap_or("").to_owned());
            }
            rest = &rest[open + close + 1..];
        }
        stack.is_empty()
    }

    #[test]
    fn markdown_becomes_telegram_html() {
        let page = pages(
            "# Итог\n\n**Важно** и *так*, `код <1>`, snake_case_name, 2*3*4\n- пункт\n- [x] готово\n> цитата\n---\n[сайт](https://a.kz/?x=1&y=2) [плохая](javascript:alert)\n```\nlet a = 1 < 2;\n```",
            true,
        )
        .join("");
        assert!(page.contains("<b>Итог</b>"));
        assert!(page.contains("<b>Важно</b> и <i>так</i>"));
        assert!(page.contains("<code>код &lt;1&gt;</code>"));
        assert!(page.contains("snake_case_name, 2*3*4"));
        assert!(page.contains("• пункт"));
        assert!(page.contains("☑ готово"));
        assert!(page.contains("▎ <i>цитата</i>"));
        assert!(page.contains("———"));
        assert!(page.contains("<a href=\"https://a.kz/?x=1&amp;y=2\">сайт</a>"));
        assert!(page.contains("[плохая](javascript:alert)"));
        assert!(page.contains("<pre>let a = 1 &lt; 2;\n</pre>"));
        assert!(balanced(&page));
    }

    #[test]
    fn broken_markup_stays_plain_and_html_is_escaped() {
        let page = pages("**нет конца и <script> *тоже `и так", true).join("");
        assert_eq!(page, "**нет конца и &lt;script&gt; *тоже `и так");
    }

    #[test]
    fn every_page_fits_telegram_and_is_balanced() {
        let mut text = String::from("# Начало\n```\n");
        for line in 0..2000 {
            text.push_str(&format!("код {line} <тег> 😀 & ещё\n"));
        }
        text.push_str("```\n");
        text.push_str(&"&\"<>".repeat(5000));
        text.push('\n');
        for line in 0..2000 {
            text.push_str(&format!("**строка** {line} со *смыслом* и 😀\n\n"));
        }
        for markdown in [true, false] {
            let pages = pages(&text, markdown);
            assert!(pages.len() > 5);
            for page in &pages {
                assert!(size(page) <= 3900, "{}", size(page));
                assert!(balanced(page), "{page}");
            }
        }
    }

    #[test]
    fn code_block_survives_a_page_break() {
        let text = format!("```\n{}```\nпосле", "строка кода\n".repeat(1000));
        let pages = pages(&text, true);
        assert!(pages.len() > 1);
        assert!(pages[0].starts_with("<pre>") && pages[0].ends_with("</pre>"));
        assert!(pages[1].starts_with("<pre>"));
        assert!(pages.last().unwrap().ends_with("после"));
    }

    #[test]
    fn narrow_tables_align_and_wide_ones_become_cards() {
        let narrow = pages(
            "| Товар | Цена |\n|---|---:|\n| Чай | 100 |\n| Кофе | 2000 |",
            true,
        )
        .join("");
        assert!(
            narrow.contains("<pre>Товар  Цена\nЧай    100\nКофе   2000\n</pre>"),
            "{narrow}"
        );
        let wide = pages(
            "| Товар | Описание | Цена |\n|---|---|---|\n| Чай | **Очень** длинное описание товара для витрины | 100 |",
            true,
        )
        .join("");
        assert!(
            wide.contains("<b>Чай</b>\nОписание: <b>Очень</b> длинное"),
            "{wide}"
        );
        assert!(wide.contains("Цена: 100"));
        assert!(!wide.contains("---"));
    }

    #[test]
    fn empty_page_uses_the_requested_chat_language() {
        assert_eq!(pages_for(crate::i18n::Lang::Ru, "", true), vec!["(пусто)"]);
        assert_eq!(pages_for(crate::i18n::Lang::En, "", true), vec!["(empty)"]);
    }

    #[test]
    fn plain_files_keep_their_look_and_empty_ones_say_so() {
        assert_eq!(pages("a,b\n1,*2*", false), vec!["<pre>a,b\n1,*2*\n</pre>"]);
        assert_eq!(pages("", true), vec!["(пусто)"]);
    }

    #[test]
    fn a_section_starts_on_a_new_page_when_this_one_is_mostly_full() {
        let text = format!("{}# Раздел\nтекст", "строка текста\n".repeat(160));
        let pages = pages(&text, true);
        assert_eq!(pages.len(), 2);
        assert!(pages[1].starts_with("<b>Раздел</b>"));
    }
}
