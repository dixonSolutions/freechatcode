// Record only new provider tabs containing this demo's unique workspace path.
// Existing tabs and the user's browser process are left open.
const { chromium } = require(process.env.FREECHATCODE_PLAYWRIGHT_PACKAGE || 'playwright-core');
const fs = require('node:fs');
const cp = require('node:child_process');
const path = require('node:path');

async function main() {
  const [endpoint, directory, host, workspace] = process.argv.slice(2);
  if (!workspace) throw new Error('Usage: capture_browser.cjs ENDPOINT DIRECTORY HOST WORKSPACE');
  fs.mkdirSync(directory, { recursive: true });
  const stopped = () => fs.existsSync(path.join(directory, 'stop'));
  const browser = await chromium.connectOverCDP(endpoint);
  const context = browser.contexts()[0];
  const workers = [];
  let serial = 0;
  const isTarget = page => {
    try {
      const hostname = new URL(page.url()).hostname;
      return hostname === host || hostname === `www.${host}`;
    } catch { return false; }
  };

  context.on('page', page => workers.push(record(page)));
  fs.writeFileSync(path.join(directory, 'ready'), 'ready');
  while (!stopped()) await new Promise(resolve => setTimeout(resolve, 100));
  const results = await Promise.allSettled(workers);
  const errors = results.filter(result => result.status === 'rejected').map(result => String(result.reason));
  if (errors.length) fs.writeFileSync(path.join(directory, 'errors.json'), JSON.stringify(errors));

  async function record(page) {
    await page.waitForLoadState('domcontentloaded').catch(() => {});
    // A unique fixture path identifies the harness's tab. Wait through its title
    // request, which does not yet include the workspace, before taking frames.
    let belongs = false;
    for (let attempt = 0; attempt < 1200 && !stopped() && !page.isClosed(); attempt++) {
      if (isTarget(page)) {
        const text = await page.locator('body').innerText().catch(() => '');
        if (text.includes(workspace)) { belongs = true; break; }
      }
      await page.waitForTimeout(100).catch(() => {});
    }
    if (!belongs || stopped()) return;
    await page.setViewportSize({ width: 960, height: 800 });
    const close = page.locator('[aria-label="Close sidebar"]').first();
    if (await close.isVisible().catch(() => false)) await close.click();
    const selector = host.includes('gemini') ? 'model-response' : '[role=main] .n6owBd, [role=main] .otQkpb, [role=main] .Y3BBE';
    const assistant = page.locator(selector);
    for (let attempt = 0; attempt < 1200 && !stopped(); attempt++) {
      if (await assistant.last().isVisible().catch(() => false)) break;
      await page.waitForTimeout(100).catch(() => {});
    }
    if (stopped() || !await assistant.last().isVisible().catch(() => false)) return;

    const file = path.join(directory, `page-${serial++}.mp4`);
    const encoder = cp.spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'image2pipe', '-vcodec', 'mjpeg', '-r', '10', '-i', '-', '-an', '-c:v', 'libx264', '-preset', 'ultrafast', '-crf', '22', '-pix_fmt', 'yuv420p', file], { stdio: ['pipe', 'ignore', 'inherit'] });
    const finished = new Promise((resolve, reject) => {
      encoder.once('error', reject);
      encoder.once('exit', code => code === 0 ? resolve() : reject(new Error(`ffmpeg exited ${code}`)));
    });
    // Handle encoder failure while screenshots are still being collected.
    finished.catch(() => {});
    encoder.stdin.on('error', () => {});
    const start = Date.now();
    let frames = 0;
    try {
      while (!stopped() && !page.isClosed() && encoder.exitCode === null) {
        const frame = await page.screenshot({ type: 'jpeg', quality: 85 });
        const due = Math.floor((Date.now() - start) / 100) + 1;
        while (frames < due) {
          if (!encoder.stdin.write(frame)) {
            await new Promise((resolve, reject) => {
              const drained = () => { encoder.stdin.removeListener('error', failed); resolve(); };
              const failed = error => { encoder.stdin.removeListener('drain', drained); reject(error); };
              encoder.stdin.once('drain', drained);
              encoder.stdin.once('error', failed);
            });
          }
          frames++;
        }
        await new Promise(resolve => setTimeout(resolve, 60));
      }
    } finally {
      encoder.stdin.end();
      await finished;
      fs.writeFileSync(`${file}.json`, JSON.stringify({ start, end: Date.now(), frames }));
    }
  }
}
main().then(() => process.exit(0)).catch(error => { console.error(error.message); process.exit(1); });
