# Sign in with Abhoth on the router's admin pages (D154)

The router's own admin pages (`/admin/`, D151) accept admin keys. When a router runs beside an
Abhoth web UI, its admins should be able to sign in with their web UI account instead. This is a
small OAuth2 authorization-code flow with PKCE (RFC 6749 4.1, RFC 7636 S256): the web UI is the
provider, each router a registered client. Shared paths and shapes: `orca_core::sso`.

## Flow

```text
browser            router (:8010)                       web UI
  | GET /admin/sso   |                                     |
  |----------------->| state, PKCE verifier kept here      |
  |<-- 302 + Lax binding cookie (10 min)                   |
  | GET /oauth/authorize?client_id&redirect_uri&state&code_challenge&S256
  |------------------------------------------------------->| sign in (password, 2FA) if needed
  |                                                        | consent (once per router and user)
  |<-- {redirect: <redirect_uri>?code&state} (code: 60 s, single use)
  | GET /admin/sso-callback?code&state (+ binding cookie)  |
  |----------------->| POST /api/oauth/token {client_id, client_secret, code,
  |                  |   redirect_uri, code_verifier}  -->|
  |                  |<-- {sub, username, admin}           |
  |<-- 200 page: admin session cookie, then ./ (the admin pages)
```

- **State and binding.** `state` is random and kept on the router with the verifier and the
  redirect URI (10 minutes, single use: a callback uses it up whatever happens next). A second
  random value goes into an HttpOnly `SameSite=Lax` cookie (Lax so it rides along on the web UI's
  top-level redirect back); only its hash is kept, and the callback must carry it, so a callback
  started in another browser (login CSRF) is refused.
- **Callback answer.** A small page that sets the usual session cookie (HttpOnly,
  `SameSite=Strict`, Secure over HTTPS, 8 h, 2 h idle) and refreshes to `./`. A redirect would
  end a cross-site chain and the Strict cookie would not be sent with it; the page's own
  navigation is same-site.
- **Paths.** `/admin/sso` and `/admin/sso-callback` sit beside `/admin/login`, so every cookie has
  the same default path (`/admin`) also behind a proxy's prefix.
- **Exchange.** Server to server: to `accounts_url` on a linked router (inside the container),
  else to the provider URL. The web UI takes the code out of its store first, then checks the
  client secret (hash, constant time), the grant type, expiry, the client and redirect URI the
  code was issued for, and the PKCE verifier. The answer is who signed in, not a token: the router
  makes its own session (`Principal::Key` with the `webui-sso` scope, label `webui:<username>`).
- **Admins only.** The consent page refuses a non-admin ("Only Abhoth admins can …") whatever
  they answer; no code is made. The router checks `admin` in the answer again.
- **No open redirects.** The authorize API answers an unknown client, a redirect URI that is not
  exactly one of the client's, a missing S256 challenge or a bad state with an error on the web
  UI's own page, never a redirect. Redirect URIs are absolute http(s) URLs without query,
  fragment, user info or dot segments that end in `/admin/sso-callback`.

## Clients

`sso_clients` (id, name, SHA-256 of the secret, redirect URIs) and `sso_consents` (client, user,
created, last used) in `abhoth.db` (migration 3). Codes and link requests are in memory.

- **Linked router** (accounts URL and link token): `POST /api/internal/sso/register` with the
  link token, `{name, redirect_uris}` → `{client_id, client_secret, redirect_uris}` (a new secret
  each time; id `router-<name>`). It registers when it has no credentials, and again (with the
  list plus the new one, at most 16) when it is opened at an address it may vouch for: an IP
  literal or `localhost` on a request no proxy passed on, or a host listed in
  `access.public_urls`. A Host header can't add anything else.
- **Other routers**: `POST /api/oauth/link-requests` (no credentials; HTTPS, loopback or the
  container network; 20 waiting, 3 per address, 15 minutes) → `{id, code, poll}`; an admin sees
  the code in Settings → Service keys → Router sign-in and approves (a client with its own random
  id, so a request can't take over a registered one) or denies; the poll collects the credentials
  once. `abhoth router sso link` does this and writes `<state_dir>/sso-client.json` (0600).
- **Secrets** never appear in logs, the audit or the terminal. The CLI prints the code to
  compare, not the secret.

## Limits and audit

- Router: 200 sign-ins waiting (20 per address); failed callbacks count toward the admin sign-in
  lockout (10 per address, 100 in all, per 15 minutes).
- Web UI: 500 codes waiting; 20 failed exchanges per address per 15 minutes.
- Web UI audit: `sso.client-registered`, `sso.link-requested|denied|collected|expired`,
  `sso.consent`, `sso.authorized`, `sso.denied`, `sso.refused`, `sso.sign-in`,
  `sso.exchange-refused`, `sso.consent-revoked`, `sso.client-removed`.
- Router audit: `admin.sign-in` (`webui:<username>`), `admin.sign-in-refused` (`ip:<addr>`, with
  why), `admin.sso-registered`.

## Not covered

- A session made this way lasts its 8 hours even if the user stops being an admin meanwhile (as a
  key session does until its key is revoked); signing out ends it.
- The app's in-dialog "Open router admin" goes through the web UI's signed proxy (the acting-user
  header) and needs none of this.
