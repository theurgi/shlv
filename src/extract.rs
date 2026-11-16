//! The readable main text of a page (feature `fetch`): the article, docs, or README without
//! navigation and boilerplate, as input for summaries and LLM tags. It is used and dropped, never
//! stored.
//!
//! Hand-rolled and heuristic: a tolerant tree builder over [`crate::html`]'s tokenizer, then
//!
//! 1. boilerplate is pruned: `script`, `style`, `nav`, `header`, `footer`, `aside`, forms, hidden
//!    elements, landmark roles other than `main`, and elements whose class or id names a sidebar,
//!    menu, cookie banner, and the like;
//! 2. the root is `<main>`/`<article>`/`role=main` when one holds most of the text, else the
//!    element whose paragraphs score highest (much text, few links), else `<body>`;
//! 3. the root's text comes out as paragraphs, link-heavy short blocks are dropped, and the
//!    result is capped at [`MAX_WORDS`], cut at a paragraph boundary.

use crate::html::{self, Tag, tag_end};

/// Longest extract, in words: a few pages of text, enough for a summary or tags.
pub const MAX_WORDS: usize = 3000;

/// Below this many words the page has no usable text (an empty JavaScript shell, say).
pub const MIN_WORDS: usize = 20;

/// The page's main text: paragraphs separated by blank lines. Empty when the page has no usable
/// text; callers can tell that apart from a failed fetch, which never gets here.
pub fn extract(html: &str) -> String {
    let doc = Doc::parse(html);
    let root = doc.root();
    let mut blocks = Blocks::default();
    doc.render(root, &mut blocks, false, false);
    blocks.flush();
    let mut out = String::new();
    let mut words = 0;
    for b in blocks.done {
        // Link lists (related posts, tag clouds) inside the content.
        if b.text.len() < 200 && b.link_chars * 10 > b.text.len() * 6 {
            continue;
        }
        let n = b.text.split_whitespace().count();
        if words + n > MAX_WORDS {
            if words == 0 {
                // A single huge paragraph: cut it at a word boundary instead.
                out = b
                    .text
                    .split_whitespace()
                    .take(MAX_WORDS)
                    .collect::<Vec<_>>()
                    .join(" ");
                words = MAX_WORDS;
            }
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&b.text);
        words += n;
    }
    if words < MIN_WORDS {
        out.clear();
    }
    out
}

/// Elements whose content is not HTML: skipped to their end tag unparsed.
const RAW: &[&str] = &[
    "script",
    "style",
    "template",
    "textarea",
    "title",
    "noscript",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
];

/// Elements with no end tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr", "param", "keygen",
];

/// Boilerplate and non-text elements, dropped with everything inside.
const DROP: &[&str] = &[
    "head", "nav", "header", "footer", "aside", "form", "button", "select", "svg", "math",
    "canvas", "object", "embed", "dialog", "menu", "audio", "video", "picture", "map", "label",
    "input", "fieldset", "datalist", "output", "progress", "meter", "search",
];

/// ARIA roles of boilerplate landmarks and widgets.
const DROP_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "dialog",
    "alertdialog",
    "menu",
    "menubar",
    "toolbar",
    "tablist",
    "tooltip",
];

/// Words in a class or id (split on `-`, `_`, and spaces) that mark boilerplate.
const DROP_WORDS: &[&str] = &[
    "sidebar",
    "navbar",
    "nav",
    "navigation",
    "menu",
    "footer",
    "breadcrumb",
    "breadcrumbs",
    "cookie",
    "cookies",
    "consent",
    "share",
    "sharing",
    "social",
    "related",
    "advert",
    "advertisement",
    "ads",
    "banner",
    "popup",
    "modal",
    "skip",
    "toc",
    "editsection",
    "navbox",
    "catlinks",
    "noprint",
    "printfooter",
    "jump",
    "sitesub",
    "subscribe",
    "newsletter",
    "pagination",
    "sphinxsidebar",
    "ambox",
    "reflist",
    "references",
];

