// shotStr: DPR detection (layout metrics → device-metrics override → JS
// fallback) and the printed coordinate mapping.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { shotStr } from '../lib/tools/screenshot.mjs';
import { FakeCdp } from './helpers.mjs';

const PNG_BYTES = Buffer.from('fake-png-bytes');

function tmpOut(t) {
  const dir = mkdtempSync(join(tmpdir(), 'cdp-shot-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return join(dir, 'out.png');
}

test('writes the decoded screenshot file and reports DPR from layout metrics', async (t) => {
  const out = tmpOut(t);
  const cdp = new FakeCdp(async (method) => {
    switch (method) {
      case 'Page.getLayoutMetrics':
        // visualViewport is physical px, cssVisualViewport is CSS px → DPR 2.
        return { visualViewport: { clientWidth: 400 }, cssVisualViewport: { clientWidth: 200 } };
      case 'Emulation.getDeviceMetricsOverride': return {}; // no override set
      case 'Page.captureScreenshot': return { data: PNG_BYTES.toString('base64') };
      default: throw new Error('unexpected ' + method);
    }
  });

  const str = await shotStr(cdp, 'SID', out, 'AAAABBBBCCCC');
  assert.deepEqual(readFileSync(out), PNG_BYTES);

  assert.ok(str.startsWith(out + '\n'), 'first line is the output path');
  assert.ok(str.includes('Device pixel ratio (DPR): 2'));
  assert.ok(str.includes('divide by 2'));
  assert.ok(str.includes('screenshot point (200, 400) → CSS (100, 200)'));
  assert.ok(str.includes('CSS px = screenshot px / 2 ≈ screenshot px × 0.5'));
});

test('falls back to window.devicePixelRatio when metrics are unavailable', async (t) => {
  const out = tmpOut(t);
  const cdp = new FakeCdp(async (method, params) => {
    switch (method) {
      case 'Page.getLayoutMetrics': throw new Error('metrics unavailable');
      case 'Runtime.enable': return {};
      case 'Runtime.evaluate':
        if (params.expression === 'window.devicePixelRatio') return { result: { value: '1.25' } };
        throw new Error('unexpected eval: ' + params.expression);
      case 'Page.captureScreenshot': return { data: PNG_BYTES.toString('base64') };
      default: throw new Error('unexpected ' + method);
    }
  });

  const str = await shotStr(cdp, 'SID', out, 'T1');
  assert.ok(str.includes('Device pixel ratio (DPR): 1.25'));
  assert.ok(str.includes('screenshot point (125, 250) → CSS (100, 200)'));
  // Any non-1 DPR gets the inverse-scaling hint line.
  assert.ok(str.includes('On this 1.25x display: CSS px = screenshot px / 1.25 ≈ screenshot px × 0.8'));
});

test('DPR 1 output omits the scaling hint line', async (t) => {
  const out = tmpOut(t);
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Page.captureScreenshot') return { data: PNG_BYTES.toString('base64') };
    if (method === 'Page.getLayoutMetrics') return {};
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: '1' } };
    throw new Error('unexpected ' + method);
  });
  const str = await shotStr(cdp, 'SID', out, 'T1');
  assert.ok(str.includes('Device pixel ratio (DPR): 1'));
  assert.ok(!str.includes('On this'));
});
