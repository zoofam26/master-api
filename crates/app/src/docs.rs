//! Swagger UI pages (loaded from a CDN so no vendored assets are needed)
//! plus the human docs index.

use serde_json::Value;

const SWAGGER_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{TITLE} — master-api</title>
  <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
  <style>
    body { margin: 0; background: #fafafa; }
    #topbar { background: #1f2937; color: #fff; padding: 10px 16px; font-family: system-ui, sans-serif; }
    #topbar a { color: #93c5fd; text-decoration: none; }
  </style>
</head>
<body>
  <div id="topbar">
    <strong>{TITLE}</strong> — part of
    <a href="/docs">master-api</a> ·
    <a href="{SPEC_URL}">OpenAPI JSON</a> ·
    <a href="/">service catalogue</a>
  </div>
  <div id="swagger-ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js" crossorigin></script>
  <script>
    window.addEventListener('load', function () {
      window.ui = SwaggerUIBundle({
        url: '{SPEC_URL}',
        dom_id: '#swagger-ui',
        deepLinking: true,
        tryItOutEnabled: true,
        docExpansion: 'list',
        displayRequestDuration: true,
      });
    });
  </script>
</body>
</html>
"#;

const INDEX_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>master-api — API documentation</title>
  <style>
    body { font-family: system-ui, -apple-system, sans-serif; background: #f8fafc; color: #0f172a; margin: 0; }
    header { background: #1f2937; color: #fff; padding: 24px 32px; }
    header h1 { margin: 0 0 4px; font-size: 22px; }
    header p { margin: 0; color: #cbd5e1; }
    main { max-width: 860px; margin: 24px auto; padding: 0 16px; }
    .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 12px; }
    a.card { display: block; background: #fff; border: 1px solid #e2e8f0; border-radius: 10px; padding: 14px 16px; text-decoration: none; color: inherit; transition: border-color .15s; }
    a.card:hover { border-color: #3b82f6; }
    .card h2 { margin: 0 0 4px; font-size: 16px; color: #1d4ed8; }
    .card code { font-size: 12px; color: #64748b; }
    footer { max-width: 860px; margin: 24px auto; padding: 0 16px 32px; color: #64748b; font-size: 13px; }
  </style>
</head>
<body>
  <header>
    <h1>master-api</h1>
    <p>Eleven services, one binary — pick a service to open its Swagger UI.</p>
  </header>
  <main>
    <div class="grid">
      {CARDS}
    </div>
  </main>
  <footer>
    Machine-readable index: <a href="/openapi.json">/openapi.json</a> ·
    Service catalogue: <a href="/">/</a> ·
    Health: <a href="/health">/health</a>
  </footer>
</body>
</html>
"#;

/// The Swagger UI page for one service.
pub fn swagger_page(spec_url: &str, title: &str) -> String {
    SWAGGER_TEMPLATE
        .replace("{TITLE}", title)
        .replace("{SPEC_URL}", spec_url)
}

/// The docs index page listing every service.
pub fn index_page(services: &[Value]) -> String {
    let cards: String = services
        .iter()
        .map(|s| {
            format!(
                "<a class=\"card\" href=\"{url}\"><h2>{name}</h2><code>{url}</code></a>",
                url = s.get("url").and_then(|v| v.as_str()).unwrap_or("#"),
                name = s.get("name").and_then(|v| v.as_str()).unwrap_or("service"),
            )
        })
        .collect();
    INDEX_TEMPLATE.replace("{CARDS}", &cards)
}
