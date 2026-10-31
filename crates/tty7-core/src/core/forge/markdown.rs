//! Making an issue's Markdown safe to hand to the text view.
//!
//! Issue bodies and comments are written by anyone on the internet. The text
//! view renders Markdown natively (no browser, no script), so there is nothing
//! to execute — but two things in it act on the reader's machine:
//!
//! - **Images load themselves.** An `![](…)` or `<img src>` is fetched the
//!   moment it is drawn, which tells whoever controls that URL that this
//!   issue was opened, from which IP, and when. Only images GitHub itself
//!   hosts (a screenshot pasted into an issue, see [`is_github_hosted`]) are
//!   drawn — reading the issue already told GitHub as much. Any other image
//!   becomes a plain link: the reader can still open it, deliberately, in
//!   the browser. (github.com proxies third-party images through its own
//!   servers for the same reason; tty7 has no proxy, so it does not load them.)
//! - **Links open whatever they name.** A click hands the URL to the OS, and
//!   `file:///…` or an app's custom scheme can launch programs. Only `http`,
//!   `https` and `mailto` targets survive; anything else is pointed at `#`.
//!
//! This is a rewrite of the *source text*, not a Markdown parser. It knows
//! enough structure to leave code alone (fenced blocks and inline code spans
//! are copied verbatim, since `![x](y)` inside backticks is text) and to find
//! the link and image forms Markdown and the HTML subset actually have.

/// Rewrite `src` as described in the module docs. `image_label` names an image
/// whose alt text is empty ("image"), in the reader's language.
pub fn sanitize(src: &str, image_label: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut fence: Option<(char, usize)> = None;
    for line in src.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = fence_marker(trimmed).filter(|_| indent <= 3);
        match (fence, marker) {
            (None, Some(m)) => {
                fence = Some(m);
                out.push_str(line);
                continue;
            }
            (Some((ch, n)), Some((mch, mn))) if ch == mch && mn >= n && closes_fence(trimmed) => {
                fence = None;
                out.push_str(line);
                continue;
            }
            (Some(_), _) => {
                out.push_str(line);
                continue;
            }
            (None, None) => {}
        }
        out.push_str(&sanitize_line(line, image_label));
    }
    out
}

/// A fence opener/closer: three or more of one of `` ` `` / `~`.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let ch = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = line.chars().take_while(|c| *c == ch).count();
    (n >= 3).then_some((ch, n))
}

/// A closing fence carries nothing after its marker but whitespace.
fn closes_fence(line: &str) -> bool {
    let ch = line.chars().next().unwrap_or(' ');
    line.trim_start_matches(ch).trim().is_empty()
}

/// One line outside a fenced block: code spans kept, the rest rewritten.
fn sanitize_line(line: &str, image_label: &str) -> String {
    // A reference definition, `[id]: url`, is a link target of its own.
    if let Some(rewritten) = reference_definition(line) {
        return rewritten;
    }
    // A table row cannot be split across paragraphs without ending the table.
    let own_block = !line.trim_start().starts_with('|');
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        match rest.find('`') {
            Some(start) => {
                out.push_str(&sanitize_text(&rest[..start], image_label, own_block));
                let after = &rest[start..];
                let ticks = after.chars().take_while(|c| *c == '`').count();
                let closer = "`".repeat(ticks);
                match after[ticks..].find(&closer) {
                    Some(end) => {
                        let span = ticks + end + ticks;
                        out.push_str(&after[..span]);
                        rest = &after[span..];
                    }
                    // An unclosed run of backticks is literal text.
                    None => {
                        out.push_str(&after[..ticks]);
                        rest = &after[ticks..];
                    }
                }
            }
            None => {
                out.push_str(&sanitize_text(rest, image_label, own_block));
                break;
            }
        }
    }
    out
}

