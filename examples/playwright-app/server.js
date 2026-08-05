// Renders its response with a real browser.
//
// The greeting is produced by Chromium and read back out of the DOM, so the
// conformance content check only passes if the browser binary reached the
// runtime image and its system libraries are present. A plain string would
// pass whether or not the browser works.
const http = require('node:http');
const { chromium } = require('playwright');

let rendered = null;
let failure = null;

async function render() {
  // --no-sandbox because the sandbox cannot work in this image anyway: the
  // deploy COPY --chown strips the setuid bit from Chromium's helper, and the
  // default seccomp profile blocks the user-namespace clone the layer-1
  // sandbox needs. Know what it costs — an unsandboxed renderer means a
  // compromised page is code execution in the container. This page is a
  // hardcoded string, so there is nothing to compromise. An app that loads
  // untrusted URLs should run the browser in its own container, or supply a
  // Chromium-compatible seccomp profile, rather than copy this line.
  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  try {
    const page = await browser.newPage();
    await page.setContent('<h1>hello from autopack</h1>');
    return await page.textContent('h1');
  } finally {
    await browser.close();
  }
}

render()
  .then((text) => {
    rendered = text;
  })
  .catch((error) => {
    failure = error;
    console.error(`browser render failed: ${error.message}`);
  });

const server = http.createServer((_request, response) => {
  if (rendered) {
    response.writeHead(200, { 'content-type': 'text/plain' });
    response.end(`${rendered}\n`);
    return;
  }
  // 503 while the browser is still starting; the caller retries. A hard
  // failure stays a 503 so it surfaces as a failure rather than a hang.
  response.writeHead(503, { 'content-type': 'text/plain' });
  response.end(failure ? `browser unavailable: ${failure.message}\n` : 'starting\n');
});

server.listen(process.env.PORT || 3000, '0.0.0.0');

process.on('SIGTERM', () => server.close(() => process.exit(0)));
