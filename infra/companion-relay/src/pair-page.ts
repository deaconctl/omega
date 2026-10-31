// The page behind Lathe's "Copy link". Android's Messages app doesn't make
// `lathe-companion://` links tappable, and Chrome won't open one typed into
// its address bar, but both open an https link. The pairing details travel in
// the fragment, which browsers never send to the server, so the token stays
// out of every log; the page rebuilds the app link from it.

const APP_SCHEME = "lathe-companion";
const ANDROID_PACKAGE = "com.lathe.companion";

const HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pair with Lathe</title>
<style>
  :root { color-scheme: light dark; --bg: #f6f6f4; --fg: #1d1d1b; --muted: #6b6b66; --accent: #2f6fdf; --card: #ffffff; }
  @media (prefers-color-scheme: dark) { :root { --bg: #151515; --fg: #ececea; --muted: #9a9a95; --accent: #6c9cff; --card: #1f1f1f; } }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--fg);
         font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
  main { width: min(26rem, calc(100% - 32px)); background: var(--card); border-radius: 16px; padding: 28px 24px; text-align: center;
         box-shadow: 0 1px 3px rgba(0, 0, 0, 0.12); }
  h1 { font-size: 1.25rem; margin: 0 0 8px; }
  p { margin: 0 0 20px; color: var(--muted); }
  a.button { display: block; padding: 14px; border-radius: 12px; background: var(--accent); color: #fff; text-decoration: none; font-weight: 600; }
  [hidden] { display: none !important; }
</style>
</head>
<body>
<main>
  <section id="open" hidden>
    <h1>Pair with Lathe</h1>
    <p>Open Lathe Companion to pair this phone, then allow it in Lathe.</p>
    <a class="button" id="link" href="#">Open Lathe Companion</a>
  </section>
  <section id="invalid" hidden>
    <h1>This pairing link is incomplete</h1>
    <p>Copy the link again from the Lathe Companion dialog in Lathe.</p>
  </section>
  <p id="hint" hidden>Nothing happened? Install the Lathe Companion app first. Pairing links expire after 5 minutes.</p>
</main>
<script>
  (function () {
    var query = location.hash.slice(1);
    // Only a pairing link of the expected shape becomes an app link.
    var valid = /^v=1&/.test(query) && /(^|&)desktop_id=/.test(query) && /(^|&)token=/.test(query) && !/[#\\s<>"']/.test(query);
    if (!valid) {
      document.getElementById("invalid").hidden = false;
      return;
    }
    var android = /Android/i.test(navigator.userAgent);
    var href = android
      ? "intent://pair?" + query + "#Intent;scheme=${APP_SCHEME};package=${ANDROID_PACKAGE};end"
      : "${APP_SCHEME}://pair?" + query;
    document.getElementById("link").href = href;
    document.getElementById("open").hidden = false;
    document.getElementById("hint").hidden = false;
    // Browsers that allow it switch to the app right away; the button covers
    // the ones that need a tap first.
    location.href = href;
  })();
</script>
</body>
</html>
`;

export function pairPage(): Response {
  return new Response(HTML, {
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "no-store",
      "Referrer-Policy": "no-referrer",
      "X-Content-Type-Options": "nosniff",
      "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    },
  });
}