/// Elements that start a new paragraph.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "pre",
    "blockquote",
    "table",
    "tr",
    "figure",
    "figcaption",
    "details",
    "summary",
    "address",
    "center",
    "hgroup",
    "body",
    "html",
];

/// Elements whose text counts as a paragraph when scoring candidates.
const PARAGRAPH: &[&str] = &["p", "pre", "td", "blockquote", "dd", "li"];

struct Node<'a> {
    name: String,
    attrs: &'a str,
    parent: usize,
    children: Vec<Child<'a>>,
}

enum Child<'a> {
    El(usize),
    Text(&'a str),
}

struct Doc<'a> {
    nodes: Vec<Node<'a>>,
    /// Per node: pruned as boilerplate (itself, not counting ancestors).
    dropped: Vec<bool>,
}

impl<'a> Doc<'a> {
    fn parse(src: &'a str) -> Self {
        let mut nodes = vec![Node {
            name: "#root".into(),
            attrs: "",
            parent: 0,
            children: Vec::new(),
        }];
        let mut stack = vec![0usize];
        let mut rest = src;
        while let Some(lt) = rest.find('<') {
            let top = *stack.last().unwrap_or(&0);
            if lt > 0 {
                nodes[top].children.push(Child::Text(&rest[..lt]));
            }
            rest = &rest[lt..];
            if let Some(after) = rest.strip_prefix("<!--") {
                rest = after.find("-->").map_or("", |e| &after[e + 3..]);
                continue;
            }
            if rest.starts_with("<!") || rest.starts_with("<?") {
                rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
                continue;
            }
            // `<` not starting a tag is text.
            let starts_tag = rest[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '/');
            let end = match tag_end(rest) {
                Some(e) if starts_tag => e,
                _ => {
                    nodes[top].children.push(Child::Text("<"));
                    rest = &rest[1..];
                    continue;
                }
            };
            let tag = Tag::parse(&rest[1..end]);
            let self_closing = rest[..end].ends_with('/');
            rest = &rest[end + 1..];
            let name = tag.name.to_ascii_lowercase();
            if tag.closing {
                if let Some(pos) = stack.iter().rposition(|&i| nodes[i].name == name)
                    && pos > 0
                {
                    stack.truncate(pos);
                }
                continue;
            }
            close_implied(&nodes, &mut stack, &name);
            let parent = *stack.last().unwrap_or(&0);
            let id = nodes.len();
            nodes.push(Node {
                name: name.clone(),
                attrs: tag.attrs,
                parent,
                children: Vec::new(),
            });
            nodes[parent].children.push(Child::El(id));
            if RAW.contains(&name.as_str()) {
                // Skip to the end tag; keep nothing (none of these hold page text).
                let close = format!("</{name}");
                rest = html::find_ci(rest, &close)
                    .and_then(|i| rest[i..].find('>').map(|e| &rest[i + e + 1..]))
                    .unwrap_or("");
                continue;
            }
            if !self_closing && !VOID.contains(&name.as_str()) {
                stack.push(id);
            }
        }
        if !rest.is_empty() {
            let top = *stack.last().unwrap_or(&0);
            nodes[top].children.push(Child::Text(rest));
        }
        let dropped = nodes.iter().map(is_boilerplate).collect();
        Doc { nodes, dropped }
    }

    /// Whether node `i` or any ancestor was pruned.
    fn pruned(&self, mut i: usize) -> bool {
        loop {
            if self.dropped[i] {
                return true;
            }
            if i == 0 {
                return false;
            }
            i = self.nodes[i].parent;
        }
    }

