import { test, expect } from '@playwright/test';
import { openTab, startHopd, stopHopd } from '../helpers';

test('ranking: votes merge into Sum edges and decay is an event', async ({ browser }) => {
  const hopd = await startHopd('hoprt/hop/ranking.hop', 19110, 19111);
  try {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();
    await openTab(a, hopd.url);
    await openTab(b, hopd.url);

    await expect(a.locator('#vote-count')).toContainText('0 votes');
    await a.locator('#vote-ripgrep-grep').click();
    await expect(a.locator('#vote-count')).toContainText('1 votes');
    await expect(b.locator('#vote-count')).toContainText('1 votes');
    await expect(a.locator('#edges li')).toContainText('ripgrep>grep');
    await expect(b.locator('#edges li')).toContainText('ripgrep>grep');

    await a.locator('#vote-ripgrep-grep').click();
    await expect(a.locator('#edges li')).toContainText('ripgrep>grep  2');
    await expect(b.locator('#edges li')).toContainText('ripgrep>grep  2');

    await a.locator('#decay').click();
    await expect(a.locator('#edges li')).toContainText('ripgrep>grep  1.8');
    await expect(b.locator('#edges li')).toContainText('ripgrep>grep  1.8');

    await ctxA.close();
    await ctxB.close();
  } finally {
    await stopHopd(hopd);
  }
});