fn reference_definition(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 || !trimmed.starts_with('[') {
        return None;
    }
    let close = trimmed.find("]:")?;
    if trimmed[1..close].contains(']') {
        return None;
    }
    let head_len = line.len() - trimmed.len() + close + 2;
    let tail = &line[head_len..];
    let target_start = tail.len() - tail.trim_start().len();
    let target = tail[target_start..].split_whitespace().next()?;
    let bare = target.trim_start_matches('<').trim_end_matches('>');
    if is_safe_target(bare) {
        return None;
    }
    let from = head_len + target_start;
    Some(format!(
        "{}#{}",
        &line[..from],
        &line[from + target.len()..]
    ))
}

/// Plain text between code spans. `own_block` puts each image kept as an
/// image in a paragraph of its own: the text view draws an image that shares
/// a paragraph with text at line height, a screenshot shrunk to an icon.
fn sanitize_text(text: &str, image_label: &str, own_block: bool) -> String {
    let image = |alt: &str, url: &str| match own_block {
        true => format!("\n\n![{alt}]({url})\n\n"),
        false => format!("![{alt}]({url})"),
    };
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        // A GitHub-hosted `![alt](url)` stays an image.
        if rest.starts_with("![")
            && !escaped(bytes, i)
            && let Some((alt, url, len)) = inline_image(rest)
            && is_github_hosted(url)
        {
            out.push_str(&image(alt, &escape_destination(url)));
            i += len;
            continue;
        }
        // Any other `![alt](…)` / `![alt][ref]` → a link with the same target.
        if rest.starts_with("![") && !escaped(bytes, i) {
            out.push('[');
            if rest[2..].starts_with(']') {
                out.push_str(image_label);
            }
            i += 2;
            continue;
        }
        // `](target` — the target half of an inline link or image.
        if rest.starts_with("](") {
            out.push_str("](");
            i += 2;
            let target = &text[i..];
            let lead = target.len() - target.trim_start().len();
            let body = &target[lead..];
            let (url, len) = if let Some(inner) = body.strip_prefix('<') {
                let end = inner.find('>').unwrap_or(inner.len());
                (&inner[..end], end + 2)
            } else {
                let end = body
                    .find(|c: char| c.is_whitespace() || c == ')')
                    .unwrap_or(body.len());
                (&body[..end], end)
            };
            out.push_str(&target[..lead]);
            if is_safe_target(url) {
                // Copied, not scanned, so nothing in it may read as markup
                // of its own: were the parser to end the link elsewhere
                // (an unbalanced `(`, a title left open), a `![…](…)` or a
                // tag inside the URL would come alive unsanitized.
                let pointy = body.starts_with('<');
                if pointy {
                    out.push('<');
                }
                out.push_str(&neutralize_markup(url));
                if pointy && len <= body.len() && body[..len].ends_with('>') {
                    out.push('>');
                }
            } else {
                out.push('#');
            }
            i += lead + len.min(body.len());
            continue;
        }
        if rest.starts_with('<') {
            // `<img …>` → a link to what it would have loaded.
            if starts_with_tag(rest, "img") || starts_with_tag(rest, "image") {
                let end = rest.find('>').map_or(rest.len(), |e| e + 1);
                let tag = &rest[..end];
                let src = attr(tag, "src").unwrap_or_default();
                let alt = attr(tag, "alt").filter(|a| !a.trim().is_empty());
                let label = alt.as_deref().unwrap_or(image_label);
                let label = label.replace(['[', ']'], "");
                if is_github_hosted(&src) {
                    out.push_str(&image(&label, &escape_destination(&src)));
                } else if is_safe_target(&src) && !src.is_empty() {
                    out.push_str(&format!("[{label}]({})", escape_destination(&src)));
                } else {
                    out.push_str(&format!("[{label}](#)"));
                }
                i += end;
                continue;
            }
            // `<scheme:…>` autolink with a scheme we do not open.
            if let Some(end) = rest.find('>') {
                let inner = &rest[1..end];
                if !inner.contains(char::is_whitespace)
                    && scheme(inner).is_some()
                    && !is_safe_target(inner)
                {
                    out.push_str("\\<");
                    i += 1;
                    continue;
                }
            }
            // Any other tag: neutralise `href`/`src` values it carries.
            if let Some(end) = tag_end(rest) {
                out.push_str(&rewrite_tag_urls(&rest[..end]));
                i += end;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn escaped(bytes: &[u8], i: usize) -> bool {
    let mut n = 0;
    let mut j = i;
    while j > 0 && bytes[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

fn starts_with_tag(s: &str, name: &str) -> bool {
    let Some(rest) = s.strip_prefix('<') else {
        return false;
    };
    // `get`, not a slice: `name.len()` can land inside a multibyte char.
    rest.get(..name.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(name))
        && rest[name.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
}

/// The length of an HTML tag starting at `s[0] == '<'`, when it is one.
fn tag_end(s: &str) -> Option<usize> {
    let rest = s.strip_prefix('<')?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if !rest.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    // Stop at the next `<`: a stray `<` in prose is not a tag.
    let end = s.find('>')?;
    (!s[1..end].contains('<')).then_some(end + 1)
}

fn rewrite_tag_urls(tag: &str) -> String {
    let mut tag = tag.to_string();
    for name in ["href", "src", "srcset", "poster", "background"] {
        if let Some(value) = attr(&tag, name)
            && !is_safe_target(&value)
        {
            tag = tag.replacen(&value, "#", 1);
        }
    }
    tag
}

/// An attribute's value, quoted or bare. Case-insensitive on the name.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(name) {
        let at = from + pos;
        from = at + name.len();
        let before_ok = lower[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_whitespace() || c == '/');
        let after = lower[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let offset = tag.len() - tag[from..].trim_start().len() + 1;
        let value = tag[offset..].trim_start();
        return Some(match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or("").to_string(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string(),
        });
    }
    None
}

/// The scheme of `url`, when it has one: letters first, then letters, digits,
/// `+`, `-`, `.`, up to a `:` that comes before any `/`, `?` or `#`.
fn scheme(url: &str) -> Option<&str> {
    let colon = url.find(':')?;
    let head = &url[..colon];
    if head.is_empty()
        || url[..colon].contains(['/', '?', '#'])
        || !head.chars().next()?.is_ascii_alphabetic()
        || !head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(head)
}

/// Point each pasted attachment at a URL it can be downloaded from.
///
/// An image pasted into an issue is written into the Markdown as
/// `https://github.com/user-attachments/assets/<uuid>`. On a public repository
/// that URL answers anyone; on a private one it wants a browser session, and
/// an API token does not count. The rendered HTML GitHub returns alongside
/// (`body_html`, from the `full` media type) carries the same image as a
/// `private-user-images.githubusercontent.com/…<uuid>…?jwt=…` URL that
/// downloads without credentials for a few minutes. Swap each attachment for
/// its signed twin by the uuid both carry; one with no twin is left alone.
pub fn sign_attachments(markdown: &str, html: &str) -> String {
    const PREFIX: &str = "https://github.com/user-attachments/assets/";
    if html.is_empty() || !markdown.contains(PREFIX) {
        return markdown.to_string();
    }
    let signed: Vec<String> = html
        .split(['"', '\''])
        .filter(|v| v.starts_with("https://private-user-images.githubusercontent.com/"))
        .map(|v| v.replace("&amp;", "&"))
        .collect();
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at]);
        let tail = &rest[at + PREFIX.len()..];
        let id_len = tail
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .unwrap_or(tail.len());
        let id = &tail[..id_len];
        match signed
            .iter()
            .find(|url| id.len() >= 32 && url.split('?').next().is_some_and(|p| p.contains(id)))
        {
            Some(url) => out.push_str(url),
            None => out.push_str(&rest[at..at + PREFIX.len() + id_len]),
        }
        rest = &tail[id_len..];
    }
    out.push_str(rest);
    out
}

/// `![alt](url)` or `![alt](url "title")` at the start of `s`: the alt text,
/// the URL, and how many bytes the whole form takes. `None` for anything
/// else, including the reference form `![alt][id]`.
fn inline_image(s: &str) -> Option<(&str, &str, usize)> {
    let rest = s.strip_prefix("![")?;
    let alt_end = rest.find(']')?;
    let alt = &rest[..alt_end];
    if alt.contains('[') {
        return None;
    }
    let target = rest[alt_end + 1..].strip_prefix('(')?;
    let close = target.find(')')?;
    let inside = target[..close].trim();
    let url = inside.split_whitespace().next()?;
    let url = url
        .strip_prefix('<')
        .and_then(|u| u.strip_suffix('>'))
        .unwrap_or(url);
    Some((alt, url, 2 + alt_end + 1 + 1 + close + 1))
}

/// Whether an image URL is one GitHub serves itself: attachments pasted into
/// an issue (`github.com/user-attachments/…`, a repository's `/assets/…`) and
/// anything under `githubusercontent.com` (older attachments, raw files,
/// avatars, GitHub's own image proxy). Only `https`.
pub fn is_github_hosted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // The authority ends at the first of these for a URL parser (`\\` counts
    // as `/` in an `https` URL), so `https://evil.io?.githubusercontent.com`
    // is evil.io.
    let (host, path) = rest.split_at(rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len()));
    // Only plain DNS characters: userinfo, a port, an entity or a percent
    // escape would each make the "host" above something else.
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return false;
    }
    let host = host.to_ascii_lowercase();
    if host == "github.com" {
        let mut parts = path.trim_start_matches('/').split('/');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some("user-attachments"), Some(_), _) => true,
            (Some(_), Some(_), Some("assets")) => true,
            _ => false,
        };
    }
    host.ends_with(".githubusercontent.com")
}

