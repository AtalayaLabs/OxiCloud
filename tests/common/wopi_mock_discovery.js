#!/usr/bin/env node
// Minimal mock WOPI discovery server for the Hurl WOPI suite.
//
// Serves a valid RFC-shaped discovery XML on `GET /discovery.xml` so
// `OXICLOUD_WOPI_DISCOVERY_URL=http://127.0.0.1:<port>/discovery.xml`
// resolves to a real editor URL when `/api/wopi/editor-url` fetches it.
//
// The `urlsrc` we hand back points at a black-hole host so no real
// editor process needs to be running — the Hurl suite only asserts on
// OxiCloud's own responses (token contents, HTTP status codes,
// headers). The mock exists purely to let `get_editor_url` succeed
// end-to-end so we can exercise the mint-time authz path (Viewer-
// clicks-Edit gets a read-only token).
//
// Node stdlib only — matches the tooling used by tests/oidc/fake_idp
// (both are stdlib-free apart from `node-oidc-provider` on that side).
// No package.json, no npm install, no extra dependency for the api
// test suite. Started + reaped by `tests/api/run.sh`. Port comes from
// `WOPI_MOCK_PORT` env var (default 9100).

'use strict';

const http = require('http');

const DISCOVERY_XML = `<?xml version="1.0" encoding="utf-8"?>
<wopi-discovery>
  <net-zone name="external-http">
    <!-- text/plain lets the txt files the Hurl suite uploads round-trip. -->
    <app name="text/plain" favIconUrl="http://mock-editor.invalid/favicon.ico">
      <action name="edit" ext="txt" urlsrc="http://mock-editor.invalid/edit?"/>
      <action name="view" ext="txt" urlsrc="http://mock-editor.invalid/view?"/>
    </app>
    <!-- One office extension so tests can also exercise the docx path
         if they need to. -->
    <app name="application/vnd.openxmlformats-officedocument.wordprocessingml.document">
      <action name="edit" ext="docx" urlsrc="http://mock-editor.invalid/edit?"/>
      <action name="view" ext="docx" urlsrc="http://mock-editor.invalid/view?"/>
    </app>
    <!-- OnlyOffice-shaped legacy binary doc: view + convert (no edit). -->
    <app name="application/msword">
      <action name="view" ext="doc" default="true" urlsrc="http://mock-editor.invalid/view?"/>
      <action name="convert" ext="doc" targetext="docx" requires="update" urlsrc="http://mock-editor.invalid/convert?"/>
    </app>
    <!-- Collabora-shaped edit-only xlsx: edit (no view). -->
    <app name="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet">
      <action name="edit" ext="xlsx" default="true" urlsrc="http://mock-editor.invalid/edit?"/>
    </app>
  </net-zone>
  <proof-key oldvalue="" oldmodulus="" oldexponent=""
             value="" modulus="" exponent=""/>
</wopi-discovery>
`;

const port = Number(process.env.WOPI_MOCK_PORT || 9100);

const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/discovery.xml') {
    res.writeHead(200, {
      'Content-Type': 'application/xml; charset=utf-8',
      'Content-Length': Buffer.byteLength(DISCOVERY_XML),
    });
    res.end(DISCOVERY_XML);
    return;
  }
  res.writeHead(404);
  res.end();
});

// SIGTERM from `kill` in run.sh cleanup — exit quietly so the test
// runner's tail-of-log stays clean.
for (const sig of ['SIGTERM', 'SIGINT']) {
  process.on(sig, () => server.close(() => process.exit(0)));
}

server.listen(port, '127.0.0.1', () => {
  console.log(`wopi-mock-discovery listening on 127.0.0.1:${port}`);
});
