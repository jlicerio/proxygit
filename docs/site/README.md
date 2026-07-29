# ProxyGit share site

Static landing page with mermaid architecture / network diagrams.

## Local preview

```bash
# from repo root
python3 -m http.server 8765 --directory docs/site
# open http://127.0.0.1:8765/
```

## GitHub Pages

Source: `docs/site` (or deploy `docs/site` as Pages root via Actions / Settings).

After the repo exists:

```bash
# Settings → Pages → Deploy from branch → /docs
# or use the included GitHub Action path below once pushed
```

Update repo URLs in `index.html` if the GitHub owner/name differs from `jlicerio/proxygit`.