/// Whether a link target may be handed to the OS opener.
///
/// Relative targets and fragments carry no scheme and so cannot name a program;
/// they are kept. Whitespace and control characters are stripped first, the
/// way a browser does, so ` jav\tascript:` is judged as `javascript:`.
///
/// Character references are decoded before judging too — both the Markdown
/// and the HTML parser decode them in a URL, so `file&#58;///x` is `file:///x`
/// by the time it is opened. An `&` still standing before the path, after
/// that, is a reference this decoder does not know, and is not guessed at.
pub fn is_safe_target(url: &str) -> bool {
    let judge = |url: &str| {
        let cleaned: String = url
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .collect();
        let head = &cleaned[..cleaned.find(['/', '?', '#']).unwrap_or(cleaned.len())];
        if head.contains('&') {
            return false;
        }
        match scheme(&cleaned) {
            None => !cleaned.contains(':') || cleaned.starts_with(['/', '#', '?', '.']),
            Some(s) => matches!(s.to_ascii_lowercase().as_str(), "http" | "https" | "mailto"),
        }
    };
    judge(&decode_char_refs(url))
}

/// Decode the character references a parser would decode in a URL: numeric
/// ones (with or without the `;`, as HTML allows) and the few named ones that
/// spell URL syntax or whitespace. Anything else is left as written.
fn decode_char_refs(s: &str) -> String {
    const NAMED: [(&str, char); 10] = [
        ("colon", ':'),
        ("Tab", '\t'),
        ("NewLine", '\n'),
        ("sol", '/'),
        ("quest", '?'),
        ("num", '#'),
        ("period", '.'),
        ("amp", '&'),
        ("lpar", '('),
        ("rpar", ')'),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        if let Some(num) = tail.strip_prefix('#') {
            let (hex, digits) = match num.strip_prefix(['x', 'X']) {
                Some(h) => (true, h),
                None => (false, num),
            };
            let n = digits
                .find(|c: char| {
                    !(if hex {
                        c.is_ascii_hexdigit()
                    } else {
                        c.is_ascii_digit()
                    })
                })
                .unwrap_or(digits.len());
            if n > 0 {
                let ch = u32::from_str_radix(&digits[..n], if hex { 16 } else { 10 })
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{FFFD}');
                out.push(ch);
                let mut used = 1 + usize::from(hex) + n;
                if tail[used..].starts_with(';') {
                    used += 1;
                }
                rest = &tail[used..];
                continue;
            }
        } else {
            let n = tail
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(tail.len());
            if tail[n..].starts_with(';')
                && let Some((_, ch)) = NAMED.iter().find(|(name, _)| *name == &tail[..n])
            {
                out.push(*ch);
                rest = &tail[n + 1..];
                continue;
            }
        }
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `url` percent-encoded where Markdown would read it as syntax — the link's
/// own end, a nested image or link, a tag, an escape, a code span — for
/// writing as an inline link's destination.
fn escape_destination(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            ' ' | '(' | ')' | '<' | '>' | '[' | ']' | '\\' | '`' | '"' | '\'' => {
                out.push_str(&format!("%{:02X}", c as u32));
            }
            c if c.is_whitespace() || c.is_control() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// [`escape_destination`]'s lighter cousin for a destination copied from the
/// source as written: only what could open markup of its own is encoded, so
/// a URL with balanced parentheses (`…/Foo_(bar)`) still reads the same.
fn neutralize_markup(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            '<' | '>' | '[' | ']' => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Whether a fragment of raw HTML, as the Markdown parser hands it over, is
/// one [`sanitize`] could have produced: no `<img>` (every one is rewritten
/// into Markdown, and the HTML parser reads `<image>` as `<img>`), and every
/// `href`/`src` a target [`is_safe_target`] allows.
///
/// For checking what the parser actually saw, after the fact — the rewrite is
/// line-based, and a construct it reads differently from the parser (a code
/// span that is really inside a tag, a fence the parser does not accept)
/// would otherwise slip through.
pub fn html_is_safe(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if lower.contains("<img") || lower.contains("<image") {
        return false;
    }
    for name in ["href", "src"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(name) {
            let at = from + pos;
            from = at + name.len();
            let after = &html[from..];
            let Some(value) = after.trim_start().strip_prefix('=') else {
                continue;
            };
            let value = value.trim_start();
            let value = match value.chars().next() {
                Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
                _ => value
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or(""),
            };
            if !is_safe_target(value) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(src: &str) -> String {
        sanitize(src, "image")
    }

    #[test]
    fn a_multibyte_char_after_lt_is_prose() {
        assert!(s("a <日本 b").contains("日本"));
        assert!(!starts_with_tag("<日本", "img"));
        assert!(starts_with_tag("<IMG src=x>", "img"));
    }

    #[test]
    fn github_hosted_images_stay_images() {
        let pasted = "https://github.com/user-attachments/assets/352d14e0-d11c";
        // Each in a paragraph of its own, so it is drawn at its size.
        assert_eq!(
            s(&format!("![shot]({pasted})")),
            format!("\n\n![shot]({pasted})\n\n")
        );
        assert_eq!(
            s(&format!("a ![](<{pasted}> \"t\") b")),
            format!("a \n\n![]({pasted})\n\n b")
        );
        assert_eq!(
            s(
                r#"<img width="300" alt="Screenshot" src="https://user-images.githubusercontent.com/1/a.png">"#
            ),
            "\n\n![Screenshot](https://user-images.githubusercontent.com/1/a.png)\n\n"
        );
        assert_eq!(
            s(&format!(r#"<img src="{pasted}" />"#)),
            format!("\n\n![image]({pasted})\n\n")
        );
        // In a table row the image stays in its cell.
        assert_eq!(
            s(&format!("| ![x]({pasted}) |")),
            format!("| ![x]({pasted}) |")
        );
    }

    #[test]
    fn only_github_itself_counts_as_github_hosted() {
        for yes in [
            "https://github.com/user-attachments/assets/x",
            "https://github.com/owner/repo/assets/1/x",
            "https://private-user-images.githubusercontent.com/1/x.png?jwt=y",
            "https://camo.githubusercontent.com/abc",
            "https://RAW.githubusercontent.com/o/r/main/a.png",
        ] {
            assert!(is_github_hosted(yes), "{yes}");
        }
        for no in [
            "http://github.com/user-attachments/assets/x",
            "https://github.com/owner/repo",
            "https://github.com.evil.io/user-attachments/assets/x",
            "https://evil.io/x.githubusercontent.com/a.png",
            "https://githubusercontent.com.evil.io/a.png",
            "https://github.com@evil.io/user-attachments/assets/x",
            "https://x.io/a.png",
            "",
        ] {
            assert!(!is_github_hosted(no), "{no}");
        }
    }

    #[test]
    fn images_become_links_to_the_same_target() {
        assert_eq!(
            s("see ![shot](https://x.io/a.png) here"),
            "see [shot](https://x.io/a.png) here"
        );
        assert_eq!(s("![](https://x.io/a.png)"), "[image](https://x.io/a.png)");
        assert_eq!(s("![ref style][1]"), "[ref style][1]");
        // An escaped bang is text, and stays text.
        assert_eq!(s(r"\![not](x)"), r"\![not](x)");
    }

    #[test]
    fn html_images_become_links_too() {
        assert_eq!(
            s(r#"<img width="300" alt="Screenshot" src="https://x.io/u/a.png">"#),
            "[Screenshot](https://x.io/u/a.png)"
        );
        assert_eq!(
            s("<IMG SRC='https://x.io/b.png' />"),
            "[image](https://x.io/b.png)"
        );
        assert_eq!(s(r#"<img src="file:///etc/passwd">"#), "[image](#)");
    }

    #[test]
    fn links_to_anything_but_the_web_are_disarmed() {
        assert_eq!(s("[ok](https://github.com)"), "[ok](https://github.com)");
        assert_eq!(s("[mail](mailto:a@b.c)"), "[mail](mailto:a@b.c)");
        assert_eq!(s("[rel](docs/a.md)"), "[rel](docs/a.md)");
        assert_eq!(s("[frag](#heading)"), "[frag](#heading)");
        assert_eq!(s("[x](file:///Applications/Calc.app)"), "[x](#)");
        assert_eq!(s("[x](javascript:alert(1))"), "[x](#))");
        assert_eq!(s("[x](<vscode://open?x=1> \"t\")"), "[x](# \"t\")");
        assert_eq!(s("[x]( ssh://host )"), "[x]( # )");
        assert_eq!(s("<a href=\"file:///x\">x</a>"), "<a href=\"#\">x</a>");
        assert_eq!(
            s("<a href=\"https://ok.io\">x</a>"),
            "<a href=\"https://ok.io\">x</a>"
        );
        assert_eq!(s("<smb://share/x>"), "\\<smb://share/x>");
        assert_eq!(s("<https://ok.io>"), "<https://ok.io>");
        assert_eq!(s("[r]: file:///x \"t\"\n"), "[r]: # \"t\"\n");
        assert_eq!(s("[r]: https://ok.io\n"), "[r]: https://ok.io\n");
    }

    #[test]
    fn code_is_left_exactly_as_written() {
        let fenced = "```md\n![x](http://a/b.png)\n<img src=\"x\">\n```\n![y](http://c/d.png)\n";
        assert_eq!(
            s(fenced),
            "```md\n![x](http://a/b.png)\n<img src=\"x\">\n```\n[y](http://c/d.png)\n"
        );
        assert_eq!(
            s("use `![a](b)` for images, ![c](http://d)"),
            "use `![a](b)` for images, [c](http://d)"
        );
        assert_eq!(
            s("~~~~\n[x](file:///y)\n~~~~\n"),
            "~~~~\n[x](file:///y)\n~~~~\n"
        );
    }

    #[test]
    fn prose_with_angle_brackets_is_untouched() {
        assert_eq!(s("a < b and c > d"), "a < b and c > d");
        assert_eq!(s("Vec<String> is fine"), "Vec<String> is fine");
        assert_eq!(s("unicode ✓ → ok"), "unicode ✓ → ok");
    }

    #[test]
    fn safe_targets_are_judged_after_stripping_whitespace() {
        assert!(!is_safe_target(" jav\tascript:alert(1)"));
        assert!(!is_safe_target("FILE:///x"));
        assert!(is_safe_target("HTTPS://x.io"));
        assert!(is_safe_target("./a:b"));
        assert!(is_safe_target("/abs/path"));
    }

    #[test]
    fn a_host_is_what_a_url_parser_would_call_the_host() {
        for no in [
            "https://evil.io?.githubusercontent.com/a.png",
            "https://evil.io#.githubusercontent.com/a.png",
            "https://evil.io\\.githubusercontent.com/a.png",
            "https://evil.io&#47;.githubusercontent.com/a.png",
            "https://evil.io%2F.githubusercontent.com/a.png",
            "https://x.githubusercontent.com:8443/a.png",
        ] {
            assert!(!is_github_hosted(no), "{no}");
        }
        assert!(is_github_hosted(
            "https://camo.githubusercontent.com/abc?x=1#y"
        ));
    }

    #[test]
    fn an_image_url_cannot_smuggle_a_second_image() {
        // `)` in an attribute value would close the Markdown image early and
        // open a new one from the rest of the value.
        let out = s(r#"<img src="https://x.githubusercontent.com/a)![](https://evil.io/p.png">"#);
        assert!(!out.contains("![](https://evil"), "{out}");
        assert!(out.contains("/a%29!%5B%5D%28https://evil.io"), "{out}");
        // A kept link target cannot hide an image in itself either.
        let out = s("[x](https://ok.io/![b](https://evil.io/p.png) \"open title");
        assert!(!out.contains("![b]"), "{out}");
    }

    #[test]
    fn the_html_parsers_image_alias_is_an_image_too() {
        assert_eq!(
            s(r#"<image src="https://x.io/a.png">"#),
            "[image](https://x.io/a.png)"
        );
        assert_eq!(
            s("<IMAGE SRC=https://x.io/a.png>"),
            "[image](https://x.io/a.png)"
        );
    }

    #[test]
    fn character_references_do_not_disguise_a_scheme() {
        for no in [
            "file&#58;///System/Applications/Calculator.app",
            "file&#x3A;///x",
            "file&#x3a///x",
            "file&colon;///x",
            "jav&Tab;ascript:alert(1)",
            "&#x66;ile:///x",
            "&unknown;:x",
        ] {
            assert!(!is_safe_target(no), "{no}");
        }
        assert!(is_safe_target("https://x.io/?a=1&b=2"));
        assert!(is_safe_target("https://x.io/a&amp;b"));
        assert_eq!(
            s("[x](file&#58;///System/Applications/Calculator.app)"),
            "[x](#)"
        );
        assert_eq!(
            s(r#"<a href="file&#58;///x">x</a>"#),
            r##"<a href="#">x</a>"##
        );
    }

    #[test]
    fn html_left_standing_is_checked_as_the_parser_sees_it() {
        assert!(html_is_safe(r#"<a href="https://ok.io">"#));
        assert!(html_is_safe("<details><summary>x</summary>"));
        assert!(!html_is_safe(r#"<img src="https://x.io/a.png">"#));
        assert!(!html_is_safe(r#"<IMAGE src="https://x.io/a.png">"#));
        assert!(!html_is_safe(r#"<a href='file:///x'>"#));
        assert!(!html_is_safe(r#"<a/href=file:///x>"#));
        assert!(!html_is_safe(r#"<a href="file&#58;///x">"#));
    }

    #[test]
    fn attachments_are_swapped_for_their_signed_twins() {
        let id = "352d14e0-d11c-42db-b8b3-042b31e34ce7";
        let md = format!(
            "see <img alt=\"x\" src=\"https://github.com/user-attachments/assets/{id}\" /> and \
             https://github.com/user-attachments/assets/00000000-0000-0000-0000-000000000000"
        );
        let signed = format!(
            "https://private-user-images.githubusercontent.com/1/2-{id}.png?jwt=a.b&amp;x=1"
        );
        let html = format!(r#"<a href="{signed}"><img src="{signed}" alt="x"></a>"#);
        let out = sign_attachments(&md, &html);
        assert!(
            out.contains(&format!(
                "https://private-user-images.githubusercontent.com/1/2-{id}.png?jwt=a.b&x=1\""
            )),
            "{out}"
        );
        // No twin in the HTML: left as written.
        assert!(
            out.ends_with("assets/00000000-0000-0000-0000-000000000000"),
            "{out}"
        );
        // No HTML at all (a fixture, an old reply): nothing changes.
        assert_eq!(sign_attachments(&md, ""), md);
    }
}
