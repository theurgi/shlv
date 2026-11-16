# Page titles

With the `fetch` feature, `add --fetch` downloads the page and takes its title (`<title>`, else
`og:title`) unless `--title` is given. A fetch that fails (offline, timeout, HTTP error, not HTML)
prints a warning and still adds the bookmark. `shlv fetch` fills in titles afterwards, a few
requests at a time (`-j`), and exits nonzero if any failed. Requests time out after 15 s, follow at
most 5 redirects, read at most 2 MiB, and send `User-Agent: shlv/VERSION`. HTTPS trusts the system's
CA bundle (`$SSL_CERT_FILE`, else the usual system paths), so a proxy CA the system trusts works;
without one, shlv uses its bundled Mozilla roots.
