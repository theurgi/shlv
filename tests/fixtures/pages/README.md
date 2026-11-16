# Page fixtures

Real pages saved on 2026-11-16 for the main-text extraction tests (`tests/extract.rs`, task 017).
They are trimmed copies: the bodies of `<script>`, `<style>`, and `<svg>` elements and very long
`data-*` attributes were removed. Everything else is as served.

| File                  | Kind                           | Source                                                                                                  | License                          |
| --------------------- | ------------------------------ | ------------------------------------------------------------------------------------------------------- | -------------------------------- |
| `rust-blog.html`      | blog post                      | https://blog.rust-lang.org/2024/11/28/Rust-1.83.0/                                                      | MIT or Apache-2.0                |
| `rust-book.html`      | docs page with a sidebar       | https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html                                           | MIT or Apache-2.0                |
| `github-readme.html`  | README on GitHub               | https://github.com/BurntSushi/ripgrep                                                                   | README: MIT or Unlicense         |
| `wikinews.html`       | news article                   | https://en.wikinews.org/wiki/Monta_publishes_its_2025_EV_charger_ranking,_including_hardware_performance | CC BY 2.5                        |
| `wikipedia.html`      | encyclopedia article           | https://en.wikipedia.org/wiki/Social_bookmarking                                                        | CC BY-SA 4.0                     |
| `wikipedia-talk.html` | discussion thread (forum-like) | https://en.wikipedia.org/wiki/Talk:Social_bookmarking                                                   | CC BY-SA 4.0                     |
| `mdn.html`            | reference docs                 | https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/article                            | CC BY-SA 2.5                     |
| `python-docs.html`    | Sphinx docs                    | https://docs.python.org/3/library/sqlite3.html                                                          | PSF License (docs)               |
| `sqlite-fts5.html`    | long single-page docs          | https://www.sqlite.org/fts5.html                                                                        | public domain                    |
| `serde-docsrs.html`   | rustdoc on docs.rs             | https://docs.rs/serde/latest/serde/                                                                     | MIT or Apache-2.0                |
| `excalidraw.html`     | JavaScript app shell, no text  | https://excalidraw.com/                                                                                 | MIT                              |

Site chrome (GitHub's, docs.rs's, Wikimedia's navigation) is in the copies as served; it is what
the tests check gets removed. Stack Exchange pages (CC BY-SA) would make a better forum fixture
but refuse automated downloads.
