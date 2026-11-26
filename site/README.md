# The Abhoth website

A static site with no dependencies. `node site/build.mjs` builds `src/pages/**.html` and `public/` into
`dist/`. Every page starts with a `<!--meta {...} -->` JSON header. In page bodies, `{{base}}`,
`{{version}}` and `{{repo}}` are filled in.

```sh
SITE_BASE=/ node site/build.mjs          # a local preview at the server root (Git Bash: MSYS_NO_PATHCONV=1)
npx serve site/dist                      # or any static server
```

## Deploying

`.github/workflows/site.yml` builds on pushes to `main` that touch `site/`. When the repository
variable `PAGES_ENABLED` is `true`, it deploys to GitHub Pages (Settings > Pages, source "GitHub Actions").
Live at <https://madninjaskillz.github.io/abhoth/>.

## Moving to abhoth.co.uk

1. At the registrar, add these records:
   - apex `A` records `185.199.108.153`, `185.199.109.153`, `185.199.110.153` and `185.199.111.153`;
   - (optional) `AAAA` records `2606:50c0:8000::153` through `2606:50c0:8003::153`;
   - `www` as a `CNAME` to `madninjaskillz.github.io`.
2. Recommended: verify the domain at GitHub (Settings > Pages > Verified domains, under your account) with
   the TXT record `_github-pages-challenge-madninjaskillz.abhoth.co.uk` that GitHub shows.
3. Add `site/public/CNAME` containing `abhoth.co.uk`. The build then uses `/` and `https://abhoth.co.uk`
   for links, canonical URLs, the sitemap and OpenGraph.
4. In Settings > Pages > Custom domain, enter `abhoth.co.uk`. Once the certificate is issued, tick
   "Enforce HTTPS".
5. Point the legacy-domain redirects at the new domain: see "Old site redirects" in `CUTOVER.md`
   (one line per repo).