    /// The element to take the text from.
    fn root(&self) -> usize {
        // Landmarks first: the narrowest one that still holds most of the landmark text.
        let landmarks: Vec<(usize, usize)> = (1..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                (n.name == "main"
                    || n.name == "article"
                    || attr_is(n.attrs, "role", "main")
                    || attr_is(n.attrs, "id", "main-content"))
                    && !self.pruned(i)
            })
            .map(|i| (i, self.text_len(i)))
            .filter(|&(_, len)| len >= 200)
            .collect();
        if let Some(&(_, best)) = landmarks.iter().max_by_key(|(_, len)| *len) {
            let deepest = landmarks
                .iter()
                .filter(|(_, len)| *len * 10 >= best * 6)
                .max_by_key(|(i, _)| self.depth(*i));
            if let Some(&(i, _)) = deepest {
                return i;
            }
        }
        self.best_scored().unwrap_or_else(|| self.body())
    }

    /// Readability-style scoring: each paragraph adds to its parent's score and half to its
    /// grandparent's; the highest score wins.
    fn best_scored(&self) -> Option<usize> {
        let mut score = vec![0f64; self.nodes.len()];
        for i in 1..self.nodes.len() {
            if !PARAGRAPH.contains(&self.nodes[i].name.as_str()) || self.pruned(i) {
                continue;
            }
            let (text, links) = self.text_and_links(i);
            let plain = text.saturating_sub(links);
            if plain < 25 {
                continue;
            }
            let s = 1.0 + (plain as f64 / 100.0).min(3.0);
            let parent = self.nodes[i].parent;
            score[parent] += s;
            score[self.nodes[parent].parent] += s / 2.0;
        }
        score[0] = 0.0;
        let (i, s) = score.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        (*s > 0.0).then_some(i)
    }

    fn body(&self) -> usize {
        (1..self.nodes.len())
            .find(|&i| self.nodes[i].name == "body")
            .unwrap_or(0)
    }

    fn depth(&self, mut i: usize) -> usize {
        let mut d = 0;
        while i != 0 {
            i = self.nodes[i].parent;
            d += 1;
        }
        d
    }

    /// Characters of unpruned text under `i`, not counting link text.
    fn text_len(&self, i: usize) -> usize {
        let (t, l) = self.text_and_links(i);
        t.saturating_sub(l)
    }

    /// Characters of unpruned text under `i`, and how many of them are inside links.
    fn text_and_links(&self, i: usize) -> (usize, usize) {
        let mut acc = (0, 0);
        self.count(i, false, &mut acc);
        acc
    }

    fn count(&self, i: usize, in_link: bool, acc: &mut (usize, usize)) {
        let in_link = in_link || self.nodes[i].name == "a";
        for c in &self.nodes[i].children {
            match c {
                Child::Text(t) => {
                    let n = t.split_whitespace().map(str::len).sum::<usize>();
                    acc.0 += n;
                    if in_link {
                        acc.1 += n;
                    }
                }
                Child::El(j) if !self.dropped[*j] => self.count(*j, in_link, acc),
                Child::El(_) => {}
            }
        }
    }

    fn render(&self, i: usize, out: &mut Blocks, in_link: bool, in_pre: bool) {
        let name = self.nodes[i].name.as_str();
        // Inside <pre>, only the <pre> itself is a block.
        let block = BLOCK.contains(&name) && !in_pre;
        if block {
            out.flush();
        }
        if name == "li" {
            out.push("- ", false);
        }
        let in_link = in_link || name == "a";
        let pre = in_pre || name == "pre";
        for c in &self.nodes[i].children {
            match c {
                Child::Text(t) if pre => out.push_pre(&html::decode(t)),
                Child::Text(t) => out.push(&html::decode(t), in_link),
                Child::El(j) if self.dropped[*j] => {}
                Child::El(j) => match self.nodes[*j].name.as_str() {
                    "br" => out.line_break(),
                    "td" | "th" => {
                        out.push(" ", false);
                        self.render(*j, out, in_link, pre);
                        out.push(" ", false);
                    }
                    _ => self.render(*j, out, in_link, pre),
                },
            }
        }
        if block {
            out.flush();
        }
    }
}

/// Closes elements that the opening of `name` ends implicitly: a `<p>` ends an open `<p>`, a
/// `<li>` an open `<li>` in the same list, and so on.
fn close_implied(nodes: &[Node], stack: &mut Vec<usize>, name: &str) {
    let (closes, scope): (&[&str], &[&str]) = match name {
        "li" => (&["li"], &["ul", "ol", "menu"]),
        "dt" | "dd" => (&["dt", "dd"], &["dl"]),
        "tr" => (&["tr", "td", "th"], &["table", "tbody", "thead", "tfoot"]),
        "td" | "th" => (&["td", "th"], &["tr", "table"]),
        "option" => (&["option"], &["select", "datalist"]),
        n if n == "p" || BLOCK.contains(&n) => {
            if stack.last().is_some_and(|&i| nodes[i].name == "p") {
                stack.pop();
            }
            return;
        }
        _ => return,
    };
    for pos in (1..stack.len()).rev() {
        let n = nodes[stack[pos]].name.as_str();
        if scope.contains(&n) {
            return;
        }
        if closes.contains(&n) {
            stack.truncate(pos);
            return;
        }
    }
}

fn is_boilerplate(n: &Node) -> bool {
    if DROP.contains(&n.name.as_str()) || RAW.contains(&n.name.as_str()) {
        return true;
    }
    if n.attrs.is_empty() {
        return false;
    }
    let tag = Tag {
        name: &n.name,
        closing: false,
        attrs: n.attrs,
    };
    if tag.attr("hidden").is_some()
        || tag.attr("aria-hidden").is_some_and(|v| v == "true")
        || tag.attr("style").is_some_and(|s| {
            let s: String = s
                .split_whitespace()
                .collect::<String>()
                .to_ascii_lowercase();
            s.contains("display:none") || s.contains("visibility:hidden")
        })
    {
        return true;
    }
    if let Some(role) = tag.attr("role")
        && DROP_ROLES.contains(&role.trim().to_ascii_lowercase().as_str())
    {
        return true;
    }
    // Citation markers, as in Wikipedia's `[21]`. (Not a general class word: Sphinx marks
    // ordinary links `reference internal`.)
    if n.name == "sup" && tag.attr("class").is_some_and(|c| c.contains("reference")) {
        return true;
    }
    // Never prune the content landmarks by class.
    if n.name == "main" || n.name == "article" || n.name == "body" {
        return false;
    }
    ["class", "id"].iter().any(|a| {
        tag.attr(a).is_some_and(|v| {
            v.to_ascii_lowercase()
                .split([' ', '-', '_'])
                .any(|w| DROP_WORDS.contains(&w))
        })
    })
}

fn attr_is(attrs: &str, name: &str, value: &str) -> bool {
    let tag = Tag {
        name: "",
        closing: false,
        attrs,
    };
    tag.attr(name)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
}

/// One paragraph of output text.
struct Block {
    text: String,
    link_chars: usize,
}

/// Builds paragraphs from a stream of text, collapsing whitespace.
#[derive(Default)]
struct Blocks {
    cur: String,
    link_chars: usize,
    /// Whether whitespace was seen since the last word.
    space: bool,
    done: Vec<Block>,
}

impl Blocks {
    fn push(&mut self, text: &str, in_link: bool) {
        for (i, word) in text.split(char::is_whitespace).enumerate() {
            if i > 0 {
                self.space = true;
            }
            if word.is_empty() {
                continue;
            }
            if self.space && !self.cur.is_empty() && !self.cur.ends_with(['\n', ' ']) {
                self.cur.push(' ');
            }
            self.space = false;
            self.cur.push_str(word);
            if in_link {
                self.link_chars += word.len();
            }
        }
    }

    /// Text inside `<pre>`: line breaks kept.
    fn push_pre(&mut self, text: &str) {
        self.cur.push_str(text);
    }

    fn line_break(&mut self) {
        let t = self.cur.trim_end_matches(' ').len();
        self.cur.truncate(t);
        if !self.cur.is_empty() && !self.cur.ends_with('\n') {
            self.cur.push('\n');
        }
        self.space = false;
    }

    fn flush(&mut self) {
        let text = self
            .cur
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim_matches(|c: char| c.is_whitespace());
        // A list marker with nothing after it is not a paragraph.
        if !text.is_empty() && text != "-" {
            self.done.push(Block {
                text: text.to_string(),
                link_chars: self.link_chars,
            });
        }
        self.cur.clear();
        self.link_chars = 0;
        self.space = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn prefers_main_and_drops_boilerplate() {
        let body = words(40);
        let html = format!(
            "<html><head><title>T</title><style>p{{}}</style></head><body>\
             <nav><a href=/>Home</a> <a href=/x>Menu item</a></nav>\
             <header>Site name</header>\
             <div class=\"cookie-banner\">We use cookies</div>\
             <main><h1>Title &amp; more</h1><p>{body}</p><p hidden>secret</p>\
             <p style=\"display: none\">gone</p><script>var x = '<p>no</p>';</script>\
             <ul><li>one<li>two</ul></main>\
             <aside>Related posts</aside><footer>© 2026</footer></body></html>"
        );
        let text = extract(&html);
        assert_eq!(text, format!("Title & more\n\n{body}\n\n- one\n\n- two"));
    }

    #[test]
    fn scores_blocks_without_landmarks() {
        let p = |n| format!("<p>{}, and more, words.</p>", words(n));
        let html = format!(
            "<body><div id=top><a href=/a>A link</a> <a href=/b>Another link</a></div>\
             <div class=content>{}{}{}</div><div>short</div></body>",
            p(30),
            p(30),
            p(30)
        );
        let text = extract(&html);
        assert_eq!(text.matches("\n\n").count(), 2, "{text}");
        assert!(
            !text.contains("A link") && !text.contains("short"),
            "{text}"
        );
    }

    #[test]
    fn keeps_pre_and_line_breaks() {
        let html = format!(
            "<article><p>{}</p><pre>fn main() {{\n    println!(\"&lt;hi&gt;\");\n}}</pre>\
             <p>line one<br>line two</p></article>",
            words(30)
        );
        let text = extract(&html);
        assert!(
            text.contains("fn main() {\n    println!(\"<hi>\");\n}"),
            "{text}"
        );
        assert!(text.ends_with("line one\nline two"), "{text}");
    }

    #[test]
    fn caps_at_a_paragraph_boundary() {
        let para = format!("<p>{}</p>", words(1000));
        let text = extract(&format!("<main>{}</main>", para.repeat(4)));
        assert_eq!(text.split_whitespace().count(), 3000);
        assert_eq!(text.matches("\n\n").count(), 2);
        // One paragraph longer than the cap is cut at a word.
        let text = extract(&format!("<main><p>{}</p></main>", words(5000)));
        assert_eq!(text.split_whitespace().count(), MAX_WORDS);
    }

    #[test]
    fn empty_when_no_usable_text() {
        assert_eq!(extract(""), "");
        assert_eq!(
            extract(
                "<html><body><noscript>You need JavaScript</noscript>\
                 <div id=root></div><script src=app.js></script></body></html>"
            ),
            ""
        );
        assert_eq!(extract("<p>Too short to count.</p>"), "");
    }

    #[test]
    fn tolerates_broken_markup() {
        let text = extract(&format!(
            "<div><p>{} < 3 and a > b <b>bold</i></p></span><p>{}",
            words(15),
            words(10)
        ));
        assert!(text.contains("w14 < 3 and a > b bold"), "{text}");
        assert!(text.ends_with("w9"), "{text}");
    }
}
